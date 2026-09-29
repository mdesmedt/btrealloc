//! Building throwaway btrfs filesystems to test against, and reading them back.
//!
//! Every test gets its own filesystem: a tmpfs, an image inside it, and a loop
//! mount of that image's top-level subvolume, all torn down when the [`Fs`]
//! goes out of scope. Nothing is shared between tests, so no test depends on
//! what another one applied.
//!
//! The shapes below are the ones btrealloc distinguishes, built with the same
//! operations the kernel sees from `dd`, `fallocate --punch-hole`,
//! `cp --reflink` and `chattr +C`, done here as plain syscalls.
//!
//! What the tests read back about a file's extents, they read with their own
//! code rather than the tool's: the two drifting apart then shows up as a
//! failure rather than as agreement.

#![allow(dead_code)] // each test uses a different corner of this

use std::collections::{BTreeMap, HashMap};
use std::ffi::{c_int, c_ulong, c_void};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use linux_raw_sys::btrfs::{
    BTRFS_EXTENT_DATA_KEY, BTRFS_FILE_EXTENT_INLINE, BTRFS_FIRST_FREE_OBJECTID,
    btrfs_file_extent_item, btrfs_ioctl_ino_lookup_args, btrfs_ioctl_search_header,
    btrfs_ioctl_search_key,
};
use linux_raw_sys::general::{
    FALLOC_FL_KEEP_SIZE, FALLOC_FL_PUNCH_HOLE, FS_NOCOW_FL, RLIMIT_NOFILE, rlimit, statfs,
};
use linux_raw_sys::ioctl::{
    BTRFS_IOC_INO_LOOKUP, BTRFS_IOC_TREE_SEARCH_V2, FICLONE, FS_IOC_GETFLAGS, FS_IOC_SETFLAGS,
};
use sha2::{Digest, Sha256};

use btrealloc::Options;
use btrealloc::extent::Extent;
use btrealloc::kernel;
use btrealloc::run::Report;
use btrealloc::scan::{ScanStats, Scanner, Totals};

pub const MIB: u64 = 1 << 20;

unsafe extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, arg: *mut c_void) -> c_int;
    fn fallocate(fd: c_int, mode: c_int, offset: i64, len: i64) -> c_int;
    fn fstatfs(fd: c_int, buf: *mut statfs) -> c_int;
    fn sync();
    fn getrlimit(resource: c_int, rlim: *mut rlimit) -> c_int;
    fn setrlimit(resource: c_int, rlim: *const rlimit) -> c_int;
}

