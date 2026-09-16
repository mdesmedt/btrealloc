use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::rc::Rc;

use crate::extent::Extent;
use crate::format::human;
use crate::kernel::{self, Filesystem};

/// Extents of one size class, the class being a power of two: everything from
/// that size up to the next one.
#[derive(Clone, Copy)]
pub struct Bucket {
    pub count: u64,
    pub allocated_bytes: u64,
    pub used_bytes: u64,
    pub unreachable_bytes: u64,
}

impl Bucket {
    pub const ZERO: Bucket = Bucket {
        count: 0,
        allocated_bytes: 0,
        used_bytes: 0,
        unreachable_bytes: 0,
    };
}

/// Totals for returning stats.
pub struct Totals {
    pub allocated_bytes: u64,
    pub used_bytes: u64,
    pub unreachable_bytes: u64,
    pub buckets: [Bucket; 64],
}

pub struct Scan {
    pub files: u64,
    pub file_bytes: u64,
    pub extents: HashMap<u64, Extent>,
    /// Extents we found a reference into but could not fully account for:
    /// freed while we scanned, or shared with an inode outside this
    /// subvolume (most often a snapshot's). Kept only so we do not repeat the
    /// lookup every time another reference into the same extent turns up.
    skipped: HashSet<u64>,
    /// The filesystem being scanned, kept for what the run after it needs to
    /// ask: the sector size.
    pub fs: Filesystem,
}

impl Scan {
    pub fn new(fs: Filesystem) -> Scan {
        Scan {
            files: 0,
            file_bytes: 0,
            extents: HashMap::new(),
            skipped: HashSet::new(),
            fs,
        }
    }

    /// Adds a single file to the scan, looking up the extents it holds. Every
    /// extent not already known is fully discovered right here: by the time
    /// this returns, everything in `self.extents` accounts for every
    /// reference to it, wherever on the filesystem that reference is.
    pub fn add_file(&mut self, path: &Path, size: u64) {
        let path = Rc::new(path.to_path_buf());
        let refs = match kernel::file_extents(&path) {
            Ok(refs) => refs,
            Err(e) => {
                eprintln!("skipping {}: {e}", path.display());
                return;
            }
        };

        self.files += 1;
        self.file_bytes += size;
        for r in refs {
            let disk_address = r.disk_address;
            if self.extents.contains_key(&disk_address) || self.skipped.contains(&disk_address) {
                continue;
            }
            match Extent::discover(&self.fs, r) {
                Ok(Some(extent)) => {
                    self.extents.insert(disk_address, extent);
                }
                Ok(None) => {
                    eprintln!(
                        "extent {disk_address:#x}: shared with a snapshot or otherwise \
                         unresolvable, leaving it alone"
                    );
                    self.skipped.insert(disk_address);
                }
                Err(e) => {
                    eprintln!("extent {disk_address:#x}: {e}");
                    self.skipped.insert(disk_address);
                }
            }
        }
    }

    /// Walks `dir`, staying on the filesystem identified by `dev`: entries on a
    /// different device are skipped.
    pub fn walk(&mut self, dir: &Path, dev: u64, seen: &mut HashSet<(u64, u64)>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!("skipping {}: {e}", dir.display());
                return;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };

            if meta.dev() != dev {
                continue;
            }
            if meta.is_dir() {
                self.walk(&path, dev, seen);
            } else if meta.is_file() && seen.insert((meta.dev(), meta.ino())) {
                self.add_file(&path, meta.size());
            }
        }
    }

    /// Allocated and used bytes over all extents, and the same split by extent
    /// size class.
    pub fn totals(&self) -> Totals {
        let mut totals = Totals {
            allocated_bytes: 0,
            used_bytes: 0,
            unreachable_bytes: 0,
            buckets: [Bucket::ZERO; 64],
        };

        for extent in self.extents.values() {
            let used_bytes = extent.disk_used_bytes();
            let free_bytes = extent.disk_free_bytes();

            totals.allocated_bytes += extent.disk_bytes;
            totals.used_bytes += used_bytes;
            totals.unreachable_bytes += free_bytes;

            let bucket = &mut totals.buckets[extent.disk_bytes.max(1).ilog2() as usize];
            bucket.count += 1;
            bucket.allocated_bytes += extent.disk_bytes;
            bucket.used_bytes += used_bytes;
            bucket.unreachable_bytes += free_bytes;
        }
        totals
    }

    /// Prints what the scan found: the totals, then the same split by extent size class.
    pub fn report(&self) {
        let totals = self.totals();
        println!("files:              {}", self.files);
        println!("file size:          {}", human(self.file_bytes));
        println!("extents:            {}", self.extents.len());
        println!("allocated:          {}", human(totals.allocated_bytes));
        println!("used:               {}", human(totals.used_bytes));
        println!("unreachable:        {}", human(totals.unreachable_bytes));
        if !self.skipped.is_empty() {
            println!("left alone:         {} extents", self.skipped.len());
        }

        println!();
        println!("extent size rounded down to a power of two:");
        println!(
            "{:>12}  {:>10}  {:>12}  {:>12}  {:>12}",
            "size", "count", "allocated", "used", "unreachable"
        );
        for (log2, bucket) in totals.buckets.iter().enumerate() {
            if bucket.count == 0 {
                continue;
            }
            println!(
                "{:>12}  {:>10}  {:>12}  {:>12}  {:>12}",
                human(1 << log2),
                bucket.count,
                human(bucket.allocated_bytes),
                human(bucket.used_bytes),
                human(bucket.unreachable_bytes),
            );
        }
    }
}
