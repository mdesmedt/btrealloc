use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::format::human;
use crate::kernel::{self, Filesystem};

/// One reference from a file into an extent.
pub struct Ref {
    pub path: Rc<PathBuf>,
    pub file_offset: u64,
    pub start: u64,
    pub end: u64,
    pub nocow: bool,
}

/// One extent and every reference to it under the scanned path.
pub struct Extent {
    /// Bytes on disk
    pub disk_bytes: u64,
    /// Bytes after decompression
    pub uncompressed_bytes: u64,
    /// The references to each holder of this extent
    pub refs: Vec<Ref>,
    /// The filesystem references this more times than we found in our path
    pub unknown_refs: bool,
    /// [`Extent::live_ranges`] memoised. Every caller below asks for it, most
    /// of them more than once, and computing it sorts the whole reference list.
    live: OnceCell<Vec<(u64, u64)>>,
}

impl Extent {
    pub fn new(disk_bytes: u64, uncompressed_bytes: u64) -> Extent {
        Extent {
            disk_bytes,
            uncompressed_bytes,
            refs: Vec::new(),
            unknown_refs: false,
            live: OnceCell::new(),
        }
    }

    /// Adds a reference, dropping the memoised live ranges it invalidates.
    pub fn add_ref(&mut self, r: Ref) {
        self.live.take();
        self.refs.push(r);
    }

    /// The live (actively referenced) ranges of this extent, merged, in order.
    pub fn live_ranges(&self) -> &[(u64, u64)] {
        self.live.get_or_init(|| {
            let mut refs: Vec<(u64, u64)> = self.refs.iter().map(|r| (r.start, r.end)).collect();
            refs.sort_unstable();

            let mut merged: Vec<(u64, u64)> = Vec::new();
            for (start, end) in refs {
                match merged.last_mut() {
                    Some(last) if start <= last.1 => last.1 = last.1.max(end),
                    _ => merged.push((start, end)),
                }
            }
            merged
        })
    }

    /// Uncompressed bytes of this extent still referenced, counted once
    /// however many files reference them.
    pub fn live_uncompressed_bytes(&self) -> u64 {
        self.live_ranges()
            .iter()
            .map(|&(start, end)| end - start)
            .sum()
    }

    /// On-disk bytes of this extent actively referenced by files.
    pub fn disk_used_bytes(&self) -> u64 {
        // Scale on-disk over uncompressed, so compressed extents are counted in
        // on-disk bytes.
        if self.uncompressed_bytes == 0 {
            return 0;
        }
        (self.live_uncompressed_bytes().min(self.uncompressed_bytes) as u128
            * self.disk_bytes as u128
            / self.uncompressed_bytes as u128) as u64
    }

    /// On-disk bytes of this extent nothing under the scanned path uses.
    pub fn disk_free_bytes(&self) -> u64 {
        self.disk_bytes - self.disk_used_bytes()
    }

    /// Returns the files holding this extent, in path order, each file's
    /// references in file order.
    ///
    /// Sorting first and grouping runs of equal paths keeps this linear in the
    /// reference count past the sort; a deduplicator can spread one extent over
    /// thousands of files, where searching the groups built so far does not
    /// finish.
    pub fn holders(&self) -> Vec<(&Rc<PathBuf>, Vec<&Ref>)> {
        let mut refs: Vec<&Ref> = self.refs.iter().collect();
        refs.sort_unstable_by(|a, b| (&*a.path, a.file_offset).cmp(&(&*b.path, b.file_offset)));

        let mut holders: Vec<(&Rc<PathBuf>, Vec<&Ref>)> = Vec::new();
        for r in refs {
            match holders.last_mut() {
                Some((path, group)) if Rc::ptr_eq(path, &r.path) => group.push(r),
                _ => holders.push((&r.path, vec![r])),
            }
        }
        holders
    }
}

/// Extents of one size class, the class being a power of two: everything from
/// that size up to the next one.
#[derive(Clone, Copy)]
pub struct Bucket {
    pub count: u64,
    pub allocated_bytes: u64,
    pub used_bytes: u64,
    pub unreachable_bytes: u64,
    pub unreachable_shared_bytes: u64,
}

impl Bucket {
    pub const ZERO: Bucket = Bucket {
        count: 0,
        allocated_bytes: 0,
        used_bytes: 0,
        unreachable_bytes: 0,
        unreachable_shared_bytes: 0,
    };
}

/// Totals for returning stats.
pub struct Totals {
    pub allocated_bytes: u64,
    pub used_bytes: u64,
    pub unreachable_bytes: u64,
    pub unreachable_shared_bytes: u64,
    pub buckets: [Bucket; 64],
}

pub struct Scan {
    pub files: u64,
    pub file_bytes: u64,
    pub extents: HashMap<u64, Extent>,
    /// The filesystem being scanned, kept for what the run after it needs to
    /// ask: how many references an extent really has, and the sector size.
    pub fs: Filesystem,
}