/// Runs a command, or fails the test with everything it said.
fn must(program: &str, args: &[&str]) {
    let output = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{program}: {e}"));
    assert!(
        output.status.success(),
        "{program} {}: {}\n{}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
}

/// A throwaway btrfs filesystem, in a tmpfs of its own so it never touches a
/// real disk, unmounted and removed when this is dropped.
pub struct Fs {
    base: PathBuf,
    mnt: PathBuf,
}

impl Fs {
    /// A filesystem big enough for a handful of the shapes below.
    pub fn new() -> Fs {
        Fs::with_options(2048, "")
    }

    /// `size_mib` of tmpfs, and `mountopts` passed on to the btrfs mount on top
    /// of the loop option.
    pub fn with_options(size_mib: u64, mountopts: &str) -> Fs {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "btrealloc-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        let tmpfs = base.join("tmpfs");
        let mnt = base.join("mnt");
        std::fs::create_dir_all(&tmpfs).expect("make the tmpfs directory");
        std::fs::create_dir_all(&mnt).expect("make the mount point");

        let size = format!("size={size_mib}M");
        must(
            "mount",
            &["-t", "tmpfs", "-o", &size, "tmpfs", tmpfs.to_str().unwrap()],
        );
        let fs = Fs { base, mnt };

        let img = tmpfs.join("btrfs.img");
        File::create(&img)
            .and_then(|f| f.set_len(size_mib * MIB))
            .expect("make the image file");
        must("mkfs.btrfs", &["-q", img.to_str().unwrap()]);

        // The top-level subvolume, as btrealloc expects to be given.
        let opts = match mountopts {
            "" => "loop,subvolid=5".to_string(),
            extra => format!("loop,subvolid=5,{extra}"),
        };
        must(
            "mount",
            &["-o", &opts, img.to_str().unwrap(), fs.mnt.to_str().unwrap()],
        );
        fs
    }

    /// The mount point itself: everything a run could have touched is below it.
    pub fn root(&self) -> &Path {
        &self.mnt
    }

    /// The tmpfs the filesystem's image lives in, which is not btrfs.
    pub fn tmpfs(&self) -> PathBuf {
        self.base.join("tmpfs")
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.mnt.join(rel)
    }

    /// The directory at `rel`, made along with any parents.
    pub fn dir(&self, rel: &str) -> PathBuf {
        let dir = self.path(rel);
        std::fs::create_dir_all(&dir).expect("make a directory");
        dir
    }

    /// The sector size mkfs picked for this filesystem, read back rather than
    /// assumed: it is the page size at mkfs time, which is not 4096 on every
    /// architecture.
    pub fn sectorsize(&self) -> u64 {
        sectorsize(&self.mnt)
    }

    /// The loop device the filesystem is on.
    fn device(&self) -> String {
        let source = Command::new("findmnt")
            .args(["-no", "SOURCE", self.mnt.to_str().unwrap()])
            .output()
            .expect("run findmnt");
        let source = String::from_utf8_lossy(&source.stdout);
        // findmnt names a mount of a subvolume other than the top-level one as
        // `device[/subvolume]`.
        source.trim().split('[').next().unwrap().to_string()
    }

    /// The subvolume at `rel`, made new.
    pub fn subvolume(&self, rel: &str) -> PathBuf {
        let path = self.path(rel);
        must("btrfs", &["subvolume", "create", path.to_str().unwrap()]);
        path
    }

    /// Enough empty files in a new directory at `rel` to spread a subvolume's
    /// tree over leaves below its root. A snapshot copies the root block
    /// outright, so only a tree deeper than that has leaves to share.
    pub fn filler(&self, rel: &str) {
        let dir = self.dir(rel);
        for i in 0..4000 {
            std::fs::write(dir.join(format!("file-{i:05}")), b"").expect("write a filler file");
        }
    }

    /// A read-only snapshot of the subvolume at `from`, as `to`, kept alongside it.
    pub fn snapshot(&self, from: &str, to: &str) -> PathBuf {
        let (from, to) = (self.path(from), self.path(to));
        sync_fs();
        must(
            "btrfs",
            &[
                "subvolume",
                "snapshot",
                "-r",
                from.to_str().unwrap(),
                to.to_str().unwrap(),
            ],
        );
        sync_fs();
        to
    }

    /// Deletes the subvolume at `rel`, and waits for the deletion to be cleaned
    /// up.
    ///
    /// The cleaner otherwise sleeps until the next periodic commit, 30 seconds
    /// away by default. A filesystem sync wakes it, where a plain `sync` does not.
    pub fn delete_subvolume(&self, rel: &str) {
        let path = self.path(rel);
        must("btrfs", &["subvolume", "delete", path.to_str().unwrap()]);
        must("btrfs", &["filesystem", "sync", self.mnt.to_str().unwrap()]);
        must("btrfs", &["subvolume", "sync", self.mnt.to_str().unwrap()]);
        sync_fs();
    }

    /// Snapshots the subvolume at `from` as `to`, then deletes `from` and waits
    /// for the deletion to be cleaned up.
    ///
    /// Every tree block the two still shared is left to the snapshot, and the
    /// cleaner rewrites the references in it to name the block rather than the
    /// subvolume: the data extents those leaves point at come out with shared
    /// data backreferences, as on any filesystem with snapshot history.
    pub fn snapshot_and_delete(&self, from: &str, to: &str) -> PathBuf {
        let path = self.path(to);
        sync_fs();
        must(
            "btrfs",
            &[
                "subvolume",
                "snapshot",
                self.path(from).to_str().unwrap(),
                path.to_str().unwrap(),
            ],
        );
        self.delete_subvolume(from);
        path
    }

    /// Whether the subvolume at `rel` is read-only.
    pub fn is_readonly(&self, rel: &str) -> bool {
        let output = Command::new("btrfs")
            .args([
                "property",
                "get",
                "-ts",
                self.path(rel).to_str().unwrap(),
                "ro",
            ])
            .output()
            .expect("run btrfs property get");
        String::from_utf8_lossy(&output.stdout).trim() == "ro=true"
    }

    /// The subvolume at `rel` mounted again on its own, somewhere outside this
    /// mount, as a system mounts its root or home subvolume.
    pub fn mount_subvolume(&self, rel: &str) -> Mount {
        let path = self.base.join(format!("subvol-{}", rel.replace('/', "-")));
        std::fs::create_dir_all(&path).expect("make the mount point");
        must(
            "mount",
            &[
                "-o",
                &format!("subvol={rel}"),
                &self.device(),
                path.to_str().unwrap(),
            ],
        );
        Mount { path }
    }

    /// The top-level subvolume mounted again, read-only.
    pub fn mount_readonly(&self) -> Mount {
        let path = self.base.join("readonly");
        std::fs::create_dir_all(&path).expect("make the mount point");
        must(
            "mount",
            &["--bind", self.mnt.to_str().unwrap(), path.to_str().unwrap()],
        );
        let mount = Mount { path };
        must(
            "mount",
            &["-o", "remount,bind,ro", mount.path.to_str().unwrap()],
        );
        mount
    }

    /// How many shared data backreferences the extent tree holds, read from
    /// the device with `btrfs inspect-internal dump-tree`.
    pub fn shared_data_backrefs(&self) -> usize {
        sync_fs();
        let dump = Command::new("btrfs")
            .args([
                "inspect-internal",
                "dump-tree",
                "-t",
                "extent",
                &self.device(),
            ])
            .output()
            .expect("run btrfs inspect-internal dump-tree");
        assert!(
            dump.status.success(),
            "dump-tree failed: {}",
            String::from_utf8_lossy(&dump.stderr)
        );
        String::from_utf8_lossy(&dump.stdout)
            .lines()
            .filter(|line| line.contains("shared data backref"))
            .count()
    }
}

impl Drop for Fs {
    fn drop(&mut self) {
        // Best effort: a failure here must not mask the failure of a test.
        let _ = Command::new("umount").arg(&self.mnt).status();
        let _ = Command::new("umount").arg(self.base.join("tmpfs")).status();
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// A second mount of an [`Fs`], unmounted when this is dropped.
pub struct Mount {
    pub path: PathBuf,
}

impl Drop for Mount {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(&self.path).status();
    }
}

/// The sector size of the btrfs filesystem `path` is on, which it reports as
/// its block size.
pub fn sectorsize(path: &Path) -> u64 {
    let file = File::open(path).expect("open a path to statfs");
    let mut buf: statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { fstatfs(file.as_raw_fd(), &raw mut buf) };
    assert!(rc == 0, "statfs: {}", std::io::Error::last_os_error());
    buf.f_bsize as u64
}

/// `mib` MiB of incompressible data, so compressed and uncompressed runs
/// allocate the same extents.
pub fn write_random(path: &Path, mib: u64) {
    let mut urandom = File::open("/dev/urandom").expect("open /dev/urandom");
    let mut file = File::create(path).expect("create the file");
    let mut buf = vec![0u8; MIB as usize];
    for _ in 0..mib {
        urandom.read_exact(&mut buf).expect("read /dev/urandom");
        file.write_all(&buf).expect("write the file");
    }
    file.sync_all().expect("sync the file");
    sync_fs();
}

/// `mib` MiB of incompressible data guaranteed to land in a single extent, for
/// fixtures that need one of a specific minimum size rather than however many
/// pieces a plain write happens to split into.
///
/// `fallocate` reserves the whole range as one on-disk extent before anything
/// is written. Writing into a never-before-written (prealloc) region needs no
/// copy-on-write, so the kernel fills the reservation in place instead of
/// however writeback timing happens to chop up a plain buffered write under
/// load or emulation.
pub fn write_random_one_extent(path: &Path, mib: u64) {
    let mut urandom = File::open("/dev/urandom").expect("open /dev/urandom");
    let file = File::create(path).expect("create the file");
    let rc = unsafe { fallocate(file.as_raw_fd(), 0, 0, (mib * MIB) as i64) };
    assert!(rc == 0, "fallocate: {}", std::io::Error::last_os_error());

    let mut buf = vec![0u8; MIB as usize];
    for i in 0..mib {
        urandom.read_exact(&mut buf).expect("read /dev/urandom");
        file.write_all_at(&buf, i * MIB).expect("write the file");
    }
    file.sync_all().expect("sync the file");
    sync_fs();
}

/// `mib` MiB that zstd will squeeze, for the compressed-mount case.
pub fn write_compressible(path: &Path, mib: u64) {
    let mut file = File::create(path).expect("create the file");
    let buf = vec![b'a'; MIB as usize];
    for _ in 0..mib {
        file.write_all(&buf).expect("write the file");
    }
    file.sync_all().expect("sync the file");
    sync_fs();
}

/// Drops a stretch of the file. The extent stays; only this file's reference to
/// that part of it goes away.
pub fn punch_hole(path: &Path, offset: u64, len: u64) {
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open the file to punch");
    let mode = (FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE) as c_int;
    let rc = unsafe { fallocate(file.as_raw_fd(), mode, offset as i64, len as i64) };
    assert!(rc == 0, "punch hole: {}", std::io::Error::last_os_error());
    file.sync_all().expect("sync the punched file");
    sync_fs();
}

/// One extent with one live piece at each end: the simplest waste btrealloc sees.
/// The MiB at the start and the MiB at the end stay, the rest is dropped.
pub fn bookend(path: &Path, mib: u64) {
    write_random(path, mib);
    punch_hole(path, MIB, (mib - 2) * MIB);
}

/// A copy sharing the original's extents, as `cp --reflink=always` makes.
/// The two can be in different subvolumes of the same mount.
pub fn reflink(src: &Path, dest: &Path) {
    let src = File::open(src).expect("open the reflink source");
    let dest = File::create(dest).expect("create the reflink destination");
    let rc = unsafe {
        ioctl(
            dest.as_raw_fd(),
            FICLONE as c_ulong,
            src.as_raw_fd() as *mut c_void,
        )
    };
    assert!(rc == 0, "reflink: {}", std::io::Error::last_os_error());
    dest.sync_all().expect("sync the reflink");
    sync_fs();
}

/// Marks a file or directory nodatacow, as `chattr +C` does. On a directory it
/// applies to whatever is created in it afterwards, which is how a datacow file
/// ends up in a nodatacow directory.
pub fn set_nocow(path: &Path) {
    let file = File::open(path).expect("open the file to mark nodatacow");
    let mut flags: c_int = 0;
    let rc = unsafe {
        ioctl(
            file.as_raw_fd(),
            FS_IOC_GETFLAGS as c_ulong,
            (&raw mut flags).cast(),
        )
    };
    assert!(rc == 0, "get flags: {}", std::io::Error::last_os_error());

    flags |= FS_NOCOW_FL as c_int;
    let rc = unsafe {
        ioctl(
            file.as_raw_fd(),
            FS_IOC_SETFLAGS as c_ulong,
            (&raw mut flags).cast(),
        )
    };
    assert!(rc == 0, "set flags: {}", std::io::Error::last_os_error());
}

/// Flushes everything, so the extents the next scan reads are the ones the
/// writes above meant to leave.
pub fn sync_fs() {
    unsafe { sync() };
}

/// Lowers the soft limit on files this process may have open to `soft`, until
/// this is dropped and the limit it replaced comes back.
pub struct OpenFilesLimit {
    previous: rlimit,
}

impl OpenFilesLimit {
    pub fn lower_to(soft: u64) -> OpenFilesLimit {
        let mut previous = rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        let rc = unsafe { getrlimit(RLIMIT_NOFILE as c_int, &mut previous) };
        assert!(rc == 0, "getrlimit: {}", std::io::Error::last_os_error());
        let lowered = rlimit {
            rlim_cur: soft.min(previous.rlim_cur),
            rlim_max: previous.rlim_max,
        };
        let rc = unsafe { setrlimit(RLIMIT_NOFILE as c_int, &lowered) };
        assert!(rc == 0, "setrlimit: {}", std::io::Error::last_os_error());
        OpenFilesLimit { previous }
    }

    /// The soft limit now in force.
    pub fn soft(&self) -> u64 {
        let mut current = rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        let rc = unsafe { getrlimit(RLIMIT_NOFILE as c_int, &mut current) };
        assert!(rc == 0, "getrlimit: {}", std::io::Error::last_os_error());
        current.rlim_cur
    }
}

impl Drop for OpenFilesLimit {
    fn drop(&mut self) {
        unsafe { setrlimit(RLIMIT_NOFILE as c_int, &self.previous) };
    }
}

/// The SHA-256 of every file under `dir`, by path. Replaces the shell suite's
/// `md5sum` pass: contents must never change under a rewrite.
pub fn checksums(dir: &Path) -> BTreeMap<PathBuf, [u8; 32]> {
    let mut sums = BTreeMap::new();
    for path in listing(dir) {
        if path.is_file() {
            let mut file = File::open(&path).expect("open a file to checksum");
            let mut hasher = Sha256::new();
            let mut buf = vec![0u8; MIB as usize];
            loop {
                let n = file.read(&mut buf).expect("read a file to checksum");
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            sums.insert(path, hasher.finalize().into());
        }
    }
    sums
}

/// Every entry under `dir`, sorted, subvolumes and snapshots included. A
/// temporary file left anywhere on the filesystem turns up here, not just one
/// under a name we guessed.
pub fn listing(dir: &Path) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read a directory").flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            entries.push(path);
        }
    }
    entries.sort();
    entries
}

