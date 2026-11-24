use std::collections::HashMap;
use std::ffi::{CString, OsStr, c_int, c_long, c_ulong, c_void};
use std::fmt;
use std::fs::File;
use std::io;
use std::mem::size_of;
use std::ops::{Deref, DerefMut};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Arc, Mutex};

use linux_raw_sys::btrfs::{
    BTRFS_BLOCK_GROUP_DATA, BTRFS_BLOCK_GROUP_METADATA, BTRFS_EXTENT_DATA_KEY,
    BTRFS_EXTENT_DATA_REF_KEY, BTRFS_EXTENT_FLAG_DATA, BTRFS_EXTENT_ITEM_KEY,
    BTRFS_EXTENT_OWNER_REF_KEY, BTRFS_EXTENT_TREE_OBJECTID, BTRFS_FILE_EXTENT_INLINE,
    BTRFS_FIRST_FREE_OBJECTID, BTRFS_FS_INFO_FLAG_GENERATION, BTRFS_FS_TREE_OBJECTID,
    BTRFS_INODE_ITEM_KEY, BTRFS_LOGICAL_INO_ARGS_IGNORE_OFFSET, BTRFS_ROOT_BACKREF_KEY,
    BTRFS_ROOT_ITEM_KEY, BTRFS_ROOT_TREE_OBJECTID, BTRFS_SHARED_DATA_REF_KEY, BTRFS_SUBVOL_RDONLY,
    btrfs_extent_data_ref, btrfs_extent_item, btrfs_extent_owner_ref, btrfs_file_extent_item,
    btrfs_inode_item, btrfs_ioctl_fs_info_args, btrfs_ioctl_ino_lookup_args,
    btrfs_ioctl_ino_path_args, btrfs_ioctl_logical_ino_args, btrfs_ioctl_search_header,
    btrfs_ioctl_search_key, btrfs_ioctl_space_args, btrfs_ioctl_space_info, btrfs_root_item,
    btrfs_root_ref,
};
use linux_raw_sys::errno::{EDQUOT, EIO, ENOSPC, EROFS, ESTALE, EUCLEAN};
use linux_raw_sys::general::{
    __IncompleteArrayField, __NR_open_by_handle_at, __NR_open_tree, __NR_openat2, AT_EMPTY_PATH,
    BTRFS_SUPER_MAGIC, FILE_DEDUPE_RANGE_SAME, FS_NOCOW_FL, O_CLOEXEC, O_DIRECTORY, O_NOATIME,
    O_NOFOLLOW, O_PATH, O_RDONLY, O_RDWR, O_TMPFILE, OPEN_TREE_CLOEXEC, OPEN_TREE_CLONE,
    RESOLVE_BENEATH, RESOLVE_NO_SYMLINKS, RESOLVE_NO_XDEV, file_dedupe_range,
    file_dedupe_range_info, open_how, statfs, winsize,
};
use linux_raw_sys::ioctl::{
    BTRFS_IOC_FS_INFO, BTRFS_IOC_INO_LOOKUP, BTRFS_IOC_INO_PATHS, BTRFS_IOC_LOGICAL_INO_V2,
    BTRFS_IOC_SPACE_INFO, BTRFS_IOC_SUBVOL_GETFLAGS, BTRFS_IOC_TREE_SEARCH_V2, FIDEDUPERANGE,
    FS_IOC_GETFLAGS, FS_IOC_SETFLAGS, TIOCGWINSZ,
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
    fn syscall(number: c_long, ...) -> c_long;
}

/// A private copy of the mount `fd` is on, attached nowhere, with nothing
/// mounted on top of it. Mounts made over parts of the original, such as the
/// read-only bind NixOS puts over `/nix/store`, are not carried over, so every
/// path from its root ends on the filesystem itself.
///
/// It lives only as long as the descriptor returned, and whatever is opened
/// through it: the kernel unmounts it once those close, however the process
/// ends, a kill included. Nothing is ever left in the mount table.
fn clone_mount(fd: c_int) -> io::Result<OwnedFd> {
    let rc = unsafe {
        syscall(
            __NR_open_tree as c_long,
            fd,
            c"".as_ptr(),
            (OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC | AT_EMPTY_PATH) as c_ulong,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(rc as c_int) })
}

