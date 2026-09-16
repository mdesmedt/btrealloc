use std::cell::RefCell;
use std::collections::HashSet;
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
    BTRFS_EXTENT_DATA_KEY, BTRFS_EXTENT_ITEM_KEY, BTRFS_EXTENT_TREE_OBJECTID,
    BTRFS_FILE_EXTENT_INLINE, BTRFS_FIRST_FREE_OBJECTID, BTRFS_LOGICAL_INO_ARGS_IGNORE_OFFSET,
    btrfs_extent_item, btrfs_file_extent_item, btrfs_ioctl_ino_lookup_args,
    btrfs_ioctl_ino_path_args, btrfs_ioctl_logical_ino_args, btrfs_ioctl_search_header,
    btrfs_ioctl_search_key,
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

const BUF_SIZE: usize = 64 * 1024;

/// A zeroed `Box<T>`, allocated straight on the heap: `Box::new(x)` builds `x`
/// on the stack first, which overflows it for something [`LogicalInoBuf`]-sized.
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
    fn new(tree_id: u64) -> SearchArgs {
        SearchArgs {
            key: btrfs_ioctl_search_key {
                tree_id,
                min_objectid: 0,
                max_objectid: u64::MAX,
                min_offset: 0,
                max_offset: u64::MAX,
                min_transid: 0,
                max_transid: u64::MAX,
                min_type: 0,
                max_type: u32::MAX,
                nr_items: u32::MAX,
                unused: 0,
                unused1: 0,
                unused2: 0,
                unused3: 0,
                unused4: 0,
            },
            buf_size: BUF_SIZE as u64,
            buf: [0; BUF_SIZE],
        }
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
}

/// A handle on the mounted filesystem, for questions about it as a whole
/// rather than about one file.
pub struct Filesystem {
    file: File,
    /// The sector size: a file extent reference covers whole sectors, never
    /// part of one. It is the page size at mkfs time, typically 4096 bytes.
    pub sectorsize: u64,
    /// One search buffer, reused in [`Filesystem::extent_refs`]
    searchargs: RefCell<Box<SearchArgs>>,
    /// One backreference buffer, reused in [`Filesystem::logical_ino`].
    logicalino: RefCell<Box<LogicalInoBuf>>,
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