/// One reference a file holds into an extent, as its file extent item records
/// it.
pub struct FileExtent {
    pub disk_address: u64,
    pub disk_bytes: u64,
    pub file_offset: u64,
    pub extent_offset: u64,
    pub num_bytes: u64,
}

/// `struct btrfs_ioctl_search_args_v2` with room for a few hundred items.
#[repr(C)]
struct SearchArgs {
    key: btrfs_ioctl_search_key,
    buf_size: u64,
    buf: [u8; SEARCH_BUF_SIZE],
}

const SEARCH_BUF_SIZE: usize = 64 * 1024;

/// Every file extent item of inode `inode` in subvolume `tree_id`, searched for
/// through `fd`: a tree id of zero means the subvolume `fd` is in. Holes and
/// inline extents are left out; they occupy no extent of their own.
fn search_file_extents(fd: c_int, tree_id: u64, inode: u64) -> Vec<FileExtent> {
    const HEADER_SIZE: usize = size_of::<btrfs_ioctl_search_header>();
    let mut args: Box<SearchArgs> = Box::new(unsafe { std::mem::zeroed() });
    args.key.tree_id = tree_id;
    args.key.min_objectid = inode;
    args.key.max_objectid = inode;
    args.key.min_type = BTRFS_EXTENT_DATA_KEY;
    args.key.max_type = BTRFS_EXTENT_DATA_KEY;
    args.key.max_offset = u64::MAX;
    args.key.max_transid = u64::MAX;
    args.buf_size = SEARCH_BUF_SIZE as u64;

    let mut extents = Vec::new();
    loop {
        args.key.nr_items = u32::MAX;
        let rc = unsafe {
            ioctl(
                fd,
                BTRFS_IOC_TREE_SEARCH_V2 as c_ulong,
                (&raw mut *args).cast(),
            )
        };
        assert!(rc == 0, "tree search: {}", std::io::Error::last_os_error());
        if args.key.nr_items == 0 {
            return extents;
        }

        let mut pos = 0;
        for _ in 0..args.key.nr_items {
            let header: btrfs_ioctl_search_header =
                unsafe { std::ptr::read_unaligned(args.buf[pos..].as_ptr().cast()) };
            let item = &args.buf[pos + HEADER_SIZE..pos + HEADER_SIZE + header.len as usize];
            pos += HEADER_SIZE + header.len as usize;
            // The next search carries on past this item.
            args.key.min_offset = header.offset + 1;

            assert!(item.len() >= size_of::<btrfs_file_extent_item>());
            let fe: btrfs_file_extent_item =
                unsafe { std::ptr::read_unaligned(item.as_ptr().cast()) };
            if fe.type_ as u32 == BTRFS_FILE_EXTENT_INLINE as u32 || fe.disk_bytenr == 0 {
                continue;
            }
            extents.push(FileExtent {
                disk_address: fe.disk_bytenr,
                disk_bytes: fe.disk_num_bytes,
                file_offset: header.offset,
                extent_offset: fe.offset,
                num_bytes: fe.num_bytes,
            });
        }
    }
}

