//! Building throwaway btrfs filesystems to test against, and reading them back.
//!
//! Every test gets its own filesystem: a tmpfs, an image inside it, and a loop
//! mount of that image, all torn down when the [`Fs`] goes out of scope. Nothing
//! is shared between tests, so no test depends on what another one applied.
//!
//! The shapes below are the ones btrealloc distinguishes, built with the same
//! operations the kernel sees from `dd`, `fallocate --punch-hole`,
//! `cp --reflink` and `chattr +C`, done here as plain syscalls.

#![allow(dead_code)] // each test uses a different corner of this

use std::collections::BTreeMap;
use std::ffi::{c_int, c_ulong, c_void};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use linux_raw_sys::general::{FALLOC_FL_KEEP_SIZE, FALLOC_FL_PUNCH_HOLE, FS_NOCOW_FL};
use linux_raw_sys::ioctl::{FICLONE, FS_IOC_GETFLAGS, FS_IOC_SETFLAGS};
use sha2::{Digest, Sha256};

use btrealloc::Options;
use btrealloc::kernel;
use btrealloc::run::Report;
use btrealloc::scan::{Extent, Scan};
use btrealloc::worklist::{self, Worklist};

pub const MIB: u64 = 1 << 20;

unsafe extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, arg: *mut c_void) -> c_int;
    fn fallocate(fd: c_int, mode: c_int, offset: i64, len: i64) -> c_int;
    fn sync();
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

        let opts = match mountopts {
            "" => "loop".to_string(),
            extra => format!("loop,{extra}"),
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

    pub fn path(&self, rel: &str) -> PathBuf {
        self.mnt.join(rel)
    }

    /// The directory at `rel`, made along with any parents.
    pub fn dir(&self, rel: &str) -> PathBuf {
        let dir = self.path(rel);
        std::fs::create_dir_all(&dir).expect("make a directory");
        dir
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

/// Every entry under `dir`, sorted. A temporary file left anywhere on the
/// filesystem turns up here, not just one under a name we guessed.
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

/// The physical addresses of a file's extents. Same answer `filefrag -v` gives,
/// without parsing it: two files at the same address share one copy.
pub fn physical_extents(path: &Path) -> Vec<u64> {
    let mut addresses: Vec<u64> = kernel::file_extents(path)
        .expect("read the file's extents")
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

/// The extents holding `path`, each with the address it lives at.
pub fn extents_of<'a>(scan: &'a Scan, path: &Path) -> Vec<(u64, &'a Extent)> {
    let mut extents: Vec<(u64, &Extent)> = scan
        .extents
        .iter()
        .filter(|(_, extent)| extent.refs.iter().any(|r| **r.path == *path))
        .map(|(&address, extent)| (address, extent))
        .collect();
    extents.sort_by_key(|&(address, _)| address);
    extents
}

/// What the extents holding `path` allocate, and how much of that anything
/// under the scan still uses. Equal means nothing in them is wasted.
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

/// Whether the run would rewrite the extent at `address`.
pub fn on_worklist(worklist: &Worklist, address: u64) -> bool {
    worklist.jobs.iter().any(|job| job.disk_address == address)
}

/// Whether any job on the worklist names `path`, as a holder or otherwise.
pub fn job_for<'a>(worklist: &'a Worklist, path: &Path) -> Option<&'a worklist::Job> {
    worklist
        .jobs
        .iter()
        .find(|job| job.holders.iter().any(|h| **h.path == *path))
}

fn options(path: &Path, apply: bool, dryrun: bool) -> Options {
    Options {
        path: path.to_path_buf(),
        apply,
        dryrun,
        // Hashing either side of every rewrite: the tests want the strictest
        // check the tool can make of itself.
        verify: apply,
        verbose: false,
    }
}

/// Scans `dir` and returns what the scan found.
pub fn scan(dir: &Path) -> Scan {
    btrealloc::scan(&options(dir, false, false)).expect("scan the fixture")
}

/// Scans `dir` and selects the extents worth rewriting.
pub fn scan_worklist(dir: &Path) -> (Scan, Worklist) {
    let scan = scan(dir);
    let worklist = worklist::create_jobs(&scan);
    (scan, worklist)
}

/// Scans, rewrites, and syncs, then returns the scan it worked from and what it
/// did. Assert on the report; the same lines were printed as it went.
pub fn apply(dir: &Path) -> (Scan, Report) {
    let options = options(dir, true, false);
    let scan = btrealloc::scan(&options).expect("scan the fixture");
    // The same worklist the run builds for itself, kept so the check below
    // knows which holders each rewritten extent had.
    let worklist = worklist::create_jobs(&scan);
    let report = btrealloc::run(&options, &scan);
    sync_fs();
    assert_released(&scan, &worklist, &report);
    (scan, report)
}