        let searchargs = SearchArgs::new(BTRFS_EXTENT_TREE_OBJECTID as u64);
        Ok(Filesystem {
            file,
            sectorsize,
            searchargs: RefCell::new(Box::new(searchargs)),
            logicalino: RefCell::new(boxed_zeroed()),
            root_id,
            subvol_root,
        })
    }

    /// How many references the whole filesystem holds to this extent, counting
    /// parts of the tree we never walked. `None` if it is not in the extent tree,
    /// meaning it was freed while we scanned.
    pub fn extent_refs(&self, disk_address: u64, disk_bytes: u64) -> io::Result<Option<u64>> {
        let mut args = self.searchargs.borrow_mut();
        args.key.min_objectid = disk_address;
        args.key.max_objectid = disk_address;
        args.key.min_type = BTRFS_EXTENT_ITEM_KEY;
        args.key.max_type = BTRFS_EXTENT_ITEM_KEY;
        args.key.min_offset = disk_bytes;
        args.key.max_offset = disk_bytes;
        args.search(self.file.as_raw_fd())?;

        if args.key.nr_items == 0 {
            return Ok(None);
        }
        // `struct btrfs_extent_item` starts with its reference count.
        read_struct::<btrfs_extent_item>(&args.buf, size_of::<btrfs_ioctl_search_header>())
            .map(|item| Some(item.refs))
            .ok_or_else(|| io::Error::other("extent item is too short to hold a reference count"))
    }

    /// Every reference the filesystem holds to the extent at `disk_address`,
    /// resolved down to the file and byte range each one is at.
    ///
    /// `Ok(None)` means at least one of them belongs to an inode we cannot
    /// resolve to a path on this handle: most often a snapshot, whose inodes
    /// live in a different subvolume tree than the one opened.
    pub fn all_refs(&self, disk_address: u64) -> io::Result<Option<Vec<ExtentRef>>> {
        let owners = self.logical_ino(disk_address)?;

        let mut refs = Vec::new();
        for (root, inum) in owners {
            if root != self.root_id {
                return Ok(None);
            }
            let Some(path) = self.resolve_path(inum)? else {
                return Ok(None);
            };
            for r in file_extents(&path)? {
                if r.disk_address == disk_address {
                    refs.push(r);
                }
            }
        }
        Ok(Some(refs))
    }

    /// Every (root, inode) pair the extent tree records a backreference to
    /// `disk_address` for, however much of the extent that reference covers.
    fn logical_ino(&self, disk_address: u64) -> io::Result<HashSet<(u64, u64)>> {
        let mut buf = self.logicalino.borrow_mut();
        let mut args = btrfs_ioctl_logical_ino_args {
            logical: disk_address,
            size: size_of::<LogicalInoBuf>() as u64,
            reserved: [0; 3],
            // Without this flag the kernel only returns backreferences whose
            // own reference starts at `disk_address`, i.e. at the very start of
            // the extent. We want every reference into it, wherever in the
            // extent it starts.
            flags: BTRFS_LOGICAL_INO_ARGS_IGNORE_OFFSET as u64,
            // `buf` is `RefMut<Box<LogicalInoBuf>>`: `*buf` only strips the
            // `RefMut`, landing on the `Box` itself, not what it points at.
            inodes: (&raw mut **buf).cast::<c_void>() as u64,
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
        if buf.bytes_missing > 0 {
            return Err(io::Error::other(
                "extent has more backreferences than fit in one lookup",
            ));
        }

        let count = buf.elem_cnt as usize / 3;
        let mut owners = HashSet::with_capacity(count);
        for i in 0..count {
            // Each result is a (inode, offset, root) triple; the offset is not
            // useful here since resolving the path lets us re-read the real
            // extent references straight from the file itself.
            let inum = buf.val[i * 3];
            let root = buf.val[i * 3 + 2];
            owners.insert((root, inum));
        }
        Ok(owners)
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

/// Room for the offset table and path bytes together: 64KiB, the same buffer
/// size the tree search above uses.
const INO_PATH_DATA_SIZE: usize = BUF_SIZE - 16;

/// `struct btrfs_data_container` with its trailing `val` array given a fixed
/// size, which is the layout [`Filesystem::logical_ino`] gives the kernel to
/// fill in. Kept once per [`Filesystem`] and reused, rather than allocated
/// fresh per lookup.
#[repr(C)]
struct LogicalInoBuf {
    bytes_left: u32,
    bytes_missing: u32,
    elem_cnt: u32,
    elem_missed: u32,
    val: [u64; LOGICAL_INO_ITEMS],
}

/// Room for this many `(inode, offset, root)` triples: a megabyte of `u64`s.
const LOGICAL_INO_ITEMS: usize = (LOGICAL_INO_BUF_SIZE - 16) / size_of::<u64>();

/// Total size of [`LogicalInoBuf`], header included.
const LOGICAL_INO_BUF_SIZE: usize = 1024 * 1024;

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
    let fd = file.as_raw_fd();
    let ino = file.metadata()?.ino();
    let nocow = is_nocow(&file)?;

    let mut args = SearchArgs::new(0); // 0 = the tree this fd lives in
    args.key.min_objectid = ino;
    args.key.max_objectid = ino;
    args.key.min_type = BTRFS_EXTENT_DATA_KEY;
    args.key.max_type = BTRFS_EXTENT_DATA_KEY;

    const HEADER_SIZE: usize = size_of::<btrfs_ioctl_search_header>();

    let mut extents = Vec::new();
    loop {
        args.search(fd)?;
        if args.key.nr_items == 0 {
            return Ok(extents);
        }

        let mut pos = 0usize;
        let mut last_offset = 0u64;
        for _ in 0..args.key.nr_items {
            let header =
                read_struct::<btrfs_ioctl_search_header>(&args.buf, pos).ok_or_else(|| {
                    io::Error::other("search claimed more items than its buffer holds")
                })?;
            last_offset = header.offset;
            let len = header.len as usize;

            let start = pos + HEADER_SIZE;
            let end = start + len;
            if end > args.buf.len() {
                return Err(io::Error::other("search item runs past its buffer"));
            }
            let item = &args.buf[start..end];
            pos = end;

            // An item too short for `btrfs_file_extent_item` is a shape we do
            // not understand; skipping it loses one reference, not the file.
            let Some(fe) = read_struct::<btrfs_file_extent_item>(item, 0) else {
                continue;
            };
            if fe.type_ as u32 == BTRFS_FILE_EXTENT_INLINE as u32 {
                continue;
            }
            if fe.disk_bytenr == 0 {
                continue; // hole
            }
            extents.push(ExtentRef {
                path: Rc::clone(&path),
                file_offset: last_offset,
                disk_address: fe.disk_bytenr,
                disk_bytes: fe.disk_num_bytes,
                uncompressed_bytes: fe.ram_bytes,
                extent_offset: fe.offset,
                num_bytes: fe.num_bytes,
                nocow,
            });
        }

        if last_offset == u64::MAX {
            return Ok(extents);
        }
        args.key.min_offset = last_offset + 1;
    }
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