/// Opens `path` below the directory `dir`, never leaving it: not through `..`,
/// a symlink, or onto another mount. An empty `path` opens `dir` itself.
fn open_beneath(dir: c_int, path: &Path, flags: u32) -> io::Result<File> {
    let path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("path holds a NUL byte"))?;
    let mut how = open_how {
        flags: (flags | O_CLOEXEC) as u64,
        mode: 0,
        resolve: (RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV) as u64,
    };
    let rc = unsafe {
        syscall(
            __NR_openat2 as c_long,
            dir,
            path.as_ptr(),
            &raw mut how,
            size_of::<open_how>(),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(rc as c_int) })
}

/// `FILEID_BTRFS_WITHOUT_PARENT`, from `include/linux/exportfs.h`: a btrfs
/// file handle naming an inode alone, with nothing about its parent directory.
const FILEID_BTRFS_WITHOUT_PARENT: c_int = 0x4d;

/// `struct file_handle` holding a [`FILEID_BTRFS_WITHOUT_PARENT`] handle: the
/// first three fields of btrfs's `struct btrfs_fid`, packed, which are the
/// inode number, its subvolume's tree id and the inode's generation. This is
/// exactly what `name_to_handle_at` gives for a file on btrfs.
#[repr(C)]
struct FileHandle {
    handle_bytes: u32,
    handle_type: c_int,
    f_handle: [u8; 20],
}

/// Opens inode `inode` of subvolume `root`, created in `generation`, by file
/// handle, through `mount`: any descriptor on the filesystem will do, whatever
/// subvolume it is in. Needs `CAP_DAC_READ_SEARCH`.
///
/// btrfs only checks the generation when it is not zero, so the real one has
/// to be passed for an inode number reused since to be refused, with `ESTALE`.
fn open_by_handle(
    mount: c_int,
    root: u64,
    inode: u64,
    generation: u64,
    flags: u32,
) -> io::Result<File> {
    let mut handle = FileHandle {
        handle_bytes: 20,
        handle_type: FILEID_BTRFS_WITHOUT_PARENT,
        f_handle: [0; 20],
    };
    handle.f_handle[0..8].copy_from_slice(&inode.to_le_bytes());
    handle.f_handle[8..16].copy_from_slice(&root.to_le_bytes());
    // btrfs keeps the generation in 32 bits here, as the inode's i_generation.
    handle.f_handle[16..20].copy_from_slice(&(generation as u32).to_le_bytes());
    let rc = unsafe {
        syscall(
            __NR_open_by_handle_at as c_long,
            mount,
            &raw mut handle,
            (flags | O_CLOEXEC) as c_int,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(rc as c_int) })
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

/// A data extent as it is read from the extent tree, before it is resolved to
/// the file extent items pointing into it: see [`Filesystem::resolve`].
pub struct Unresolved {
    disk_address: u64,
    item: ExtentItem,
    backrefs: Backrefs,
}

impl Unresolved {
    pub fn disk_address(&self) -> u64 {
        self.disk_address
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

/// One subvolume, by its path from the top-level one, as `btrfs subvolume
/// list` gives it. The path is only for naming the files in it: they are
/// opened by handle, which needs no path.
struct Subvolume {
    path: PathBuf,
}

/// What a file outside the subvolume the tool was pointed at is named under,
/// in front of its path from the top-level subvolume: the name `btrfs
/// subvolume list -a` gives the top-level subvolume.
const FS_TREE: &str = "<FS_TREE>";

/// The tree id of the top-level subvolume, the one `subvolid=5` mounts.
pub const TOP_LEVEL: u64 = BTRFS_FS_TREE_OBJECTID as u64;

/// A handle on the whole filesystem, through a directory on it: the root of
/// one of its subvolumes, or any directory inside one. Every subvolume, and
/// every inode in it, is reached from there by file handle, wherever it is.
///
/// Everything is reached through a private copy of the mount, not through the
/// mount itself: see [`clone_mount`].
pub struct Filesystem {
    /// The directory the filesystem was opened through, in the private mount.
    dir: File,
    /// The root directory of the subvolume that directory is in, opened by
    /// handle through the private mount. The temporary copies are made in it.
    subvolume_root: File,
    /// The private mount, kept for as long as this is.
    _private: OwnedFd,
    /// The path of that directory, which the paths of files below it are
    /// shown under.
    dir_path: PathBuf,
    /// The same directory's path from the root of its subvolume.
    within: PathBuf,
    /// The device that directory is on, as `stat` gives it: btrfs gives every
    /// subvolume a device of its own, so everything on the same one is in the
    /// same subvolume.
    pub device: u64,
    /// The tree id of the subvolume that directory is in: 5 for the top-level
    /// one.
    pub root: u64,
    /// Whether that directory is its subvolume's root, as where a subvolume is
    /// mounted is.
    pub at_subvolume_root: bool,
    /// Whether that subvolume is read-only, as a snapshot made with `-r` is,
    /// which leaves nowhere to make the copies in.
    pub subvolume_read_only: bool,
    /// The sector size: a file extent reference covers whole sectors, never
    /// part of one. It is the page size at mkfs time, typically 4096 bytes.
    pub sectorsize: u64,
    /// Whether it is mounted read-only, which leaves nothing to rewrite with.
    pub read_only: bool,
    /// Search buffers not in use, reused for every search made through this
    /// handle. There are as many as there have ever been searches at once:
    /// about one per thread resolving extents. See [`Filesystem::searchargs`].
    searchargs: Mutex<Vec<Box<SearchArgs>>>,
    /// Every subvolume looked up so far, by tree id, or `None` for one there
    /// is no path to.
    subvolumes: Mutex<HashMap<u64, Option<Arc<Subvolume>>>>,
    /// Each subvolume's last snapshot, by tree id, as read for the current
    /// batch of the walk. Forgotten with every batch, so a snapshot taken
    /// mid-run is not missed.
    last_snapshots: Mutex<HashMap<u64, u64>>,
}

/// A search buffer on loan from [`Filesystem::searchargs`], handed back when
/// dropped.
struct SearchArgsGuard<'a> {
    pool: &'a Mutex<Vec<Box<SearchArgs>>>,
    args: Option<Box<SearchArgs>>,
}

impl Deref for SearchArgsGuard<'_> {
    type Target = SearchArgs;
    fn deref(&self) -> &SearchArgs {
        self.args.as_ref().unwrap()
    }
}

impl DerefMut for SearchArgsGuard<'_> {
    fn deref_mut(&mut self) -> &mut SearchArgs {
        self.args.as_mut().unwrap()
    }
}

