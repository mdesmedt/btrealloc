use std::cell::RefCell;
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

use linux_raw_sys::btrfs::{
    BTRFS_EXTENT_DATA_KEY, BTRFS_EXTENT_DATA_REF_KEY, BTRFS_EXTENT_FLAG_DATA,
    BTRFS_EXTENT_ITEM_KEY, BTRFS_EXTENT_OWNER_REF_KEY, BTRFS_EXTENT_TREE_OBJECTID,
    BTRFS_FILE_EXTENT_INLINE, BTRFS_FIRST_FREE_OBJECTID, BTRFS_LOGICAL_INO_ARGS_IGNORE_OFFSET,
    BTRFS_SHARED_DATA_REF_KEY, btrfs_extent_data_ref, btrfs_extent_item, btrfs_extent_owner_ref,
    btrfs_file_extent_item, btrfs_ioctl_ino_lookup_args, btrfs_ioctl_ino_path_args,
    btrfs_ioctl_logical_ino_args, btrfs_ioctl_search_header, btrfs_ioctl_search_key,
};
use linux_raw_sys::general::{
    __IncompleteArrayField, BTRFS_SUPER_MAGIC, FILE_DEDUPE_RANGE_SAME, FS_NOCOW_FL, O_TMPFILE,
    file_dedupe_range, file_dedupe_range_info, statfs,
};
use linux_raw_sys::ioctl::{
    BTRFS_IOC_INO_LOOKUP, BTRFS_IOC_INO_PATHS, BTRFS_IOC_LOGICAL_INO_V2, BTRFS_IOC_TREE_SEARCH_V2,
    FIDEDUPERANGE, FS_IOC_GETFLAGS,
};

use crate::extent::ExtentRef;

/// The tree search buffer. Big enough that a search which comes back with room
/// to spare for the largest item btrfs can store is known to have reached the
/// end of its range, with no further call needed to find that out.
const BUF_SIZE: usize = 256 * 1024;