impl Scan {
    pub fn new(fs: Filesystem) -> Scan {
        Scan {
            files: 0,
            file_bytes: 0,
            extents: HashMap::new(),
            fs,
        }
    }

    /// Adds a single file to the scan, looking up the extents it holds.
    pub fn add_file(&mut self, path: &Path, size: u64) {
        let path = Rc::new(path.to_path_buf());
        let extents = match kernel::file_extents(&path) {
            Ok(extents) => extents,
            Err(e) => {
                eprintln!("skipping {}: {e}", path.display());
                return;
            }
        };

        self.files += 1;
        self.file_bytes += size;
        for extent in extents {
            self.extents
                .entry(extent.disk_address)
                .or_insert_with(|| Extent::new(extent.disk_bytes, extent.uncompressed_bytes))
                .add_ref(Ref {
                    path: Rc::clone(&path),
                    file_offset: extent.file_offset,
                    start: extent.extent_offset,
                    end: extent.extent_offset + extent.num_bytes,
                    nocow: extent.nocow,
                });
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

    /// Ask the filesystem how many references each extent really has, so we
    /// know which ones a rewrite here would actually free.
    pub fn classify(&mut self) {
        for (&disk_address, extent) in &mut self.extents {
            let refs = match self.fs.extent_refs(disk_address, extent.disk_bytes) {
                Ok(Some(refs)) => refs,
                // Freed while we scanned, or the lookup failed: cannot confirm.
                Ok(None) => u64::MAX,
                Err(e) => {
                    eprintln!("extent {disk_address}: {e}");
                    u64::MAX
                }
            };
            extent.unknown_refs = refs > extent.refs.len() as u64;
        }
    }

    /// Allocated and used bytes over all extents, and the same split by extent
    /// size class. Space in an extent the filesystem references more times than
    /// we found is reclaimable too, but only once those other references go.
    pub fn totals(&self) -> Totals {
        let mut totals = Totals {
            allocated_bytes: 0,
            used_bytes: 0,
            unreachable_bytes: 0,
            unreachable_shared_bytes: 0,
            buckets: [Bucket::ZERO; 64],
        };

        for extent in self.extents.values() {
            let used_bytes = extent.disk_used_bytes();
            let free_bytes = extent.disk_free_bytes();

            totals.allocated_bytes += extent.disk_bytes;
            totals.used_bytes += used_bytes;

            let bucket = &mut totals.buckets[extent.disk_bytes.max(1).ilog2() as usize];
            bucket.count += 1;
            bucket.allocated_bytes += extent.disk_bytes;
            bucket.used_bytes += used_bytes;

            if extent.unknown_refs {
                totals.unreachable_shared_bytes += free_bytes;
                bucket.unreachable_shared_bytes += free_bytes;
            } else {
                totals.unreachable_bytes += free_bytes;
                bucket.unreachable_bytes += free_bytes;
            }
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
        println!(
            "unreachable shared: {}",
            human(totals.unreachable_shared_bytes)
        );

        println!();
        println!("extent size rounded down to a power of two:");
        println!(
            "{:>12}  {:>10}  {:>12}  {:>12}  {:>12}  {:>12}",
            "size", "count", "allocated", "used", "unreachable", "shared"
        );
        for (log2, bucket) in totals.buckets.iter().enumerate() {
            if bucket.count == 0 {
                continue;
            }
            println!(
                "{:>12}  {:>10}  {:>12}  {:>12}  {:>12}  {:>12}",
                human(1 << log2),
                bucket.count,
                human(bucket.allocated_bytes),
                human(bucket.used_bytes),
                human(bucket.unreachable_bytes),
                human(bucket.unreachable_shared_bytes),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extent(refs: &[(u64, u64)]) -> Extent {
        let path = Rc::new(PathBuf::from("f"));
        let mut extent = Extent::new(1024, 1024);
        for &(off, len) in refs {
            extent.add_ref(Ref {
                path: Rc::clone(&path),
                file_offset: 0,
                start: off,
                end: off + len,
                nocow: false,
            });
        }
        extent
    }

    #[test]
    fn disjoint_references_add_up() {
        assert_eq!(extent(&[(0, 100), (900, 124)]).disk_used_bytes(), 224);
    }

    #[test]
    fn overlapping_references_count_once() {
        assert_eq!(extent(&[(0, 600), (400, 400)]).disk_used_bytes(), 800);
    }

    #[test]
    fn a_reference_inside_another_is_absorbed() {
        assert_eq!(extent(&[(0, 900), (100, 50)]).disk_used_bytes(), 900);
    }

    #[test]
    fn identical_references_are_one_range() {
        let e = extent(&[(256, 256), (256, 256)]);
        assert_eq!(e.disk_used_bytes(), 256);
        assert_eq!(e.disk_bytes - e.disk_used_bytes(), 768);
    }
}