/// Every extent reference the file at `path` holds.
pub fn file_extents(path: &Path) -> Vec<FileExtent> {
    let file = File::open(path).expect("open the file to read its extents");
    let inode = file.metadata().expect("stat the file").ino();
    search_file_extents(file.as_raw_fd(), 0, inode)
}

/// Every extent reference inode `inode` of subvolume `root` holds, read
/// through the top-level subvolume mounted at `mount`, with no path needed.
pub fn file_extents_of(mount: &Path, root: u64, inode: u64) -> Vec<FileExtent> {
    let dir = File::open(mount).expect("open the mount");
    search_file_extents(dir.as_raw_fd(), root, inode)
}

/// The subvolume tree id and inode number of the file at `path`, which is how
/// the tool names the files holding an extent.
pub fn identity(path: &Path) -> (u64, u64) {
    let file = File::open(path).expect("open the file to identify");
    let mut args: btrfs_ioctl_ino_lookup_args = unsafe { std::mem::zeroed() };
    args.objectid = BTRFS_FIRST_FREE_OBJECTID as u64;
    let rc = unsafe {
        ioctl(
            file.as_raw_fd(),
            BTRFS_IOC_INO_LOOKUP as c_ulong,
            (&raw mut args).cast(),
        )
    };
    assert!(rc == 0, "inode lookup: {}", std::io::Error::last_os_error());
    (args.treeid, file.metadata().expect("stat the file").ino())
}