impl Drop for SearchArgsGuard<'_> {
    fn drop(&mut self) {
        if let Some(args) = self.args.take() {
            self.pool.lock().unwrap().push(args);
        }
    }
}

impl Filesystem {
    /// Opens the filesystem through the directory `path`, any directory on it:
    /// the root of a subvolume, where one is mounted, or one inside it. Every
    /// subvolume is reached from there all the same. Anything else is refused.
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
            return Err(io::Error::other("not a directory"));
        }
        let root = own_root_id(file.as_raw_fd())?;
        let at_subvolume_root = meta.ino() == BTRFS_FIRST_FREE_OBJECTID as u64;
        let within = if at_subvolume_root {
            PathBuf::new()
        } else {
            ino_lookup(file.as_raw_fd(), root, meta.ino())?
        };

        let private = clone_mount(file.as_raw_fd()).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("making a private mount to work through: {e}"),
            )
        })?;
        let dir = open_beneath(private.as_raw_fd(), Path::new(""), O_RDONLY | O_DIRECTORY)?;

        // Opened properly once `fs` can open it by handle, just below.
        let placeholder = dir.try_clone()?;
        let mut fs = Filesystem {
            dir,
            subvolume_root: placeholder,
            _private: private,
            dir_path: path.to_path_buf(),
            within,
            device: meta.dev(),
            root,
            at_subvolume_root,
            subvolume_read_only: false,
            sectorsize,
            read_only: buf.f_flags as u64 & ST_RDONLY != 0,
            searchargs: Mutex::new(Vec::new()),
            subvolumes: Mutex::new(HashMap::new()),
            last_snapshots: Mutex::new(HashMap::new()),
        };
        // The subvolume's root directory, which `path` need not be, reached by
        // handle through the private mount whether or not that mount shows it.
        fs.subvolume_root = fs
            .open_handle(
                root,
                BTRFS_FIRST_FREE_OBJECTID as u64,
                O_RDONLY | O_DIRECTORY,
            )?
            .ok_or_else(|| io::Error::other(format!("subvolume {root} has no root directory")))?;
        fs.subvolume_read_only =
            subvolume_flags(fs.subvolume_root.as_raw_fd())? & BTRFS_SUBVOL_RDONLY as u64 != 0;
        Ok(fs)
    }

    /// A path to the directory the filesystem was opened through, as the
    /// private mount shows it: with nothing mounted anywhere below it, so a
    /// walk from here sees only what is really in the directory, and nothing
    /// a mount there hides or brings in from elsewhere.
    pub fn private_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.dir.as_raw_fd()))
    }

    /// The entry `name` in the directory the filesystem was opened through,
    /// as the private mount shows it, without following it if it is a
    /// symbolic link: opened only to look at, with `O_PATH`.
    pub fn entry(&self, name: &Path) -> io::Result<File> {
        open_beneath(self.dir.as_raw_fd(), name, O_PATH | O_NOFOLLOW)
    }

    /// Hands `each` the address of every extent inode `inode` points into, of
    /// the subvolume the filesystem was opened in, a page at a time: for the
    /// file extent items from file offset `from` on, as many as one search
    /// returns. Returns the file offset the next page starts at, or `None`
    /// after the last page.
    ///
    /// An extent comes up once for each item pointing into it, so more than
    /// once if part of it was overwritten. Holes and inline extents are left
    /// out.
    pub fn inode_extent_page(
        &self,
        inode: u64,
        from: u64,
        mut each: impl FnMut(u64),
    ) -> io::Result<Option<u64>> {
        let root = self.root;
        let mut args = self.searchargs();
        args.key.tree_id = root;
        args.set_range(
            (inode, BTRFS_EXTENT_DATA_KEY, from),
            (inode, BTRFS_EXTENT_DATA_KEY, u64::MAX),
        );
        args.search(self.dir.as_raw_fd())?;
        let (last, complete) = args.items(|header, item| {
            if let Some(r) = extent_ref(root, inode, header, item) {
                each(r.disk_address);
            }
            Ok(())
        })?;
        Ok(match last {
            Some(last) if !complete => last.offset.checked_add(1),
            _ => None,
        })
    }

    /// A search buffer to use until it is dropped. Reusing them matters: a
    /// fresh buffer this size is mapped anew, and every page of it faults in
    /// again as the kernel fills it.
    fn searchargs(&self) -> SearchArgsGuard<'_> {
        let args = self.searchargs.lock().unwrap().pop();
        SearchArgsGuard {
            pool: &self.searchargs,
            args: Some(args.unwrap_or_else(|| SearchArgs::new(0))),
        }
    }

    /// The data extent at `address`, read from the extent tree on its own
    /// rather than in a batch with its neighbours, or `None` if there is no
    /// data extent there any more, or only one allocated after `generation`.
    pub fn extent_at(&self, address: u64, generation: u64) -> io::Result<Option<Unresolved>> {
        let Some((item, backrefs)) = self.tally(address)?.finish() else {
            return Ok(None);
        };
        Ok((item.generation <= generation).then_some(Unresolved {
            disk_address: address,
            item,
            backrefs,
        }))
    }

    /// Writes out everything pending and commits the running transaction, as
    /// `syncfs` does on btrfs.
    pub fn sync(&self) -> io::Result<()> {
        if unsafe { syncfs(self.dir.as_raw_fd()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// The filesystem's current generation: the transaction last committed,
    /// or the one running.
    pub fn generation(&self) -> io::Result<u64> {
        let mut args: btrfs_ioctl_fs_info_args = unsafe { std::mem::zeroed() };
        args.flags = BTRFS_FS_INFO_FLAG_GENERATION as u64;
        let rc = unsafe {
            ioctl(
                self.dir.as_raw_fd(),
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

    /// Bytes held by data extents, as the block groups account for them: what
    /// the walk will add up to once it has read every extent. Logical bytes,
    /// counted once however many copies the profile keeps.
    ///
    /// `None` on a filesystem with mixed block groups (`mkfs.btrfs -M`), where
    /// data and metadata share the same space and the used bytes cannot be told
    /// apart.
    pub fn data_used(&self) -> io::Result<Option<u64>> {
        let mut args: SpaceArgs = unsafe { std::mem::zeroed() };
        args.args.space_slots = args.spaces.len() as u64;
        let rc = unsafe {
            ioctl(
                self.dir.as_raw_fd(),
                BTRFS_IOC_SPACE_INFO as c_ulong,
                (&raw mut args).cast(),
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        let filled = (args.args.total_spaces as usize).min(args.spaces.len());
        let spaces = &args.spaces[..filled];
        let (data, metadata) = (
            BTRFS_BLOCK_GROUP_DATA as u64,
            BTRFS_BLOCK_GROUP_METADATA as u64,
        );
        if spaces
            .iter()
            .any(|space| space.flags & data != 0 && space.flags & metadata != 0)
        {
            return Ok(None);
        }
        Ok(Some(
            spaces
                .iter()
                .filter(|space| space.flags & data != 0)
                .map(|space| space.used_bytes)
                .sum(),
        ))
    }

    /// The data extents from address `from` on, as many as one search returns,
    /// with the address the next batch starts from, or `None` once the extent
    /// tree is read to its end. Extents allocated after `generation` are left
    /// out.
    pub fn extent_batch(
        &self,
        from: u64,
        generation: u64,
    ) -> io::Result<(Vec<Unresolved>, Option<u64>)> {
        self.last_snapshots.lock().unwrap().clear();

        let mut args = self.searchargs();
        args.key.tree_id = BTRFS_EXTENT_TREE_OBJECTID as u64;
        args.set_range(
            (from, BTRFS_EXTENT_ITEM_KEY, 0),
            (u64::MAX, BTRFS_SHARED_DATA_REF_KEY, u64::MAX),
        );
        args.search(self.dir.as_raw_fd())?;
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
        let mut args = self.searchargs();
        args.key.tree_id = BTRFS_EXTENT_TREE_OBJECTID as u64;
        args.set_range(
            (address, BTRFS_EXTENT_ITEM_KEY, 0),
            (address, BTRFS_SHARED_DATA_REF_KEY, u64::MAX),
        );
        let mut tally = Tally::default();
        args.search_all(self.dir.as_raw_fd(), |header, item| {
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
    ///
    /// The backreference walk is skipped too for an extent the references the
    /// extent tree names already cover whole. Every other holder can only cover
    /// more of it, so it is fully used whoever they are, and with nothing to
    /// free nothing acts on it. Its references are then not every one there is,
    /// only enough to show it is fully used. On a filesystem with snapshots,
    /// that is most extents, each spared a backreference walk.
    pub fn resolve(&self, extent: Unresolved) -> io::Result<Extent> {
        let Unresolved {
            disk_address,
            item,
            backrefs,
        } = extent;
        if let Backrefs::Files(data_refs) = backrefs
            && let Ok(refs) = self.collect(disk_address, item.disk_bytes, &data_refs)
        {
            let extent = Extent::from_refs(refs);
            if extent.disk_free_bytes() == 0 || !self.may_be_shared(&extent.refs)? {
                return Ok(extent);
            }
        }
        let refs = self.collect(
            disk_address,
            item.disk_bytes,
            &self.logical_ino(disk_address)?,
        )?;
        Ok(Extent::from_refs(refs))
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
    pub fn resolve_again(&self, extent: &Extent) -> io::Result<Extent> {
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
                return Err(changed("changed while it was being read"));
            }
        }
        if refs.is_empty() {
            return Err(changed("no longer referenced"));
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

        let mut args = self.searchargs();
        args.key.tree_id = data_ref.root;
        args.set_range(
            (data_ref.inode, BTRFS_EXTENT_DATA_KEY, start),
            (data_ref.inode, BTRFS_EXTENT_DATA_KEY, last),
        );

        search_extent_refs(
            &mut args,
            self.dir.as_raw_fd(),
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

    /// Whether inode `inode` of subvolume `root` has a file extent item at file
    /// offsets `from..=to` pointing into the extent at `disk_address`.
    pub fn inode_holds_extent(
        &self,
        root: u64,
        inode: u64,
        from: u64,
        to: u64,
        disk_address: u64,
    ) -> io::Result<bool> {
        let mut args = self.searchargs();
        args.key.tree_id = root;
        args.set_range(
            (inode, BTRFS_EXTENT_DATA_KEY, from),
            (inode, BTRFS_EXTENT_DATA_KEY, to),
        );
        let mut found = false;
        search_extent_refs(&mut args, self.dir.as_raw_fd(), root, inode, |r| {
            found |= r.disk_address == disk_address;
        })?;
        Ok(found)
    }

    /// The generation in which subvolume `root` was last snapshotted, or was
    /// last made from a snapshot, from its root item. A tree block no newer
    /// than this may be shared with another tree; one written since cannot be.
    fn last_snapshot(&self, root: u64) -> io::Result<u64> {
        if let Some(&generation) = self.last_snapshots.lock().unwrap().get(&root) {
            return Ok(generation);
        }
        const AT: usize = std::mem::offset_of!(btrfs_root_item, last_snapshot);
        let mut args = self.searchargs();
        args.key.tree_id = BTRFS_ROOT_TREE_OBJECTID as u64;
        args.set_range(
            (root, BTRFS_ROOT_ITEM_KEY, 0),
            (root, BTRFS_ROOT_ITEM_KEY, u64::MAX),
        );
        let mut found = None;
        args.search_all(self.dir.as_raw_fd(), |_, item| {
            found = read_struct::<u64>(item, AT);
            Ok(())
        })?;
        let generation =
            found.ok_or_else(|| io::Error::other(format!("subvolume {root} has no root item")))?;
        self.last_snapshots.lock().unwrap().insert(root, generation);
        Ok(generation)
    }

    /// Every file extent item referencing the extent at `disk_address`, found
    /// by the kernel's backreference walk, which follows shared tree blocks up
    /// to the subvolumes holding them. Gathered into the same shape the extent
    /// tree gives when it names the files itself.
    fn logical_ino(&self, disk_address: u64) -> io::Result<Vec<DataRef>> {
        // Every lookup starts small, and grows only for the rare extent with
        // more references than fit: the kernel zeroes and copies as much as it
        // is told it has. The buffer is `u64`s, and `struct
        // btrfs_data_container`'s four `u32`s fill the first two.
        let mut size = LOGICAL_INO_START_SIZE;
        loop {
            let mut buf = vec![0u64; size.div_ceil(size_of::<u64>())];
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
                    self.dir.as_raw_fd(),
                    BTRFS_IOC_LOGICAL_INO_V2 as c_ulong,
                    (&raw mut args).cast(),
                )
            };
            if rc < 0 {
                let error = io::Error::last_os_error();
                // The kernel finds no extent at the address: it was freed.
                if error.kind() == io::ErrorKind::NotFound {
                    return Err(changed("no longer referenced"));
                }
                return Err(error);
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
    fn subvolume(&self, root: u64) -> io::Result<Option<Arc<Subvolume>>> {
        if let Some(known) = self.subvolumes.lock().unwrap().get(&root) {
            return Ok(known.clone());
        }
        // Not locked while looking: finding a subvolume finds its parent
        // first. Threads looking for the same one at once find the same path.
        let found = self.find_subvolume(root)?.map(Arc::new);
        self.subvolumes.lock().unwrap().insert(root, found.clone());
        Ok(found)
    }

    /// Finds the path to subvolume `root` the way `btrfs subvolume list` does:
    /// its root backreference names the subvolume it sits in, the directory
    /// there, and its own name in that directory. `None` for a subvolume with
    /// no backreference, which is one being deleted.
    fn find_subvolume(&self, root: u64) -> io::Result<Option<Subvolume>> {
        let path = if root == BTRFS_FS_TREE_OBJECTID as u64 {
            PathBuf::new()
        } else {
            let Some((parent, dirid, name)) = self.root_backref(root)? else {
                return Ok(None);
            };
            let Some(parent_subvolume) = self.subvolume(parent)? else {
                return Ok(None);
            };
            let dir = ino_lookup(self.dir.as_raw_fd(), parent, dirid)?;
            parent_subvolume.path.join(dir).join(name)
        };
        Ok(Some(Subvolume { path }))
    }

    /// Where subvolume `root` sits: the subvolume holding it, the directory
    /// there, and its name in that directory.
    fn root_backref(&self, root: u64) -> io::Result<Option<(u64, u64, PathBuf)>> {
        let mut args = self.searchargs();
        args.key.tree_id = BTRFS_ROOT_TREE_OBJECTID as u64;
        args.set_range(
            (root, BTRFS_ROOT_BACKREF_KEY, 0),
            (root, BTRFS_ROOT_BACKREF_KEY, u64::MAX),
        );
        let mut found = None;
        args.search_all(self.dir.as_raw_fd(), |header, item| {
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

    /// The path of inode `inode` in subvolume `root`, relative to the top-level
    /// subvolume, or `None` if the subvolume cannot be reached. An `inode` with
    /// no name any more (an orphan, unlinked but perhaps still open elsewhere)
    /// is a [`io::ErrorKind::NotFound`] error.
    ///
    /// An inode can have more than one name — hardlinks — in which case this
    /// takes the first the kernel returns. Any of them opens the same data,
    /// which is all a rewrite needs.
    pub fn inode_path(&self, root: u64, inode: u64) -> io::Result<Option<PathBuf>> {
        let Some(subvolume) = self.subvolume(root)? else {
            return Ok(None);
        };
        let Some(subvolume_root) = self.open_handle(
            root,
            BTRFS_FIRST_FREE_OBJECTID as u64,
            O_RDONLY | O_DIRECTORY,
        )?
        else {
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

    /// What to call the file at `path`, relative to the top-level subvolume:
    /// where it is under the directory the filesystem was opened through, if it
    /// is below it, or else its path from the top-level subvolume, under
    /// [`FS_TREE`].
    pub fn display_path(&self, path: &Path) -> PathBuf {
        if let Ok(Some(here)) = self.subvolume(self.root)
            && let Ok(rest) = path.strip_prefix(here.path.join(&self.within))
        {
            return self.dir_path.join(rest);
        }
        Path::new(FS_TREE).join(path)
    }

    /// Opens inode `inode` of subvolume `root` read-only, by file handle rather
    /// than by path: a handle names the inode itself, so it reaches a file in
    /// any subvolume, a read-only snapshot's included, and nothing renamed or
    /// mounted over on the way can lead it somewhere else.
    ///
    /// The handle carries the inode's generation, which btrfs checks: an inode
    /// number freed and handed to a new file since it was read is refused
    /// rather than opened. The file opened is still checked to be the inode
    /// asked for.
    ///
    /// Opened `O_NOATIME`, which root may use on any file: the copy reads every
    /// live byte through it, and without the flag those reads move the file's
    /// atime on any mount without `noatime`.
    pub fn open_inode(&self, root: u64, inode: u64) -> io::Result<File> {
        self.open_handle(root, inode, O_RDONLY | O_NOATIME)?
            .ok_or_else(|| io::Error::other("no longer the file the extent tree names"))
    }

    /// Opens inode `inode` of subvolume `root` by handle with `flags`, or
    /// `None` if there is no such inode any more: it was deleted, or its
    /// number now belongs to another.
    fn open_handle(&self, root: u64, inode: u64, flags: u32) -> io::Result<Option<File>> {
        let Some(generation) = self.inode_generation(root, inode)? else {
            return Ok(None);
        };
        let file = match open_by_handle(self.dir.as_raw_fd(), root, inode, generation, flags) {
            Ok(file) => file,
            Err(e) if e.raw_os_error() == Some(ESTALE as i32) => return Ok(None),
            Err(e) => return Err(e),
        };
        if file.metadata()?.ino() != inode || own_root_id(file.as_raw_fd())? != root {
            return Ok(None);
        }
        Ok(Some(file))
    }

    /// The generation inode `inode` of subvolume `root` was created in, from
    /// its inode item, or `None` if it has none: it was deleted.
    fn inode_generation(&self, root: u64, inode: u64) -> io::Result<Option<u64>> {
        const AT: usize = std::mem::offset_of!(btrfs_inode_item, generation);
        let mut args = self.searchargs();
        args.key.tree_id = root;
        args.set_range(
            (inode, BTRFS_INODE_ITEM_KEY, 0),
            (inode, BTRFS_INODE_ITEM_KEY, u64::MAX),
        );
        let mut found = None;
        args.search_all(self.dir.as_raw_fd(), |_, item| {
            found = read_struct::<u64>(item, AT);
            Ok(())
        })?;
        Ok(found)
    }

    /// An unnamed file in the root directory of the subvolume the filesystem
    /// was opened through, for holding a copy while files are rewritten. It has
    /// no link from the moment it exists, so the kernel frees it when the last
    /// descriptor closes however the process ends, a kill included: there is
    /// no window in which a crash could strand it.
    ///
    /// The directory itself is not modified. No directory entry is ever made,
    /// so not even its mtime moves.
    ///
    /// The file takes its attributes from the directory, nodatacow included,
    /// and btrfs refuses to dedupe between two inodes that disagree about
    /// checksums. The flag can still be cleared while the file is empty, so it
    /// is.
    pub fn temp_file(&self) -> io::Result<File> {
        let file = open_beneath(
            self.subvolume_root.as_raw_fd(),
            Path::new(""),
            O_RDWR | O_TMPFILE,
        )?;
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
}

/// Why an extent could not be resolved, when the reason is that it changed
/// under us: freed, or its references moved, between reading it and looking
/// up who holds it. On a filesystem in use this is routine, unlike a lookup
/// that failed.
#[derive(Debug)]
struct Changed(&'static str);

impl fmt::Display for Changed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Changed {}

fn changed(why: &'static str) -> io::Error {
    io::Error::other(Changed(why))
}

/// Whether `error` says only that the extent changed under us.
pub fn is_changed(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|e| e.is::<Changed>())
}

/// What to do about `error`, if it means the filesystem cannot take any more
/// rewrites: it is out of space, has gone read-only, or is failing. Every
/// extent after this one would fail the same way, and pressing on with a full
/// or damaged filesystem only makes it worse. `None` for an error that
/// concerns the extent at hand alone.
pub fn fatal(error: &io::Error) -> Option<&'static str> {
    let errno = error.raw_os_error()? as u32;
    match errno {
        ENOSPC => Some("the filesystem is out of space for the copies"),
        EDQUOT => Some("a quota leaves no room for the copies"),
        EROFS => Some("the filesystem went read-only, see the kernel log"),
        EIO | EUCLEAN => Some("the filesystem is failing, see the kernel log and run a scrub"),
        _ => None,
    }
}

/// Looks `objectid` up in subvolume `treeid`, or in the subvolume `fd` is in if
/// `treeid` is zero. The kernel fills in the tree id it looked in, and the path
/// of `objectid` from that subvolume's root.
fn ino_lookup(fd: c_int, treeid: u64, objectid: u64) -> io::Result<PathBuf> {
    ino_lookup_args(fd, treeid, objectid).map(|args| {
        // `name` is `c_char`: `i8` on x86_64, but already `u8` on aarch64.
        #[allow(clippy::unnecessary_cast)]
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

/// The flags of the subvolume whose root directory `fd` is, which say whether
/// it is read-only.
fn subvolume_flags(fd: c_int) -> io::Result<u64> {
    let mut flags: u64 = 0;
    let rc = unsafe {
        ioctl(
            fd,
            BTRFS_IOC_SUBVOL_GETFLAGS as c_ulong,
            (&raw mut flags).cast(),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(flags)
}

/// The subvolume tree id `fd` itself belongs to.
fn own_root_id(fd: c_int) -> io::Result<u64> {
    Ok(ino_lookup_args(fd, 0, BTRFS_FIRST_FREE_OBJECTID as u64)?.treeid)
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
        if let Some(r) = extent_ref(root, inode, header, item) {
            each(r);
        }
        Ok(())
    })
}

/// The reference to an extent that file extent item `item` of inode `inode`
/// in subvolume `root` is, found with `header`. `None` for a hole or an inline
/// extent, which occupy no extent of their own.
fn extent_ref(
    root: u64,
    inode: u64,
    header: &btrfs_ioctl_search_header,
    item: &[u8],
) -> Option<ExtentRef> {
    // An item too short for `btrfs_file_extent_item` is a shape we do not
    // understand; skipping it loses one reference, not the file.
    let fe = read_struct::<btrfs_file_extent_item>(item, 0)?;
    if fe.type_ as u32 == BTRFS_FILE_EXTENT_INLINE as u32 || fe.disk_bytenr == 0 {
        return None;
    }
    Some(ExtentRef {
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
    })
}

/// How many columns wide the terminal `file` is on, or `None` if it is not a
/// terminal or does not say.
pub fn terminal_width(file: &impl AsRawFd) -> Option<usize> {
    let mut size: winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        ioctl(
            file.as_raw_fd(),
            TIOCGWINSZ as c_ulong,
            (&raw mut size).cast(),
        )
    };
    (rc == 0 && size.ws_col > 0).then_some(size.ws_col as usize)
}

/// `struct btrfs_ioctl_space_args` with its trailing `spaces` array given a
/// fixed size. There is one entry per kind of block group and profile, a few
/// on any filesystem, more only while a balance converts between profiles.
#[repr(C)]
struct SpaceArgs {
    args: btrfs_ioctl_space_args,
    spaces: [btrfs_ioctl_space_info; 64],
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
/// Returns the bytes the kernel reports deduped. That is `len` whenever it
/// succeeds, even if it quietly moved less, so it proves nothing on its own.
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
    // The call as a whole succeeds even when the dedupe into `dest` fails: a
    // failure there, running out of space included, is a negative errno in
    // its status.
    if args.info.status < 0 {
        return Err(io::Error::from_raw_os_error(-args.info.status));
    }
    if args.info.status != FILE_DEDUPE_RANGE_SAME as i32 {
        return Err(io::Error::other(format!(
            "dedupe reported status {} at offset {dest_offset}",
            args.info.status
        )));
    }
    Ok(args.info.bytes_deduped)
}