/// Every extent the run says it rewrote must be one no holder points at any
/// more.
///
/// A dedupe can come back successful and leave the holder exactly where it was.
/// The run then counts the job done and its bytes freed, and nothing downstream
/// notices: the extent stays allocated with a live reference into it. Checking
/// it here makes every apply in the suite a test for that.
fn assert_released(scan: &Scan, worklist: &Worklist, report: &Report) {
    use std::fmt::Write;

    let mut trouble = String::new();
    for job in &worklist.jobs {
        if !report.rewritten.contains(&job.disk_address) {
            continue;
        }
        let mut stuck: Vec<(&Rc<PathBuf>, Vec<String>)> = Vec::new();
        for holder in &job.holders {
            let left: Vec<String> = kernel::file_extents(&holder.path)
                .expect("re-read a holder's extents")
                .iter()
                .filter(|e| e.disk_address == job.disk_address)
                .map(|e| {
                    format!(
                        "file offset {}, extent offset {}, {} bytes",
                        e.file_offset, e.extent_offset, e.num_bytes,
                    )
                })
                .collect();
            if !left.is_empty() {
                stuck.push((&holder.path, left));
            }
        }
        if stuck.is_empty() {
            continue;
        }

        let _ = writeln!(
            trouble,
            "extent {:#x}: {} on disk, {} uncompressed, {} to copy, {} to free",
            job.disk_address,
            job.disk_bytes,
            job.uncompressed_bytes,
            job.uncompressed_bytes,
            job.reclaimable_bytes,
        );
        if let Some(extent) = scan.extents.get(&job.disk_address) {
            let _ = writeln!(trouble, "  live at scan time: {:?}", extent.live_ranges());
        }
        let _ = writeln!(
            trouble,
            "  copies the rewrite makes: {:?}",
            copy_ranges(job)
        );
        for holder in &job.holders {
            let size = std::fs::metadata(&**holder.path).map(|m| m.len());
            let _ = writeln!(
                trouble,
                "  holder {} ({:?} bytes)",
                holder.path.display(),
                size,
            );
            for chunk in &holder.chunks {
                let _ = writeln!(
                    trouble,
                    "    chunk: extent offset {}, file offset {}, {} bytes",
                    chunk.extent_offset, chunk.file_offset, chunk.len,
                );
            }
        }
        for (path, left) in stuck {
            let _ = writeln!(trouble, "  STILL HOLDS: {}", path.display());
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
/// holders' chunks merged. Worked out here rather than exported from the tool,
/// so that the two drifting apart shows up as a difference rather than as
/// agreement.
fn copy_ranges(job: &worklist::Job) -> Vec<(u64, u64)> {
    let mut chunks: Vec<&worklist::Chunk> = job.holders.iter().flat_map(|h| &h.chunks).collect();
    chunks.sort_by_key(|chunk| chunk.extent_offset);

    let mut merged: Vec<(u64, u64)> = Vec::new();
    for chunk in chunks {
        let (start, end) = (chunk.extent_offset, chunk.extent_offset + chunk.len);
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// The same walk with nothing written, which must leave the filesystem exactly
/// as it was.
pub fn dryrun(dir: &Path) -> (Scan, Report) {
    let options = options(dir, false, true);
    let scan = btrealloc::scan(&options).expect("scan the fixture");
    let report = btrealloc::run(&options, &scan);
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
    const SECTORSIZE: u64 = 4096;

    let mut sector = vec![0u8; SECTORSIZE as usize];
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

    let deduped = kernel::dedupe(&src_file, src_offset, SECTORSIZE, &dest_file, dest_offset)
        .expect("dedupe the sector");
    assert_eq!(
        deduped, SECTORSIZE,
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

/// A filesystem laid out at random: a few files, then a run of the operations
/// which leave the shapes btrealloc has to handle, in an order and at offsets no
/// hand-written fixture would think of.
///
/// The shape is not the point, the oracle is: whatever comes out, every extent
/// the run reports freed has to really be released, and no file may come back
/// holding different bytes. Rebuilding a failure needs only the seed.
pub fn random_layout(dir: &Path, rng: &mut Rng) -> Vec<PathBuf> {
    const SECTORSIZE: u64 = 4096;

    let mut files: Vec<PathBuf> = Vec::new();
    let mut next = 0;
    let name = |files: &mut Vec<PathBuf>, next: &mut u64| {
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
        let path = name(&mut files, &mut next);
        write_pattern(&path, mib * MIB, rng, compressible);
    }

    for _ in 0..rng.between(12, 24) {
        let victim = files[rng.below(files.len() as u64) as usize].clone();
        let size = match std::fs::metadata(&victim) {
            Ok(meta) => meta.len(),
            Err(_) => continue,
        };
        if size < 8 * SECTORSIZE {
            continue;
        }
        match rng.below(6) {
            // Drop a stretch of a file, which is what leaves an extent partly
            // unreachable in the first place.
            0 | 1 => {
                let offset = rng.below(size - SECTORSIZE) & !(SECTORSIZE - 1);
                let len = rng.between(SECTORSIZE, size - offset) & !(SECTORSIZE - 1);
                if len > 0 {
                    punch_hole(&victim, offset, len);
                }
            }
            // A second file over the same extents.
            2 => {
                let path = name(&mut files, &mut next);
                reflink(&victim, &path);
            }
            // What a block-level deduplicator leaves: one sector of a small file
            // pointed into the middle of a much larger extent.
            3 => {
                let offset = rng.below(size - SECTORSIZE) & !(SECTORSIZE - 1);
                let path = name(&mut files, &mut next);
                sliver(&victim, offset, &path, rng.between(1, 2) * MIB, SECTORSIZE);
            }
            // A reference of more than one sector, so two holders can hold
            // different lengths of the same stretch.
            4 => {
                let offset = rng.below(size - 4 * SECTORSIZE) & !(SECTORSIZE - 1);
                let len = rng.between(1, 4) * SECTORSIZE;
                let path = name(&mut files, &mut next);
                sliver_of(&victim, offset, &path, 4 * MIB, SECTORSIZE, len);
            }
            // A size which is not a whole number of sectors, so the last
            // reference runs past the end of the data.
            _ => {
                let to = rng.between(size / 2, size - 1) - rng.below(SECTORSIZE - 1);
                let _ = OpenOptions::new()
                    .write(true)
                    .open(&victim)
                    .and_then(|f| f.set_len(to));
                sync_fs();
            }
        }
    }
    files
}