/// The physical addresses of a file's extents. Same answer `filefrag -v` gives,
/// without parsing it: two files at the same address share one copy.
pub fn physical_extents(path: &Path) -> Vec<u64> {
    let mut addresses: Vec<u64> = file_extents(path)
        .iter()
        .map(|extent| extent.disk_address)
        .collect();
    addresses.sort_unstable();
    addresses.dedup();
    addresses
}

/// The one extent a file's data sits in, or a failure which says the fixture
/// did not come out as intended.
///
/// How a write is cut into extents is btrfs's to decide, and a shape which only
/// means something in one extent has to say so: named here, an unexpected
/// layout reads as what it is rather than as a btrealloc bug found further down.
pub fn single_extent(path: &Path) -> u64 {
    let addresses = physical_extents(path);
    assert_eq!(
        addresses.len(),
        1,
        "fixture: {} should hold one extent, btrfs gave it {:?}",
        path.display(),
        addresses,
    );
    addresses[0]
}

/// Whether the file at `path` holds `extent`, going by what the scan found.
fn holds(extent: &Extent, id: (u64, u64)) -> bool {
    extent.refs.iter().any(|r| (r.root, r.inode) == id)
}

/// The extents holding `path`, each with the address it lives at.
pub fn extents_of<'a>(scan: &'a Scan, path: &Path) -> Vec<(u64, &'a Extent)> {
    let id = identity(path);
    let mut extents: Vec<(u64, &Extent)> = scan
        .extents
        .iter()
        .filter(|(_, extent)| holds(extent, id))
        .map(|(&address, extent)| (address, extent))
        .collect();
    extents.sort_by_key(|&(address, _)| address);
    extents
}

