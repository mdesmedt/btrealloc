use std::cell::RefCell;
use std::ffi::{c_int, c_ulong, c_void};
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::ptr;

use linux_raw_sys::btrfs::{
    BTRFS_EXTENT_DATA_KEY, BTRFS_EXTENT_ITEM_KEY, BTRFS_EXTENT_TREE_OBJECTID,
    BTRFS_FILE_EXTENT_INLINE, btrfs_extent_item, btrfs_file_extent_item, btrfs_ioctl_search_header,
    btrfs_ioctl_search_key,
};
use linux_raw_sys::general::{
    __IncompleteArrayField, BTRFS_SUPER_MAGIC, FILE_DEDUPE_RANGE_SAME, FS_NOCOW_FL, O_TMPFILE,
    file_dedupe_range, file_dedupe_range_info, statfs,
};
use linux_raw_sys::ioctl::{BTRFS_IOC_TREE_SEARCH_V2, FIDEDUPERANGE, FS_IOC_GETFLAGS};

const BUF_SIZE: usize = 64 * 1024;

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

        let searchargs = SearchArgs::new(BTRFS_EXTENT_TREE_OBJECTID as u64);
        Ok(Filesystem {
            file,
            sectorsize,
            searchargs: RefCell::new(Box::new(searchargs)),
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
}

/// One reference from a file into a physical extent.
pub struct ExtentRef {
    /// Physical address of the extent, i.e. its identity.
    pub disk_address: u64,
    /// Full on-disk size of the extent, however much of it is referenced.
    pub disk_bytes: u64,
    /// Uncompressed size of the extent. Equals `disk_bytes` when it is not
    /// compressed.
    pub uncompressed_bytes: u64,
    /// Where in the file this reference starts.
    pub file_offset: u64,
    /// Where inside the extent this reference starts.
    pub extent_offset: u64,
    /// Uncompressed bytes of the extent this reference uses.
    pub num_bytes: u64,
    /// The holding file is nodatacow, which rules out rewriting it: dedupe
    /// does not work on those, and overwrites go in place anyway.
    pub nocow: bool,
}

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
