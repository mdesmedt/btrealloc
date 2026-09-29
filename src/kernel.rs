use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{OsStr, c_int, c_ulong, c_void};
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::ptr;
use std::rc::Rc;
use std::vec;

use linux_raw_sys::btrfs::{
    BTRFS_EXTENT_DATA_KEY, BTRFS_EXTENT_DATA_REF_KEY, BTRFS_EXTENT_FLAG_DATA,
    BTRFS_EXTENT_ITEM_KEY, BTRFS_EXTENT_OWNER_REF_KEY, BTRFS_EXTENT_TREE_OBJECTID,
    BTRFS_FILE_EXTENT_INLINE, BTRFS_FIRST_FREE_OBJECTID, BTRFS_FS_INFO_FLAG_GENERATION,
    BTRFS_FS_TREE_OBJECTID, BTRFS_LOGICAL_INO_ARGS_IGNORE_OFFSET, BTRFS_ROOT_BACKREF_KEY,
    BTRFS_ROOT_ITEM_KEY, BTRFS_ROOT_TREE_OBJECTID, BTRFS_SHARED_DATA_REF_KEY,
    btrfs_extent_data_ref, btrfs_extent_item, btrfs_extent_owner_ref, btrfs_file_extent_item,
    btrfs_ioctl_fs_info_args, btrfs_ioctl_ino_lookup_args, btrfs_ioctl_ino_path_args,
    btrfs_ioctl_logical_ino_args, btrfs_ioctl_search_header, btrfs_ioctl_search_key,
    btrfs_root_item, btrfs_root_ref,
};
use linux_raw_sys::general::{
    __IncompleteArrayField, BTRFS_SUPER_MAGIC, FILE_DEDUPE_RANGE_SAME, FS_NOCOW_FL, O_TMPFILE,
    file_dedupe_range, file_dedupe_range_info, statfs,
};
use linux_raw_sys::ioctl::{
    BTRFS_IOC_FS_INFO, BTRFS_IOC_INO_LOOKUP, BTRFS_IOC_INO_PATHS, BTRFS_IOC_LOGICAL_INO_V2,
    BTRFS_IOC_TREE_SEARCH_V2, FIDEDUPERANGE, FS_IOC_GETFLAGS, FS_IOC_SETFLAGS,
};

use crate::extent::{Extent, ExtentRef};

/// The tree search buffer. Big enough that a search which comes back with room
/// to spare for the largest item btrfs can store is known to have reached the
/// end of its range, with no further call needed to find that out.
const BUF_SIZE: usize = 256 * 1024;

/// No btrfs item is larger than a tree node, and no tree node is larger than
/// 64 KiB.
const MAX_ITEM_SIZE: usize = 64 * 1024;

/// `statfs`'s flag for a read-only mount, as `<sys/statvfs.h>` has it.
const ST_RDONLY: u64 = 1;

/// A zeroed `Box<T>`, allocated straight on the heap: `Box::new(x)` builds `x`
/// on the stack first, which overflows it for something [`SearchArgs`]-sized.
fn boxed_zeroed<T>() -> Box<T> {
    let layout = std::alloc::Layout::new::<T>();
    unsafe {
        let ptr = std::alloc::alloc_zeroed(layout);
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Box::from_raw(ptr.cast::<T>())
    }
}

// btrfs, and this crate's LE type aliases, only ever run on little-endian hosts;
// every multi-byte field below is read in host order on that basis.
const _: () = assert!(cfg!(target_endian = "little"));

unsafe extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, arg: *mut c_void) -> c_int;
    fn fstatfs(fd: c_int, buf: *mut statfs) -> c_int;
    fn syncfs(fd: c_int) -> c_int;
}

/// `struct btrfs_ioctl_search_args_v2` with its trailing buffer given a fixed
/// size, which is the layout the ioctl expects.
#[repr(C)]
struct SearchArgs {
    key: btrfs_ioctl_search_key,
    buf_size: u64,
    buf: [u8; BUF_SIZE],
}

impl SearchArgs {
    /// A search over all of `tree_id`, allocated on the heap: the buffer is too
    /// big to build on the stack first.
    fn new(tree_id: u64) -> Box<SearchArgs> {
        let mut args: Box<SearchArgs> = boxed_zeroed();
        args.key.tree_id = tree_id;
        args.buf_size = BUF_SIZE as u64;
        args.set_range((0, 0, 0), (u64::MAX, u32::MAX, u64::MAX));
        args.key.max_transid = u64::MAX;
        args
    }

    /// Sets the keys searched between, both ends included, as
    /// `(objectid, type, offset)`. Keys order as tuples, so everything between
    /// the two in that order is found, whatever its type.
    fn set_range(&mut self, min: (u64, u32, u64), max: (u64, u32, u64)) {
        (
            self.key.min_objectid,
            self.key.min_type,
            self.key.min_offset,
        ) = min;
        (
            self.key.max_objectid,
            self.key.max_type,
            self.key.max_offset,
        ) = max;
    }