/// What the extents holding `path` allocate, and how much of that anything
/// on the filesystem still uses. Equal means nothing in them is wasted.
pub fn file_totals(scan: &Scan, path: &Path) -> (u64, u64) {
    let mut allocated = 0;
    let mut used = 0;
    for (_, extent) in extents_of(scan, path) {
        allocated += extent.disk_bytes;
        used += extent.disk_used_bytes();
    }
    (allocated, used)
}

/// The bytes the extents holding `path` would give back if it were rewritten.
pub fn file_reclaimable(scan: &Scan, path: &Path) -> u64 {
    let (allocated, used) = file_totals(scan, path);
    allocated - used
}

/// The extents in the scan the run would rewrite, in address order.
pub fn worklist(scan: &Scan) -> Vec<&Extent> {
    let mut extents: Vec<&Extent> = scan
        .extents
        .values()
        .filter(|extent| extent.worth_rewriting())
        .collect();
    extents.sort_by_key(|extent| extent.disk_address);
    extents
}

/// Whether the run would rewrite the extent at `address`.
pub fn on_worklist(worklist: &[&Extent], address: u64) -> bool {
    worklist.iter().any(|extent| extent.disk_address == address)
}

/// The extent on the worklist that `path` holds, if any.
pub fn job_for<'a>(worklist: &[&'a Extent], path: &Path) -> Option<&'a Extent> {
    let id = identity(path);
    worklist.iter().copied().find(|extent| holds(extent, id))
}

pub fn options(path: &Path, apply: bool, dryrun: bool) -> Options {
    Options {
        path: path.to_path_buf(),
        apply,
        dryrun,
        verbose: false,
    }
}

/// Every extent on the filesystem, kept in memory at once so a test can look
/// at the whole picture before and after a run.
pub struct Scan {
    pub extents: HashMap<u64, Extent>,
    pub stats: ScanStats,
}

impl Scan {
    /// Allocated and used bytes over all extents, and the same split by extent
    /// size class.
    pub fn totals(&self) -> &Totals {
        &self.stats.totals
    }
}

/// Walks the filesystem with the tool's own scanner and keeps every extent it
/// hands over, where the tool would drop each one once dealt with.
pub fn scan(fs: &Fs) -> Scan {
    let filesystem =
        Rc::new(kernel::Filesystem::open(fs.root()).expect("open the fixture's filesystem"));
    let mut scanner = Scanner::new(&filesystem).expect("start the walk");
    let extents = scanner
        .by_ref()
        .map(|extent| (extent.disk_address, extent))
        .collect();
    assert!(
        scanner.error.is_none(),
        "the walk failed: {:?}",
        scanner.error
    );
    Scan {
        extents,
        stats: scanner.stats,
    }
}

/// Scans, rewrites, and syncs, then returns the scan it worked from and what it
/// did. Assert on the report; the same lines were printed as it went.
pub fn apply(fs: &Fs) -> (Scan, Report) {
    let options = options(fs.root(), true, false);
    // Scanned beforehand, so the check below knows which holders each
    // rewritten extent had.
    let scan = scan(fs);
    let (_, report) = btrealloc::run(&options).expect("run over the fixture");
    sync_fs();
    assert_released(fs, &scan, &report);
    (scan, report)
}

/// Every extent the run says it rewrote must be one no holder points at any
/// more.
///
/// A dedupe can come back successful and leave the holder exactly where it was.
/// The run then counts the job done and its bytes freed, and nothing downstream
/// notices: the extent stays allocated with a live reference into it. Checking
/// it here makes every apply in the suite a test for that.
fn assert_released(fs: &Fs, scan: &Scan, report: &Report) {
    use std::fmt::Write;

    let mut trouble = String::new();
    for extent in worklist(scan) {
        if !report.rewritten.contains(&extent.disk_address) {
            continue;
        }
        let holders = extent.holders();
        let mut stuck: Vec<((u64, u64), Vec<String>)> = Vec::new();
        for holder in &holders {
            let left: Vec<String> = file_extents_of(fs.root(), holder.root, holder.inode)
                .iter()
                .filter(|e| e.disk_address == extent.disk_address)
                .map(|e| {
                    format!(
                        "file offset {}, extent offset {}, {} bytes",
                        e.file_offset, e.extent_offset, e.num_bytes,
                    )
                })
                .collect();
            if !left.is_empty() {
                stuck.push(((holder.root, holder.inode), left));
            }
        }
        if stuck.is_empty() {
            continue;
        }

        let _ = writeln!(
            trouble,
            "extent {:#x}: {} on disk, {} uncompressed, {} to copy, {} to free",
            extent.disk_address,
            extent.disk_bytes,
            extent.uncompressed_bytes,
            extent.live_uncompressed_bytes(),
            extent.disk_free_bytes(),
        );
        let _ = writeln!(trouble, "  live at scan time: {:?}", extent.live_ranges);
        let _ = writeln!(
            trouble,
            "  copies the rewrite makes: {:?}",
            copy_ranges(extent)
        );
        for holder in &holders {
            let _ = writeln!(
                trouble,
                "  holder: inode {} of subvolume {}",
                holder.inode, holder.root,
            );
            for r in &holder.refs {
                let _ = writeln!(
                    trouble,
                    "    chunk: extent offset {}, file offset {}, {} bytes",
                    r.extent_offset, r.file_offset, r.num_bytes,
                );
            }
        }
        for ((root, inode), left) in stuck {
            let _ = writeln!(trouble, "  STILL HOLDS: inode {inode} of subvolume {root}");
            for line in left {
                let _ = writeln!(trouble, "    {line}");
            }
        }
    }
    assert!(
        trouble.is_empty(),
        "the run reported these extents freed:\n{trouble}",
    );
}