/// No btrfs item is larger than a tree node, and no tree node is larger than
/// 64 KiB.
const MAX_ITEM_SIZE: usize = 64 * 1024;

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

    /// Runs the search over its whole key range, however many calls that
    /// takes, handing every item found to `each` with its header.
    fn search_all(
        &mut self,
        fd: c_int,
        mut each: impl FnMut(&btrfs_ioctl_search_header, &[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        const HEADER_SIZE: usize = size_of::<btrfs_ioctl_search_header>();
        loop {
            self.search(fd)?;
            if self.key.nr_items == 0 {
                return Ok(());
            }

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

            // The kernel only stops short of the end of the range when the
            // next item does not fit. Room left for any item means it did not
            // stop short.
            if BUF_SIZE - pos >= HEADER_SIZE + MAX_ITEM_SIZE {
                return Ok(());
            }

            // Carry on from just past the last key returned. Keys order as
            // (objectid, type, offset) tuples, and so does the search range.
            let Some(last) = last else { return Ok(()) };
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

/// Who holds an extent, as the extent tree records it.
enum Backrefs {
    /// There is no extent of that size at that address: it was freed while we
    /// scanned.
    Gone,
    /// Every reference names the file holding it directly.
    Files(Vec<DataRef>),
    /// At least one reference goes through a shared metadata block, as a
    /// snapshot or a balance leaves behind, or has a shape not read here. Only
    /// a full backreference walk can say whose files those are. `total` is the
    /// reference count the extent item gives.
    Opaque { total: u64 },
}

/// The extent tree's account of one extent, as it is read item by item.
#[derive(Default)]
struct Tally {
    /// The reference count the extent item gives, once it is found.
    total: Option<u64>,
    /// What the references read so far add up to.
    counted: u64,
    data_refs: Vec<DataRef>,
    /// Whether some reference does not name its file.
    opaque: bool,
}

impl Tally {
    fn add_item(&mut self, key_type: u32, key_offset: u64, disk_bytes: u64, item: &[u8]) {
        match key_type {
            BTRFS_EXTENT_ITEM_KEY => {
                // A different size at this address is a different extent,
                // allocated after ours was freed.
                if key_offset != disk_bytes {
                    return;
                }
                let Some(extent) = read_struct::<btrfs_extent_item>(item, 0) else {
                    self.opaque = true;
                    return;
                };
                if extent.flags & BTRFS_EXTENT_FLAG_DATA as u64 == 0 {
                    self.opaque = true;
                }
                self.total = Some(extent.refs);
                self.add_inline(&item[size_of::<btrfs_extent_item>()..]);
            }
            BTRFS_EXTENT_DATA_REF_KEY => match read_struct::<btrfs_extent_data_ref>(item, 0) {
                Some(r) => self.add_data_ref(&r),
                None => self.opaque = true,
            },
            // A shared data reference stored as its own item, or anything else
            // in the backreference key range.
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

    fn finish(self) -> Backrefs {
        match self.total {
            None => Backrefs::Gone,
            Some(total) if self.opaque || self.counted != total => Backrefs::Opaque { total },
            Some(_) => Backrefs::Files(self.data_refs),
        }
    }
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

/// A handle on the mounted filesystem, for questions about it as a whole
/// rather than about one file.
pub struct Filesystem {
    file: File,
    /// The sector size: a file extent reference covers whole sectors, never
    /// part of one. It is the page size at mkfs time, typically 4096 bytes.
    pub sectorsize: u64,
    /// One search buffer, reused for every search made through this handle.
    searchargs: RefCell<Box<SearchArgs>>,
    /// The buffer [`Filesystem::logical_ino`] hands the kernel, as `u64`s:
    /// `struct btrfs_data_container`'s four `u32`s fill the first two. It
    /// starts small, because the kernel zeroes and copies all of it on every
    /// call, and grows for the rare extent with more references than fit.
    logicalino: RefCell<Vec<u64>>,
    /// The subvolume tree id of the filesystem handle opened on, for telling
    /// apart a backreference in this subvolume from one in another (most often
    /// a snapshot's).
    root_id: u64,
    /// The absolute path of that subvolume's own root directory, found by
    /// walking up from the path opened. `None` if that walk never reached it,
    /// in which case a backreference can never be resolved to a path.
    subvol_root: Option<PathBuf>,
}

impl Filesystem {
    /// Any path on the filesystem will do. Fails if that path is not on btrfs:
    /// every extent question we ask later goes through btrfs-only ioctls.
    pub fn open(path: &Path) -> io::Result<Filesystem> {
        let file = File::open(path)?;

        let mut buf: statfs = unsafe { std::mem::zeroed() };
        if unsafe { fstatfs(file.as_raw_fd(), &raw mut buf) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if buf.f_type as u64 != BTRFS_SUPER_MAGIC as u64 {
            return Err(io::Error::other("not a btrfs filesystem"));
        }

        // btrfs reports its sector size as the block size, so the statfs above
        // is all it takes to ask.
        let sectorsize = buf.f_bsize as u64;
        if !sectorsize.is_power_of_two() {
            return Err(io::Error::other(format!(
                "sector size {sectorsize} is not a power of two"
            )));
        }

        let root_id = own_root_id(file.as_raw_fd())?;
        let dev = file.metadata()?.dev();
        let subvol_root = find_subvol_root(path, dev);

        Ok(Filesystem {
            file,
            sectorsize,
            searchargs: RefCell::new(SearchArgs::new(0)),
            logicalino: RefCell::new(vec![0; LOGICAL_INO_START_SIZE / size_of::<u64>()]),
            root_id,
            subvol_root,
        })
    }

    /// Resolves every extent `refs` point into, `refs` being everything one
    /// file references, to every reference the whole filesystem holds to it,
    /// down to the file and byte range each one is at. Each extent is handed to
    /// `each` as soon as it is resolved, by address: `Ok(None)` means there is
    /// nothing safe to say about it. It was freed while we scanned, or at least
    /// one reference belongs to an inode we cannot resolve to a path on this
    /// handle (most often a snapshot's, in a different subvolume tree than the
    /// one opened).
    ///
    /// The extent tree keeps an extent's references next to the extent, in
    /// address order, and a file's extents mostly sit close together on disk.
    /// So they are read a stretch of addresses at a time, many extents to a
    /// search, rather than one search per extent. Most extents are settled by
    /// that alone. The rest need the files the extent tree names searched, and
    /// only the stretch of each that can hold a reference; where it names no
    /// file, the kernel's backreference walk finds them.
    ///
    /// An `Err` is a search that failed outright, and ends the file: the
    /// extents handed over before it stand.
    pub fn resolve_extents(
        &self,
        mut refs: Vec<ExtentRef>,
        mut each: impl FnMut(u64, io::Result<Option<Vec<ExtentRef>>>),
    ) -> io::Result<()> {
        // One group per extent, in address order.
        refs.sort_unstable_by_key(|r| (r.disk_address, r.file_offset));
        let mut groups: Vec<Vec<ExtentRef>> = Vec::new();
        for r in refs {
            match groups.last_mut() {
                Some(group) if group[0].disk_address == r.disk_address => group.push(r),
                _ => groups.push(vec![r]),
            }
        }

        let mut groups = groups.into_iter().peekable();
        while let Some(group) = groups.next() {
            // Extents close enough together to read with one search.
            let mut window = vec![group];
            while let Some(next) = groups.peek()
                && window.len() < MAX_WINDOW_EXTENTS
            {
                let last = &window[window.len() - 1][0];
                let end = last.disk_address.saturating_add(last.disk_bytes);
                if next[0].disk_address > end.saturating_add(MAX_WINDOW_GAP) {
                    break;
                }
                window.extend(groups.next());
            }

            let backrefs = self.backrefs(&window)?;
            for (group, backrefs) in window.into_iter().zip(backrefs) {
                let disk_address = group[0].disk_address;
                each(disk_address, self.resolve(group, backrefs));
            }
        }
        Ok(())
    }

    /// Reads who holds each extent in `window` from the extent tree: every
    /// extent item with the references stored inline in it, and the ones
    /// stored as items of their own after it, all in one search over the
    /// addresses the window spans. Items of extents not in the window are
    /// passed over.
    fn backrefs(&self, window: &[Vec<ExtentRef>]) -> io::Result<Vec<Backrefs>> {
        let first = window[0][0].disk_address;
        let last = window[window.len() - 1][0].disk_address;

        let mut args = self.searchargs.borrow_mut();
        args.key.tree_id = BTRFS_EXTENT_TREE_OBJECTID as u64;
        args.set_range(
            (first, BTRFS_EXTENT_ITEM_KEY, 0),
            (last, BTRFS_SHARED_DATA_REF_KEY, u64::MAX),
        );

        let mut tallies: Vec<Tally> = window.iter().map(|_| Tally::default()).collect();
        args.search_all(self.file.as_raw_fd(), |header, item| {
            if !(BTRFS_EXTENT_ITEM_KEY..=BTRFS_SHARED_DATA_REF_KEY).contains(&header.type_) {
                return Ok(());
            }
            let Ok(i) =
                window.binary_search_by_key(&header.objectid, |group| group[0].disk_address)
            else {
                return Ok(());
            };
            let disk_bytes = window[i][0].disk_bytes;
            tallies[i].add_item(header.type_, header.offset, disk_bytes, item);
            Ok(())
        })?;
        Ok(tallies.into_iter().map(Tally::finish).collect())
    }

    /// Every reference to one extent, given the references to it one file
    /// holds (`local`) and what the extent tree says about it.
    fn resolve(
        &self,
        local: Vec<ExtentRef>,
        backrefs: Backrefs,
    ) -> io::Result<Option<Vec<ExtentRef>>> {
        let data_refs = match backrefs {
            Backrefs::Gone => return Ok(None),
            // One reference in all, and it is the one in hand. Which tree block
            // records it makes no difference to that. A snapshot still sharing
            // that block would see it too, and keep the extent after a rewrite;
            // that costs a copy, never data.
            Backrefs::Opaque { total: 1 } if local.len() == 1 => return Ok(Some(local)),
            Backrefs::Opaque { .. } => self.logical_ino(local[0].disk_address)?,
            Backrefs::Files(data_refs) => data_refs,
        };
        if data_refs.iter().any(|r| r.root != self.root_id) {
            return Ok(None);
        }

        let first = &local[0];
        let (inode, disk_address, uncompressed_bytes) =
            (first.inode, first.disk_address, first.uncompressed_bytes);
        // This file's own references are all in hand already. If they are not
        // all there is to this file, it changed since it was read: leave the
        // extent for now rather than act on half a picture.
        let own: u64 = data_refs
            .iter()
            .filter(|r| r.inode == inode)
            .map(|r| r.count)
            .sum();
        if own != local.len() as u64 {
            return Ok(None);
        }

        // Any other file's are read from the stretch of that file which can
        // hold them.
        let mut refs = local;
        let mut files: Vec<(u64, Rc<PathBuf>, bool)> = Vec::new();
        for data_ref in data_refs.iter().filter(|r| r.inode != inode) {
            let (path, nocow) = match files.iter().find(|(inode, ..)| *inode == data_ref.inode) {
                Some((_, path, nocow)) => (Rc::clone(path), *nocow),
                None => {
                    let Some(path) = self.resolve_path(data_ref.inode)? else {
                        return Ok(None);
                    };
                    let path = Rc::new(path);
                    let nocow = is_nocow(&File::open(&*path)?)?;
                    files.push((data_ref.inode, Rc::clone(&path), nocow));
                    (path, nocow)
                }
            };

            let found = refs.len();
            self.data_ref_extents(
                data_ref,
                disk_address,
                uncompressed_bytes,
                &path,
                nocow,
                &mut refs,
            )?;
            if (refs.len() - found) as u64 != data_ref.count {
                return Ok(None);
            }
        }
        Ok(Some(refs))
    }

    /// The file extent items one [`DataRef`] stands for, searched for in only
    /// the stretch of its file that can hold them.
    fn data_ref_extents(
        &self,
        data_ref: &DataRef,
        disk_address: u64,
        uncompressed_bytes: u64,
        path: &Rc<PathBuf>,
        nocow: bool,
        refs: &mut Vec<ExtentRef>,
    ) -> io::Result<()> {
        let (start, last) = match data_ref.at {
            // Each item sits at the base plus its own offset into the extent,
            // which is less than the extent's uncompressed length. The base
            // wraps for an item placed earlier in its file than it sits in the
            // extent, and the stretch then starts at the beginning of the file.
            RefOffsets::Base(base) => {
                let last = base.wrapping_add(uncompressed_bytes.saturating_sub(1));
                (if last >= base { base } else { 0 }, last)
            }
            RefOffsets::Between(first, last) => (first, last),
        };

        let mut args = self.searchargs.borrow_mut();
        args.key.tree_id = self.root_id;
        args.set_range(
            (data_ref.inode, BTRFS_EXTENT_DATA_KEY, start),
            (data_ref.inode, BTRFS_EXTENT_DATA_KEY, last),
        );

        search_extent_refs(
            &mut args,
            self.file.as_raw_fd(),
            data_ref.inode,
            path,
            nocow,
            |r| {
                let belongs = match data_ref.at {
                    RefOffsets::Base(base) => r.file_offset.wrapping_sub(r.extent_offset) == base,
                    RefOffsets::Between(..) => true,
                };
                if r.disk_address == disk_address && belongs {
                    refs.push(r);
                }
            },
        )
    }

    /// Every file extent item referencing the extent at `disk_address`, found
    /// by the kernel's backreference walk, which follows shared tree blocks up
    /// to the subvolumes holding them. Gathered into the same shape the extent
    /// tree gives when it names the files itself.
    fn logical_ino(&self, disk_address: u64) -> io::Result<Vec<DataRef>> {
        let mut buf = self.logicalino.borrow_mut();
        loop {
            let size = buf.len() * size_of::<u64>();
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
                let wanted = size + bytes_missing;
                if wanted > LOGICAL_INO_MAX_SIZE {
                    return Err(io::Error::other(
                        "extent has more backreferences than fit in one lookup",
                    ));
                }
                buf.resize(wanted.div_ceil(size_of::<u64>()), 0);
                continue;
            }

            // Each result is an (inode, offset, root) triple, one per file
            // extent item. With the offset ignored going in, the offset coming
            // out is the item's own file offset, so each file's items are
            // counted and bounded by the first and last of them.
            let elem_cnt = buf[1] as u32 as usize;
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

    /// The absolute path of inode `inum` in this filesystem's own subvolume, or
    /// `None` if that subvolume's root was never found, or `inum` has no name
    /// any more (an orphan, unlinked but still open elsewhere).
    ///
    /// An inode can have more than one name — hardlinks — in which case this
    /// takes the first the kernel returns. Any of them opens the same data,
    /// which is all a rewrite needs.
    fn resolve_path(&self, inum: u64) -> io::Result<Option<PathBuf>> {
        let Some(subvol_root) = &self.subvol_root else {
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
            inum,
            size: size_of::<InoPathBuf>() as u64,
            reserved: [0; 4],
            fspath: (&raw mut *buf).cast::<c_void>() as u64,
        };
        let rc = unsafe {
            ioctl(
                self.file.as_raw_fd(),
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
        Ok(Some(subvol_root.join(cstr_bytes(name))))
    }
}

/// The subvolume tree id `fd` itself belongs to.
fn own_root_id(fd: c_int) -> io::Result<u64> {
    let mut args: btrfs_ioctl_ino_lookup_args = unsafe { std::mem::zeroed() };
    args.treeid = 0; // the tree `fd` lives in
    args.objectid = BTRFS_FIRST_FREE_OBJECTID as u64;
    let rc = unsafe { ioctl(fd, BTRFS_IOC_INO_LOOKUP as c_ulong, (&raw mut args).cast()) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    // The kernel fills in the tree id it resolved `treeid: 0` to.
    Ok(args.treeid)
}

/// The absolute path of the subvolume root `path` lives under, found by
/// walking up from it until an ancestor's inode number is the one every btrfs
/// subvolume root has. `None` if that walk runs off the filesystem (`dev`
/// changes) or off the directory tree first.
fn find_subvol_root(path: &Path, dev: u64) -> Option<PathBuf> {
    let path = path.canonicalize().ok()?;
    for ancestor in path.ancestors() {
        let meta = std::fs::metadata(ancestor).ok()?;
        if meta.dev() != dev {
            return None;
        }
        if meta.ino() == BTRFS_FIRST_FREE_OBJECTID as u64 {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

/// Reads a NUL-terminated string out of `bytes`, or all of it if there is no
/// NUL — the kernel always writes one, but nothing here needs to trust that.
fn cstr_bytes(bytes: &[u8]) -> &OsStr {
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    OsStr::from_bytes(&bytes[..len])
}

/// `struct btrfs_data_container` with its trailing `val` array given a fixed
/// size, holding the layout [`Filesystem::resolve_path`] gives the kernel to
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

/// Extents further apart on disk than this are looked up with searches of
/// their own. Whatever lies between two extents in one search is read and
/// passed over, and this bounds it to a few hundred other extents' items.
const MAX_WINDOW_GAP: u64 = 1024 * 1024;

/// The most extents looked up with one search over the extent tree, which
/// bounds what is kept about them until each is handed over.
const MAX_WINDOW_EXTENTS: usize = 4096;

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

/// Every extent reference held by the file at `path`. Holes and inline extents
/// are left out; they occupy no extent of their own.
///
/// Requires `CAP_SYS_ADMIN` — the search ioctl is root-only.
pub fn file_extents(path: &Path) -> io::Result<Vec<ExtentRef>> {
    let file = File::open(path)?;
    let path = Rc::new(path.to_path_buf());
    let ino = file.metadata()?.ino();
    let nocow = is_nocow(&file)?;

    thread_local! {
        /// One search buffer for every file read, rather than one allocated
        /// for each of them.
        static ARGS: RefCell<Box<SearchArgs>> = RefCell::new(SearchArgs::new(0));
    }

    let mut extents = Vec::new();
    ARGS.with_borrow_mut(|args| {
        args.key.tree_id = 0; // the tree the fd lives in
        args.set_range(
            (ino, BTRFS_EXTENT_DATA_KEY, 0),
            (ino, BTRFS_EXTENT_DATA_KEY, u64::MAX),
        );
        search_extent_refs(args, file.as_raw_fd(), ino, &path, nocow, |r| {
            extents.push(r)
        })
    })?;
    Ok(extents)
}

/// Runs a search over file extent items of inode `inode`, handing each
/// reference to an extent to `each`. Holes and inline extents are left out.
fn search_extent_refs(
    args: &mut SearchArgs,
    fd: c_int,
    inode: u64,
    path: &Rc<PathBuf>,
    nocow: bool,
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
            path: Rc::clone(path),
            inode,
            file_offset: header.offset,
            disk_address: fe.disk_bytenr,
            disk_bytes: fe.disk_num_bytes,
            uncompressed_bytes: fe.ram_bytes,
            extent_offset: fe.offset,
            num_bytes: fe.num_bytes,
            nocow,
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

/// An unnamed file in `dir`, for holding a copy while one file is rewritten.
/// It has no link from the moment it exists, so the kernel frees it when the
/// last descriptor closes however the process ends, a kill included: there is
/// no window in which a crash could strand it.
///
/// `dir` itself is not modified. No directory entry is ever made, so not even
/// its mtime moves.
pub fn temp_file(dir: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_TMPFILE as i32)
        .open(dir)
}

/// Whether the file is marked nodatacow, in which case rewriting it neither
/// helps nor is safe to dedupe.
pub fn is_nocow(file: &File) -> io::Result<bool> {
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
    Ok(flags as u32 & FS_NOCOW_FL != 0)
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