    /// Runs one search. `nr_items` is reset on the way in and holds the number
    /// of items returned on the way out.
    fn search(&mut self, fd: c_int) -> io::Result<()> {
        self.key.nr_items = u32::MAX;
        let rc = unsafe {
            ioctl(
                fd,
                BTRFS_IOC_TREE_SEARCH_V2 as c_ulong,
                (&raw mut *self).cast(),
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Hands every item the last search returned to `each` with its header.
    /// Returns the header of the last one, and whether the search reached the
    /// end of its range rather than stopping for want of room.
    fn items(
        &self,
        mut each: impl FnMut(&btrfs_ioctl_search_header, &[u8]) -> io::Result<()>,
    ) -> io::Result<(Option<btrfs_ioctl_search_header>, bool)> {
        const HEADER_SIZE: usize = size_of::<btrfs_ioctl_search_header>();
        let mut pos = 0usize;
        let mut last = None;
        for _ in 0..self.key.nr_items {
            let header =
                read_struct::<btrfs_ioctl_search_header>(&self.buf, pos).ok_or_else(|| {
                    io::Error::other("search claimed more items than its buffer holds")
                })?;
            let start = pos + HEADER_SIZE;
            let end = start + header.len as usize;
            if end > self.buf.len() {
                return Err(io::Error::other("search item runs past its buffer"));
            }
            each(&header, &self.buf[start..end])?;
            last = Some(header);
            pos = end;
        }
        // The kernel only stops short of the end of the range when the next
        // item does not fit. Room left for any item means it did not stop
        // short.
        Ok((last, BUF_SIZE - pos >= HEADER_SIZE + MAX_ITEM_SIZE))
    }

    /// Runs the search over its whole key range, however many calls that
    /// takes, handing every item found to `each` with its header.
    fn search_all(
        &mut self,
        fd: c_int,
        mut each: impl FnMut(&btrfs_ioctl_search_header, &[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        loop {
            self.search(fd)?;
            let (last, complete) = self.items(&mut each)?;
            let Some(last) = last else { return Ok(()) };
            if complete {
                return Ok(());
            }

            // Carry on from just past the last key returned. Keys order as
            // (objectid, type, offset) tuples, and so does the search range.
            self.key.min_objectid = last.objectid;
            self.key.min_type = last.type_;
            if last.offset < u64::MAX {
                self.key.min_offset = last.offset + 1;
            } else if last.type_ < u8::MAX as u32 {
                self.key.min_type = last.type_ + 1;
                self.key.min_offset = 0;
            } else if last.objectid < u64::MAX {
                self.key.min_objectid = last.objectid + 1;
                self.key.min_type = 0;
                self.key.min_offset = 0;
            } else {
                return Ok(());
            }
        }
    }
}

/// Whether an extent tree item is one [`Tally`] reads: the extent item, and
/// the backreferences stored after it.
fn is_extent_key(key_type: u32) -> bool {
    (BTRFS_EXTENT_ITEM_KEY..=BTRFS_SHARED_DATA_REF_KEY).contains(&key_type)
}

/// What an extent item says about its extent.
#[derive(Clone, Copy)]
struct ExtentItem {
    /// Bytes on disk, from the item's key.
    disk_bytes: u64,
    /// How many references the extent has in all.
    refs: u64,
    /// The transaction the extent was allocated in.
    generation: u64,
}

/// Who holds an extent, as the extent tree records it.
enum Backrefs {
    /// Every reference names the file holding it directly.
    Files(Vec<DataRef>),
    /// At least one reference goes through a shared metadata block, as a
    /// snapshot or a balance leaves behind, or has a shape not read here. Only
    /// a full backreference walk can say whose files those are.
    Opaque,
}

/// The extent tree's account of one extent, as it is read item by item.
#[derive(Default)]
struct Tally {
    /// The extent item, once it is found. It stays `None` for a tree block,
    /// which holds metadata rather than any file's data.
    item: Option<ExtentItem>,
    /// What the references read so far add up to.
    counted: u64,
    data_refs: Vec<DataRef>,
    /// Whether some reference does not name its file.
    opaque: bool,
}

impl Tally {
    fn add_item(&mut self, key_type: u32, key_offset: u64, item: &[u8]) {
        match key_type {
            BTRFS_EXTENT_ITEM_KEY => {
                let Some(extent) = read_struct::<btrfs_extent_item>(item, 0) else {
                    self.opaque = true;
                    return;
                };
                if extent.flags & BTRFS_EXTENT_FLAG_DATA as u64 == 0 {
                    return;
                }
                self.item = Some(ExtentItem {
                    disk_bytes: key_offset,
                    refs: extent.refs,
                    generation: extent.generation,
                });
                self.add_inline(&item[size_of::<btrfs_extent_item>()..]);
            }
            BTRFS_EXTENT_DATA_REF_KEY => match read_struct::<btrfs_extent_data_ref>(item, 0) {
                Some(r) => self.add_data_ref(&r),
                None => self.opaque = true,
            },
            // A shared data reference stored as its own item, a tree block's
            // references, or anything else in the backreference key range.
            _ => self.opaque = true,
        }
    }

    /// Reads the references stored inline in an extent item, after the item
    /// itself.
    fn add_inline(&mut self, mut bytes: &[u8]) {
        while let Some(&kind) = bytes.first() {
            // Each starts with its kind. What follows depends on it.
            let body = &bytes[1..];
            let len = match kind as u32 {
                BTRFS_EXTENT_DATA_REF_KEY => match read_struct::<btrfs_extent_data_ref>(body, 0) {
                    Some(r) => {
                        self.add_data_ref(&r);
                        size_of::<btrfs_extent_data_ref>()
                    }
                    None => break,
                },
                // Simple quotas record the subvolume charged for the extent. It
                // is not a reference.
                BTRFS_EXTENT_OWNER_REF_KEY => size_of::<btrfs_extent_owner_ref>(),
                // A shared data reference names the parent tree block, not a
                // file; anything else is not read here.
                _ => break,
            };
            match body.get(len..) {
                Some(rest) => bytes = rest,
                None => break,
            }
        }
        if !bytes.is_empty() {
            self.opaque = true;
        }
    }

    fn add_data_ref(&mut self, r: &btrfs_extent_data_ref) {
        self.counted += r.count as u64;
        self.data_refs.push(DataRef {
            root: r.root,
            inode: r.objectid,
            at: RefOffsets::Base(r.offset),
            count: r.count as u64,
        });
    }

    /// The extent item and who holds the extent, or `None` if this is not a
    /// data extent.
    fn finish(self) -> Option<(ExtentItem, Backrefs)> {
        let item = self.item?;
        let backrefs = if self.opaque || self.counted != item.refs {
            Backrefs::Opaque
        } else {
            Backrefs::Files(self.data_refs)
        };
        Some((item, backrefs))
    }
}

/// A data extent as a batch reads it from the extent tree, before it is
/// resolved to the file extent items pointing into it.
struct Unresolved {
    disk_address: u64,
    item: ExtentItem,
    backrefs: Backrefs,
}

/// `count` file extent items of `inode` in subvolume `root` point into the
/// extent, at file offsets `at` says where to find.
struct DataRef {
    root: u64,
    inode: u64,
    at: RefOffsets,
    count: u64,
}

/// Where in its file the items a [`DataRef`] stands for sit.
enum RefOffsets {
    /// As the extent tree records it: every item's file offset, less its own
    /// offset into the extent, is this. Worked out with wrapping arithmetic.
    Base(u64),
    /// As the backreference walk reports it: the items are the ones pointing
    /// into the extent at file offsets from the first to the last of these.
    Between(u64, u64),
}

/// One subvolume, found by path from the top-level one.
///
/// Only the path is kept. Its root directory is opened again wherever it is
/// needed: a descriptor held for every subvolume is what runs into
/// `RLIMIT_NOFILE` on a filesystem with thousands of snapshots.
struct Subvolume {
    path: PathBuf,
}

impl Subvolume {
    /// Opens the subvolume's root directory, which inode numbers in it are
    /// resolved through, or `None` if its path no longer leads there.
    fn open(&self, root: u64) -> io::Result<Option<File>> {
        open_subvolume(&self.path, root)
    }
}

/// Opens `path` as the root directory of subvolume `root`, or `None` if there
/// is nothing there, or it is something else: a directory, another subvolume,
/// or a mount on top of it.
fn open_subvolume(path: &Path, root: u64) -> io::Result<Option<File>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if own_root_id(file.as_raw_fd())? != root
        || file.metadata()?.ino() != BTRFS_FIRST_FREE_OBJECTID as u64
    {
        return Ok(None);
    }
    Ok(Some(file))
}

/// A handle on the whole filesystem, through the root directory of its
/// top-level subvolume. From there every other subvolume, and every inode in
/// it, can be reached.
pub struct Filesystem {
    file: File,
    /// Where the top-level subvolume is mounted.
    pub mount: PathBuf,
    /// The sector size: a file extent reference covers whole sectors, never
    /// part of one. It is the page size at mkfs time, typically 4096 bytes.
    pub sectorsize: u64,
    /// Whether it is mounted read-only, which leaves nothing to rewrite with.
    pub read_only: bool,
    /// One search buffer, reused for every search made through this handle.
    searchargs: RefCell<Box<SearchArgs>>,
    /// The buffer [`Filesystem::logical_ino`] hands the kernel, as `u64`s:
    /// `struct btrfs_data_container`'s four `u32`s fill the first two. It
    /// grows for the rare extent with more references than fit, and is kept
    /// at that size, but each lookup only offers the kernel as much of it as
    /// that lookup needs.
    logicalino: RefCell<Vec<u64>>,
    /// Every subvolume looked up so far, by tree id, or `None` for one there
    /// is no path to.
    subvolumes: RefCell<HashMap<u64, Option<Rc<Subvolume>>>>,
    /// Each subvolume's last snapshot, by tree id, as read for the current
    /// batch of the walk. Forgotten with every batch, so a snapshot taken
    /// mid-run is not missed.
    last_snapshots: RefCell<HashMap<u64, u64>>,
}

impl Filesystem {
    /// Opens the filesystem through `path`, which has to be where its
    /// top-level subvolume (subvolid=5) is mounted: only from there can every
    /// subvolume be reached. Anything else is refused with what to do instead.
    pub fn open(path: &Path) -> io::Result<Filesystem> {
        let file = File::open(path)?;

        let mut buf: statfs = unsafe { std::mem::zeroed() };
        if unsafe { fstatfs(file.as_raw_fd(), &raw mut buf) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if buf.f_type as u64 != BTRFS_SUPER_MAGIC as u64 {
            return Err(io::Error::other("not on a btrfs filesystem"));
        }

        // btrfs reports its sector size as the block size, so the statfs above
        // is all it takes to ask.
        let sectorsize = buf.f_bsize as u64;
        if !sectorsize.is_power_of_two() {
            return Err(io::Error::other(format!(
                "sector size {sectorsize} is not a power of two"
            )));
        }

        let meta = file.metadata()?;
        if !meta.is_dir() {
            return Err(io::Error::other(
                "not a directory: pass the mount point of the filesystem's top-level \
                 subvolume, mounted with -o subvolid=5",
            ));
        }
        let root = own_root_id(file.as_raw_fd())?;
        if root != BTRFS_FS_TREE_OBJECTID as u64 {
            return Err(io::Error::other(format!(
                "in subvolume {root}, not the top-level subvolume: mount the filesystem \
                 with -o subvolid=5 and pass that mount point"
            )));
        }
        if meta.ino() != BTRFS_FIRST_FREE_OBJECTID as u64 {
            return Err(io::Error::other(
                "a directory inside the top-level subvolume, not its root: pass the \
                 mount point itself",
            ));
        }

        Ok(Filesystem {
            file,
            mount: path.to_path_buf(),
            sectorsize,
            read_only: buf.f_flags as u64 & ST_RDONLY != 0,
            searchargs: RefCell::new(SearchArgs::new(0)),
            logicalino: RefCell::new(vec![0; LOGICAL_INO_START_SIZE / size_of::<u64>()]),
            subvolumes: RefCell::new(HashMap::new()),
            last_snapshots: RefCell::new(HashMap::new()),
        })
    }

    /// Every data extent on the filesystem, in address order, each with every
    /// reference to it. See [`ExtentWalk`].
    pub fn walk(self: &Rc<Self>) -> io::Result<ExtentWalk> {
        // Commit what is pending first, so that everything allocated from here
        // on, our own copies included, is newer than the generation read next.
        if unsafe { syncfs(self.file.as_raw_fd()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ExtentWalk {
            fs: Rc::clone(self),
            next: Some(0),
            generation: self.generation()?,
            batch: Vec::new().into_iter(),
        })
    }

    /// The filesystem's current generation: the transaction last committed,
    /// or the one running.
    fn generation(&self) -> io::Result<u64> {
        let mut args: btrfs_ioctl_fs_info_args = unsafe { std::mem::zeroed() };
        args.flags = BTRFS_FS_INFO_FLAG_GENERATION as u64;
        let rc = unsafe {
            ioctl(
                self.file.as_raw_fd(),
                BTRFS_IOC_FS_INFO as c_ulong,
                (&raw mut args).cast(),
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        // The kernel clears the flags it does not know.
        if args.flags & BTRFS_FS_INFO_FLAG_GENERATION as u64 == 0 {
            return Err(io::Error::other(
                "this kernel is too old to report the filesystem's generation",
            ));
        }
        Ok(args.generation)
    }

    /// The data extents from address `from` on, as many as one search returns,
    /// with the address the next batch starts from, or `None` once the extent
    /// tree is read to its end. Extents allocated after `generation` are left
    /// out.
    fn extent_batch(
        &self,
        from: u64,
        generation: u64,
    ) -> io::Result<(Vec<Unresolved>, Option<u64>)> {
        self.last_snapshots.borrow_mut().clear();

        let mut args = self.searchargs.borrow_mut();
        args.key.tree_id = BTRFS_EXTENT_TREE_OBJECTID as u64;
        args.set_range(
            (from, BTRFS_EXTENT_ITEM_KEY, 0),
            (u64::MAX, BTRFS_SHARED_DATA_REF_KEY, u64::MAX),
        );
        args.search(self.file.as_raw_fd())?;
        let mut tallies: Vec<(u64, Tally)> = Vec::new();
        let (last, complete) = args.items(|header, item| {
            if !is_extent_key(header.type_) {
                return Ok(());
            }
            match tallies.last_mut() {
                Some((address, tally)) if *address == header.objectid => {
                    tally.add_item(header.type_, header.offset, item);
                }
                _ => {
                    let mut tally = Tally::default();
                    tally.add_item(header.type_, header.offset, item);
                    tallies.push((header.objectid, tally));
                }
            }
            Ok(())
        })?;
        drop(args);

        let next = match last {
            // The search stopped for want of room, so the last extent it
            // reached may have references past the end of the buffer. It is
            // read again on its own, which also makes progress when a single
            // extent's references fill the whole buffer.
            Some(last) if !complete => {
                if tallies.last().is_some_and(|(a, _)| *a == last.objectid) {
                    tallies.pop();
                }
                tallies.push((last.objectid, self.tally(last.objectid)?));
                last.objectid.checked_add(1)
            }
            _ => None,
        };

        let batch = tallies
            .into_iter()
            .filter_map(|(address, tally)| {
                let (item, backrefs) = tally.finish()?;
                (item.generation <= generation).then_some(Unresolved {
                    disk_address: address,
                    item,
                    backrefs,
                })
            })
            .collect();
        Ok((batch, next))
    }

    /// Everything the extent tree holds about the extent at `address`.
    fn tally(&self, address: u64) -> io::Result<Tally> {
        let mut args = self.searchargs.borrow_mut();
        args.key.tree_id = BTRFS_EXTENT_TREE_OBJECTID as u64;
        args.set_range(
            (address, BTRFS_EXTENT_ITEM_KEY, 0),
            (address, BTRFS_SHARED_DATA_REF_KEY, u64::MAX),
        );
        let mut tally = Tally::default();
        args.search_all(self.file.as_raw_fd(), |header, item| {
            if is_extent_key(header.type_) {
                tally.add_item(header.type_, header.offset, item);
            }
            Ok(())
        })?;
        Ok(tally)
    }

    /// Every reference to one extent, given what the extent tree says about it.
    ///
    /// The extent tree records a reference once, against the tree block holding
    /// it. A block another tree shares since a snapshot is recorded as though
    /// only one subvolume held it, until one side writes to it. The extent tree
    /// alone is trusted only where every reference was read from a leaf written
    /// since its subvolume's last snapshot, which no other tree can share. For
    /// the rest, and wherever the extent tree's account does not add up, the
    /// kernel's backreference walk follows the shared blocks to every tree
    /// reaching the extent.
    fn resolve(
        &self,
        disk_address: u64,
        item: &ExtentItem,
        backrefs: Backrefs,
    ) -> io::Result<Vec<ExtentRef>> {
        if let Backrefs::Files(data_refs) = backrefs
            && let Ok(refs) = self.collect(disk_address, item.disk_bytes, &data_refs)
            && !self.may_be_shared(&refs)?
        {
            return Ok(refs);
        }
        self.collect(
            disk_address,
            item.disk_bytes,
            &self.logical_ino(disk_address)?,
        )
    }

    /// The extent at `extent.disk_address` again, resolved afresh by the
    /// kernel's backreference walk, for acting on.
    ///
    /// The walk read the extent from the extent tree, which lags behind: the
    /// references a rewrite moves are queued, and reach the extent tree only
    /// when the transaction commits. A rewrite of one extent can by then have
    /// changed who holds another the walk has already read, most of all where
    /// a snapshot shares the leaf both are in. The backreference walk takes
    /// what is queued into account.
    pub fn reresolve(&self, extent: &Extent) -> io::Result<Extent> {
        let data_refs = self.logical_ino(extent.disk_address)?;
        let refs = self.collect(extent.disk_address, extent.disk_bytes, &data_refs)?;
        Ok(Extent::from_refs(refs))
    }

    /// Whether any of `refs` was read from a leaf another tree may share: one
    /// no newer than its subvolume's last snapshot.
    fn may_be_shared(&self, refs: &[ExtentRef]) -> io::Result<bool> {
        for r in refs {
            if r.leaf_generation <= self.last_snapshot(r.root)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Every file extent item pointing into the extent at `disk_address`, read
    /// from the stretch of each file `data_refs` says holds them. Fails if what
    /// is there does not add up to what they said, which means the extent
    /// changed since they were read.
    fn collect(
        &self,
        disk_address: u64,
        disk_bytes: u64,
        data_refs: &[DataRef],
    ) -> io::Result<Vec<ExtentRef>> {
        let mut refs = Vec::new();
        for data_ref in data_refs {
            let found = refs.len();
            self.data_ref_extents(data_ref, disk_address, disk_bytes, &mut refs)?;
            if (refs.len() - found) as u64 != data_ref.count {
                return Err(io::Error::other("changed while it was being read"));
            }
        }
        if refs.is_empty() {
            return Err(io::Error::other("no longer referenced"));
        }
        Ok(refs)
    }

    /// The file extent items one [`DataRef`] stands for, searched for in only
    /// the stretch of its file that can hold them.
    fn data_ref_extents(
        &self,
        data_ref: &DataRef,
        disk_address: u64,
        disk_bytes: u64,
        refs: &mut Vec<ExtentRef>,
    ) -> io::Result<()> {
        let (start, last) = match data_ref.at {
            // Each item sits at the base plus its own offset into the extent,
            // which is less than the extent's uncompressed length: its length
            // on disk, unless it is compressed, and then no more than btrfs
            // compresses at once. The base wraps for an item placed earlier in
            // its file than it sits in the extent, and the stretch then starts
            // at the beginning of the file.
            RefOffsets::Base(base) => {
                let uncompressed = disk_bytes.max(MAX_COMPRESSED_BYTES);
                let last = base.wrapping_add(uncompressed - 1);
                (if last >= base { base } else { 0 }, last)
            }
            RefOffsets::Between(first, last) => (first, last),
        };

        let mut args = self.searchargs.borrow_mut();
        args.key.tree_id = data_ref.root;
        args.set_range(
            (data_ref.inode, BTRFS_EXTENT_DATA_KEY, start),
            (data_ref.inode, BTRFS_EXTENT_DATA_KEY, last),
        );

        search_extent_refs(
            &mut args,
            self.file.as_raw_fd(),
            data_ref.root,
            data_ref.inode,
            |r| {
                let belongs = match data_ref.at {
                    RefOffsets::Base(base) => r.file_offset.wrapping_sub(r.extent_offset) == base,
                    RefOffsets::Between(..) => true,
                };
                if r.disk_address == disk_address && r.disk_bytes == disk_bytes && belongs {
                    refs.push(r);
                }
            },
        )
    }

    /// The generation in which subvolume `root` was last snapshotted, or was
    /// last made from a snapshot, from its root item. A tree block no newer
    /// than this may be shared with another tree; one written since cannot be.
    fn last_snapshot(&self, root: u64) -> io::Result<u64> {
        if let Some(&generation) = self.last_snapshots.borrow().get(&root) {
            return Ok(generation);
        }
        const AT: usize = std::mem::offset_of!(btrfs_root_item, last_snapshot);
        let mut args = self.searchargs.borrow_mut();
        args.key.tree_id = BTRFS_ROOT_TREE_OBJECTID as u64;
        args.set_range(
            (root, BTRFS_ROOT_ITEM_KEY, 0),
            (root, BTRFS_ROOT_ITEM_KEY, u64::MAX),
        );
        let mut found = None;
        args.search_all(self.file.as_raw_fd(), |_, item| {
            found = read_struct::<u64>(item, AT);
            Ok(())
        })?;
        let generation =
            found.ok_or_else(|| io::Error::other(format!("subvolume {root} has no root item")))?;
        self.last_snapshots.borrow_mut().insert(root, generation);
        Ok(generation)
    }

    /// Every file extent item referencing the extent at `disk_address`, found
    /// by the kernel's backreference walk, which follows shared tree blocks up
    /// to the subvolumes holding them. Gathered into the same shape the extent
    /// tree gives when it names the files itself.
    fn logical_ino(&self, disk_address: u64) -> io::Result<Vec<DataRef>> {
        let mut buf = self.logicalino.borrow_mut();
        // Every lookup starts small, however large an earlier one grew the
        // buffer: the kernel zeroes and copies as much as it is told it has.
        let mut size = LOGICAL_INO_START_SIZE;
        loop {
            if buf.len() * size_of::<u64>() < size {
                buf.resize(size.div_ceil(size_of::<u64>()), 0);
            }
            let mut args = btrfs_ioctl_logical_ino_args {
                logical: disk_address,
                size: size as u64,
                reserved: [0; 3],
                // Without this flag the kernel only returns backreferences
                // whose own reference starts at `disk_address`, i.e. at the
                // very start of the extent. We want every reference into it,
                // wherever in the extent it starts.
                flags: BTRFS_LOGICAL_INO_ARGS_IGNORE_OFFSET as u64,
                inodes: buf.as_mut_ptr().cast::<c_void>() as u64,
            };
            let rc = unsafe {
                ioctl(
                    self.file.as_raw_fd(),
                    BTRFS_IOC_LOGICAL_INO_V2 as c_ulong,
                    (&raw mut args).cast(),
                )
            };
            if rc < 0 {
                return Err(io::Error::last_os_error());
            }

            // `bytes_left` and `bytes_missing`, then `elem_cnt` and
            // `elem_missed`, each pair little-endian in one `u64`.
            let bytes_missing = (buf[0] >> 32) as usize;
            if bytes_missing > 0 {
                size += bytes_missing;
                if size > LOGICAL_INO_MAX_SIZE {
                    return Err(io::Error::other(
                        "extent has more backreferences than fit in one lookup",
                    ));
                }
                continue;
            }

            // Each result is an (inode, offset, root) triple, one per file
            // extent item. With the offset ignored going in, the offset coming
            // out is the item's own file offset, so each file's items are
            // counted and bounded by the first and last of them.
            let elem_cnt = buf[1] as u32 as usize;
            #[allow(clippy::chunks_exact_to_as_chunks)]
            let mut triples: Vec<(u64, u64, u64)> = buf[2..]
                .chunks_exact(3)
                .take(elem_cnt / 3)
                .map(|t| (t[2], t[0], t[1]))
                .collect();
            triples.sort_unstable();

            let mut data_refs: Vec<DataRef> = Vec::new();
            for (root, inode, offset) in triples {
                match data_refs.last_mut() {
                    Some(DataRef {
                        root: r,
                        inode: i,
                        at: RefOffsets::Between(_, last),
                        count,
                    }) if (*r, *i) == (root, inode) => {
                        *last = offset;
                        *count += 1;
                    }
                    _ => data_refs.push(DataRef {
                        root,
                        inode,
                        at: RefOffsets::Between(offset, offset),
                        count: 1,
                    }),
                }
            }
            return Ok(data_refs);
        }
    }

    /// The subvolume with tree id `root`, or `None` if there is no path to it.
    fn subvolume(&self, root: u64) -> io::Result<Option<Rc<Subvolume>>> {
        if let Some(known) = self.subvolumes.borrow().get(&root) {
            return Ok(known.clone());
        }
        let found = self.find_subvolume(root)?.map(Rc::new);
        self.subvolumes.borrow_mut().insert(root, found.clone());
        Ok(found)
    }

    /// Finds the path to subvolume `root` the way `btrfs subvolume list` does:
    /// its root backreference names the subvolume it sits in, the directory
    /// there, and its own name in that directory. `None` for a subvolume with
    /// no backreference, which is one being deleted, or one whose path leads
    /// somewhere else, such as a mount on top of it.
    fn find_subvolume(&self, root: u64) -> io::Result<Option<Subvolume>> {
        let path = if root == BTRFS_FS_TREE_OBJECTID as u64 {
            self.mount.clone()
        } else {
            let Some((parent, dirid, name)) = self.root_backref(root)? else {
                return Ok(None);
            };
            let Some(parent_subvolume) = self.subvolume(parent)? else {
                return Ok(None);
            };
            let dir = ino_lookup(self.file.as_raw_fd(), parent, dirid)?;
            parent_subvolume.path.join(dir).join(name)
        };

        if open_subvolume(&path, root)?.is_none() {
            return Ok(None);
        }
        Ok(Some(Subvolume { path }))
    }

    /// Where subvolume `root` sits: the subvolume holding it, the directory
    /// there, and its name in that directory.
    fn root_backref(&self, root: u64) -> io::Result<Option<(u64, u64, PathBuf)>> {
        let mut args = self.searchargs.borrow_mut();
        args.key.tree_id = BTRFS_ROOT_TREE_OBJECTID as u64;
        args.set_range(
            (root, BTRFS_ROOT_BACKREF_KEY, 0),
            (root, BTRFS_ROOT_BACKREF_KEY, u64::MAX),
        );
        let mut found = None;
        args.search_all(self.file.as_raw_fd(), |header, item| {
            if found.is_none()
                && let Some(r) = read_struct::<btrfs_root_ref>(item, 0)
                && let Some(name) = item
                    .get(size_of::<btrfs_root_ref>()..)
                    .and_then(|rest| rest.get(..r.name_len as usize))
            {
                // The key's offset is the subvolume holding this one.
                found = Some((
                    header.offset,
                    r.dirid,
                    PathBuf::from(OsStr::from_bytes(name)),
                ));
            }
            Ok(())
        })?;
        Ok(found)
    }

    /// The absolute path of inode `inode` in subvolume `root`, or `None` if the
    /// subvolume cannot be reached, or `inode` has no name any more (an orphan,
    /// unlinked but still open elsewhere).
    ///
    /// An inode can have more than one name — hardlinks — in which case this
    /// takes the first the kernel returns. Any of them opens the same data,
    /// which is all a rewrite needs.
    pub fn inode_path(&self, root: u64, inode: u64) -> io::Result<Option<PathBuf>> {
        let Some(subvolume) = self.subvolume(root)? else {
            return Ok(None);
        };
        let Some(subvolume_root) = subvolume.open(root)? else {
            return Ok(None);
        };
        let mut buf = Box::new(InoPathBuf {
            bytes_left: 0,
            bytes_missing: 0,
            elem_cnt: 0,
            elem_missed: 0,
            data: [0; INO_PATH_DATA_SIZE],
        });
        let mut args = btrfs_ioctl_ino_path_args {
            inum: inode,
            size: size_of::<InoPathBuf>() as u64,
            reserved: [0; 4],
            fspath: (&raw mut *buf).cast::<c_void>() as u64,
        };
        // The kernel looks `inum` up in the subvolume of the descriptor it is
        // asked through.
        let rc = unsafe {
            ioctl(
                subvolume_root.as_raw_fd(),
                BTRFS_IOC_INO_PATHS as c_ulong,
                (&raw mut args).cast(),
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        if buf.elem_cnt == 0 {
            return Ok(None);
        }
        // Each entry is a byte offset from the start of `data` (which is
        // where the kernel's own `val` array starts) to a NUL-terminated
        // path, relative to the subvolume root.
        let offset = u64::from_ne_bytes(buf.data[0..8].try_into().unwrap()) as usize;
        let name = buf
            .data
            .get(offset..)
            .ok_or_else(|| io::Error::other("path offset runs past its buffer"))?;
        Ok(Some(subvolume.path.join(cstr_bytes(name))))
    }
}

/// One extent, as [`ExtentWalk`] resolves it.
pub enum Resolved {
    /// The extent, with every reference to it, wherever on the filesystem.
    Extent(Extent),
    /// Nothing safe can be said about the extent at this address, for the
    /// reason `error` gives: it changed while it was being read, or looking it
    /// up failed.
    LeftAlone { disk_address: u64, error: io::Error },
}

/// Every data extent on the filesystem, in address order, read from the
/// extent tree a batch at a time as they are asked for.
///
/// Each extent is met once, however many files and snapshots reference it,
/// and is handed over with every one of those references. Nothing is kept
/// about it afterwards. An extent can be dealt with, rewritten even, before
/// the next is read.
///
/// The walk runs over a tree that changes under it, our own rewrites
/// included. It resumes from an address rather than a place in the tree, so
/// extents freed or allocated behind it make no difference. Extents allocated
/// ahead of it after it started, our own copies among them, are passed over:
/// they are fully referenced when they are made, and counting them would count
/// the same data twice.
///
/// An `Err` is a search over the extent tree that failed outright. It ends the
/// walk.
pub struct ExtentWalk {
    fs: Rc<Filesystem>,
    /// The address the next batch starts from, or `None` once the extent tree
    /// is read to its end.
    next: Option<u64>,
    /// Extents allocated after this generation are passed over.
    generation: u64,
    /// The batch read last, less the extents already handed over.
    batch: vec::IntoIter<Unresolved>,
}

impl Iterator for ExtentWalk {
    type Item = io::Result<Resolved>;

    fn next(&mut self) -> Option<io::Result<Resolved>> {
        loop {
            if let Some(Unresolved {
                disk_address,
                item,
                backrefs,
            }) = self.batch.next()
            {
                return Some(Ok(match self.fs.resolve(disk_address, &item, backrefs) {
                    Ok(refs) => Resolved::Extent(Extent::from_refs(refs)),
                    Err(error) => Resolved::LeftAlone {
                        disk_address,
                        error,
                    },
                }));
            }

            match self.fs.extent_batch(self.next?, self.generation) {
                Ok((batch, next)) => {
                    self.batch = batch.into_iter();
                    self.next = next;
                }
                Err(e) => {
                    self.next = None;
                    return Some(Err(e));
                }
            }
        }
    }
}

/// Looks `objectid` up in subvolume `treeid`, or in the subvolume `fd` is in if
/// `treeid` is zero. The kernel fills in the tree id it looked in, and the path
/// of `objectid` from that subvolume's root.
fn ino_lookup(fd: c_int, treeid: u64, objectid: u64) -> io::Result<PathBuf> {
    ino_lookup_args(fd, treeid, objectid).map(|args| {
        let name: Vec<u8> = args.name.iter().map(|&c| c as u8).collect();
        PathBuf::from(cstr_bytes(&name))
    })
}

fn ino_lookup_args(
    fd: c_int,
    treeid: u64,
    objectid: u64,
) -> io::Result<btrfs_ioctl_ino_lookup_args> {
    let mut args: btrfs_ioctl_ino_lookup_args = unsafe { std::mem::zeroed() };
    args.treeid = treeid;
    args.objectid = objectid;
    let rc = unsafe { ioctl(fd, BTRFS_IOC_INO_LOOKUP as c_ulong, (&raw mut args).cast()) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(args)
}

/// The subvolume tree id `fd` itself belongs to.
fn own_root_id(fd: c_int) -> io::Result<u64> {
    Ok(ino_lookup_args(fd, 0, BTRFS_FIRST_FREE_OBJECTID as u64)?.treeid)
}

/// Opens the file at `path` read-only, making sure it is still inode `inode` of
/// subvolume `root`. A path is only ever a way to reach an inode: one renamed
/// or replaced since it was found, or hidden under another mount, leads
/// somewhere else.
pub fn open_inode(path: &Path, root: u64, inode: u64) -> io::Result<File> {
    let file = File::open(path)?;
    if file.metadata()?.ino() != inode || own_root_id(file.as_raw_fd())? != root {
        return Err(io::Error::other("no longer the file the extent tree names"));
    }
    Ok(file)
}

/// Reads a NUL-terminated string out of `bytes`, or all of it if there is no
/// NUL — the kernel always writes one, but nothing here needs to trust that.
fn cstr_bytes(bytes: &[u8]) -> &OsStr {
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    OsStr::from_bytes(&bytes[..len])
}

/// `struct btrfs_data_container` with its trailing `val` array given a fixed
/// size, holding the layout [`Filesystem::inode_path`] gives the kernel to
/// fill in: a table of byte offsets (relative to the start of this same
/// array) to the NUL-terminated paths the offsets point at.
#[repr(C)]
struct InoPathBuf {
    bytes_left: u32,
    bytes_missing: u32,
    elem_cnt: u32,
    elem_missed: u32,
    data: [u8; INO_PATH_DATA_SIZE],
}

/// Room for the offset table and path bytes together. The kernel fills no
/// more than 4 KiB of it, header included, whatever size it is told.
const INO_PATH_DATA_SIZE: usize = 4096 - 16;

/// What [`Filesystem::logical_ino`] asks with first: room for about 170
/// references, which is plenty for nearly every extent.
const LOGICAL_INO_START_SIZE: usize = 4096;

/// The most `BTRFS_IOC_LOGICAL_INO_V2` will fill, header included.
const LOGICAL_INO_MAX_SIZE: usize = 16 * 1024 * 1024;

/// The most data btrfs compresses into one extent, and so the most any
/// compressed extent holds uncompressed.
const MAX_COMPRESSED_BYTES: u64 = 128 * 1024;

/// Reads a `T` out of `buf` at `off`, or `None` if `buf` is too short to hold
/// one there. The bytes need not be aligned. Items shorter than the struct we
/// expect are skipped, never trusted.
fn read_struct<T: Copy>(buf: &[u8], off: usize) -> Option<T> {
    let end = off.checked_add(size_of::<T>())?;
    let bytes = buf.get(off..end)?;
    // `bytes` is exactly `size_of::<T>()` initialised bytes; `T: Copy` rules
    // out anything with an invalid bit pattern in the structs used here.
    Some(unsafe { ptr::read_unaligned(bytes.as_ptr().cast::<T>()) })
}

/// Runs a search over file extent items of inode `inode` in subvolume `root`,
/// handing each reference to an extent to `each`. Holes and inline extents are
/// left out; they occupy no extent of their own.
fn search_extent_refs(
    args: &mut SearchArgs,
    fd: c_int,
    root: u64,
    inode: u64,
    mut each: impl FnMut(ExtentRef),
) -> io::Result<()> {
    args.search_all(fd, |header, item| {
        // An item too short for `btrfs_file_extent_item` is a shape we do
        // not understand; skipping it loses one reference, not the file.
        let Some(fe) = read_struct::<btrfs_file_extent_item>(item, 0) else {
            return Ok(());
        };
        if fe.type_ as u32 == BTRFS_FILE_EXTENT_INLINE as u32 {
            return Ok(());
        }
        if fe.disk_bytenr == 0 {
            return Ok(()); // hole
        }
        each(ExtentRef {
            root,
            inode,
            file_offset: header.offset,
            disk_address: fe.disk_bytenr,
            disk_bytes: fe.disk_num_bytes,
            uncompressed_bytes: fe.ram_bytes,
            extent_offset: fe.offset,
            num_bytes: fe.num_bytes,
            // The search reports the generation of the leaf each item is in.
            leaf_generation: header.transid,
        });
        Ok(())
    })
}

/// `struct file_dedupe_range` with its single `info` entry inlined, which is
/// the layout the kernel expects for `dest_count == 1`.
#[repr(C)]
struct DedupeRange {
    range: file_dedupe_range,
    info: file_dedupe_range_info,
}

// The inlined `info` must sit right where the kernel expects `info[0]`, with no
// padding introduced between it and the header.
const _: () = assert!(
    size_of::<DedupeRange>()
        == size_of::<file_dedupe_range>() + size_of::<file_dedupe_range_info>()
);

/// An unnamed file in `dir`, for holding a copy while files are rewritten.
/// It has no link from the moment it exists, so the kernel frees it when the
/// last descriptor closes however the process ends, a kill included: there is
/// no window in which a crash could strand it.
///
/// `dir` itself is not modified. No directory entry is ever made, so not even
/// its mtime moves.
///
/// The file takes its attributes from `dir`, nodatacow included, and btrfs
/// refuses to dedupe between two inodes that disagree about checksums. The
/// flag can still be cleared while the file is empty, so it is.
pub fn temp_file(dir: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_TMPFILE as i32)
        .open(dir)?;
    let flags = get_flags(&file)?;
    if flags & FS_NOCOW_FL != 0 {
        set_flags(&file, flags & !FS_NOCOW_FL)?;
        if is_nocow(&file)? {
            return Err(io::Error::other(
                "the temporary file keeps nodatacow from its directory",
            ));
        }
    }
    Ok(file)
}

/// Whether the file is marked nodatacow, in which case rewriting it neither
/// helps nor is safe to dedupe.
pub fn is_nocow(file: &File) -> io::Result<bool> {
    Ok(get_flags(file)? & FS_NOCOW_FL != 0)
}

/// The file's inode flags, as `lsattr` shows them.
fn get_flags(file: &File) -> io::Result<u32> {
    let mut flags: c_int = 0;
    let rc = unsafe {
        ioctl(
            file.as_raw_fd(),
            FS_IOC_GETFLAGS as c_ulong,
            (&raw mut flags).cast(),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(flags as u32)
}

/// Sets the file's inode flags, as `chattr` does.
fn set_flags(file: &File, flags: u32) -> io::Result<()> {
    let mut flags = flags as c_int;
    let rc = unsafe {
        ioctl(
            file.as_raw_fd(),
            FS_IOC_SETFLAGS as c_ulong,
            (&raw mut flags).cast(),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Points `dest` at `src`'s extents for a range the two already hold identical
/// bytes in. The kernel re-checks that equality under lock, so a file changing
/// underneath us fails here rather than corrupting anything.
///
/// Returns the bytes actually redirected, which can be short of `len`.
pub fn dedupe(
    src: &File,
    src_offset: u64,
    len: u64,
    dest: &File,
    dest_offset: u64,
) -> io::Result<u64> {
    let mut args = DedupeRange {
        range: file_dedupe_range {
            src_offset,
            src_length: len,
            dest_count: 1,
            reserved1: 0,
            reserved2: 0,
            info: __IncompleteArrayField::new(),
        },
        info: file_dedupe_range_info {
            dest_fd: dest.as_raw_fd() as i64,
            dest_offset,
            bytes_deduped: 0,
            status: 0,
            reserved: 0,
        },
    };

    let rc = unsafe {
        ioctl(
            src.as_raw_fd(),
            FIDEDUPERANGE as c_ulong,
            (&raw mut args).cast(),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    if args.info.status != FILE_DEDUPE_RANGE_SAME as i32 {
        return Err(io::Error::other(format!(
            "dedupe reported status {} at offset {dest_offset}",
            args.info.status
        )));
    }
    Ok(args.info.bytes_deduped)
}