/// The stretches the rewrite copies, worked out the way `run` does it: the
/// references merged. Worked out here rather than exported from the tool,
/// so that the two drifting apart shows up as a difference rather than as
/// agreement.
fn copy_ranges(extent: &Extent) -> Vec<(u64, u64)> {
    let mut refs: Vec<_> = extent.refs.iter().collect();
    refs.sort_by_key(|r| r.extent_offset);

    let mut merged: Vec<(u64, u64)> = Vec::new();
    for r in refs {
        let (start, end) = (r.extent_offset, r.extent_offset + r.num_bytes);
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// The same walk with nothing written, which must leave the filesystem exactly
/// as it was.
pub fn dryrun(fs: &Fs) -> (Scan, Report) {
    let options = options(fs.root(), false, true);
    let scan = scan(fs);
    let (_, report) = btrealloc::run(&options).expect("run over the fixture");
    (scan, report)
}

/// The shape a block-level deduplicator such as bees leaves behind: `dest` is a
/// small file of its own whose sector at `dest_offset` is pointed at one sector of
/// whatever extent holds `src` at `src_offset`.
///
/// Built the way bees builds it, with the dedupe ioctl, so the reference is a
/// single sector in the middle of a much larger extent rather than a whole-file
/// reflink.
pub fn sliver(src: &Path, src_offset: u64, dest: &Path, dest_size: u64, dest_offset: u64) {
    sliver_of(src, src_offset, dest, dest_size, dest_offset, 4096);
}

/// The same for a reference of more than one sector, so two holders can be given
/// references of different lengths into the same part of an extent.
pub fn sliver_of(
    src: &Path,
    src_offset: u64,
    dest: &Path,
    dest_size: u64,
    dest_offset: u64,
    len: u64,
) {
    // A file of its own, then one sector of it replaced by the source's sector so
    // the two really do hold the same bytes: the kernel re-checks that under
    // lock and refuses the dedupe otherwise.
    write_random(dest, dest_size.div_ceil(MIB));
    let mut buf = vec![0u8; len as usize];
    let src_file = File::open(src).expect("open the sliver source");
    src_file
        .read_exact_at(&mut buf, src_offset)
        .expect("read the source sector");
    let dest_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dest)
        .expect("open the sliver destination");
    dest_file
        .write_all_at(&buf, dest_offset)
        .expect("write the destination sector");
    dest_file.sync_all().expect("sync the destination");
    sync_fs();

    let deduped = kernel::dedupe(&src_file, src_offset, len, &dest_file, dest_offset)
        .expect("dedupe the sector");
    assert_eq!(deduped, len, "the whole sector should have been deduped");
    dest_file.sync_all().expect("sync the deduped destination");
    sync_fs();
}

/// Another sliver into a file that already has some: [`sliver`] builds the
/// destination, this one only points one more of its sectors at `src`.
pub fn sliver_into(src: &Path, src_offset: u64, dest: &Path, dest_offset: u64) {
    let sectorsize = sectorsize(src);

    let mut sector = vec![0u8; sectorsize as usize];
    let src_file = File::open(src).expect("open the sliver source");
    src_file
        .read_exact_at(&mut sector, src_offset)
        .expect("read the source sector");
    let dest_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dest)
        .expect("open the sliver destination");
    dest_file
        .write_all_at(&sector, dest_offset)
        .expect("write the destination sector");
    dest_file.sync_all().expect("sync the destination");
    sync_fs();

    let deduped = kernel::dedupe(&src_file, src_offset, sectorsize, &dest_file, dest_offset)
        .expect("dedupe the sector");
    assert_eq!(
        deduped, sectorsize,
        "the whole sector should have been deduped"
    );
    dest_file.sync_all().expect("sync the deduped destination");
    sync_fs();
}

/// A small deterministic generator, so a failing layout can be rebuilt from the
/// seed the failure printed. xorshift64: good enough to pick offsets with, and
/// it needs no dependency the tool does not already have.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A number in `0..n`.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }

    /// A number in `low..=high`.
    pub fn between(&mut self, low: u64, high: u64) -> u64 {
        low + self.below(high - low + 1)
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
}

/// `len` bytes of the generator's output, which no compressor can shrink, or a
/// long run of one byte, which zstd flattens. Both shapes matter: the first
/// keeps extents the size they were written, the second lets the mount's
/// compression decide.
pub fn write_pattern(path: &Path, len: u64, rng: &mut Rng, compressible: bool) {
    let mut file = File::create(path).expect("create the file");
    let mut buf = vec![0u8; MIB as usize];
    let mut written = 0;
    while written < len {
        let n = ((len - written) as usize).min(buf.len());
        if compressible {
            buf[..n].fill(b'a' + (rng.below(26) as u8));
        } else {
            rng.fill(&mut buf[..n]);
        }
        file.write_all(&buf[..n]).expect("write the file");
        written += n as u64;
    }
    file.sync_all().expect("sync the file");
    sync_fs();
}

/// A filesystem laid out at random: a few files in a `data` subvolume, then a
/// run of the operations which leave the shapes btrealloc has to handle, in an
/// order and at offsets no hand-written fixture would think of. Among them,
/// read-only snapshots of `data`, and reflinks into a second subvolume, so an
/// extent can end up held from several subvolumes at once.
///
/// The shape is not the point, the oracle is: whatever comes out, every extent
/// the run reports freed has to really be released, and no file may come back
/// holding different bytes. Rebuilding a failure needs only the seed.
pub fn random_layout(fs: &Fs, rng: &mut Rng) -> Vec<PathBuf> {
    let sectorsize = fs.sectorsize();
    let data = fs.subvolume("data");
    let other = fs.subvolume("other");

    let mut files: Vec<PathBuf> = Vec::new();
    let mut next = 0;
    let name = |dir: &Path, files: &mut Vec<PathBuf>, next: &mut u64| {
        let path = dir.join(format!("f{next}"));
        *next += 1;
        files.push(path.clone());
        path
    };

    // A handful of files, one of them tens of MiB so a long live stretch gets
    // staged and redirected in one piece.
    for i in 0..rng.between(4, 6) {
        let mib = if i == 0 {
            rng.between(24, 40)
        } else {
            rng.between(1, 8)
        };
        let compressible = rng.below(4) == 0;
        let path = name(&data, &mut files, &mut next);
        write_pattern(&path, mib * MIB, rng, compressible);
    }

    for _ in 0..rng.between(12, 24) {
        let victim = files[rng.below(files.len() as u64) as usize].clone();
        let size = match std::fs::metadata(&victim) {
            Ok(meta) => meta.len(),
            Err(_) => continue,
        };
        if size < 8 * sectorsize {
            continue;
        }
        match rng.below(8) {
            // Drop a stretch of a file, which is what leaves an extent partly
            // unreachable in the first place.
            0 | 1 => {
                let offset = rng.below(size - sectorsize) & !(sectorsize - 1);
                let len = rng.between(sectorsize, size - offset) & !(sectorsize - 1);
                if len > 0 {
                    punch_hole(&victim, offset, len);
                }
            }
            // A second file over the same extents.
            2 => {
                let path = name(&data, &mut files, &mut next);
                reflink(&victim, &path);
            }
            // What a block-level deduplicator leaves: one sector of a small file
            // pointed into the middle of a much larger extent.
            3 => {
                let offset = rng.below(size - sectorsize) & !(sectorsize - 1);
                let path = name(&data, &mut files, &mut next);
                sliver(&victim, offset, &path, rng.between(1, 2) * MIB, sectorsize);
            }
            // A reference of more than one sector, so two holders can hold
            // different lengths of the same stretch.
            4 => {
                let offset = rng.below(size - 4 * sectorsize) & !(sectorsize - 1);
                let len = rng.between(1, 4) * sectorsize;
                let path = name(&data, &mut files, &mut next);
                sliver_of(&victim, offset, &path, 4 * MIB, sectorsize, len);
            }
            // A size which is not a whole number of sectors, so the last
            // reference runs past the end of the data.
            5 => {
                let to = rng.between(size / 2, size - 1) - rng.below(sectorsize - 1);
                let _ = OpenOptions::new()
                    .write(true)
                    .open(&victim)
                    .and_then(|f| f.set_len(to));
                sync_fs();
            }
            // The same extents held from another subvolume.
            6 => {
                let path = name(&other, &mut files, &mut next);
                reflink(&victim, &path);
            }
            // A read-only snapshot of everything in `data` so far, which the
            // operations after it leave behind.
            _ => {
                fs.snapshot("data", &format!("snapshot{next}"));
                next += 1;
            }
        }
    }
    files
}
