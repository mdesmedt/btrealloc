use std::io;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;

use crate::kernel::Filesystem;

// Hard-coded tunables for now

/// Minimum amount of bytes to reclaim in an extent.
const MIN_RECLAIM_BYTES: u64 = 65536;

/// Move at most this many bytes to free one. We currently choose to skip copying 100MB to reclaim 1MB.
const MAX_COPY_RATIO: u64 = 4;

/// One reference from a file into a physical extent.
pub struct ExtentRef {
    pub path: Rc<PathBuf>,
    /// Physical address of the extent this reference points into, i.e. its identity.
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

/// One extent and every reference to it, wherever on the filesystem they are.
pub struct Extent {
    /// Physical address of the extent, which is its identity.
    pub disk_address: u64,
    /// Bytes on disk
    pub disk_bytes: u64,
    /// Bytes after decompression
    pub uncompressed_bytes: u64,
    /// Every reference to this extent, anywhere on the filesystem.
    pub refs: Vec<ExtentRef>,
    /// The live (actively referenced) ranges of this extent, merged, in order.
    /// Worked out once, here, from `refs`: nothing discovers a reference to an
    /// extent after this is built, so there is nothing for it to go stale
    /// against.
    pub live_ranges: Vec<Range<u64>>,
}

impl Extent {
    fn new(
        disk_address: u64,
        disk_bytes: u64,
        uncompressed_bytes: u64,
        refs: Vec<ExtentRef>,
    ) -> Extent {
        let live_ranges = live_ranges(&refs);
        Extent {
            disk_address,
            disk_bytes,
            uncompressed_bytes,
            refs,
            live_ranges,
        }
    }

    /// Fully discovers the extent `first` is one reference into: asks the
    /// filesystem how many references it really has, and if there is more than
    /// the one already in hand, resolves every one of them before this returns.
    ///
    /// `Ok(None)` means there is nothing safe to say about the extent: either it
    /// was freed while we scanned, or one of its references belongs to an inode
    /// we cannot resolve to a path (most often a snapshot's, in a different
    /// subvolume than the one being scanned). Either way the caller leaves the
    /// whole extent alone rather than acting on a partial picture of it.
    pub fn discover(fs: &Filesystem, first: ExtentRef) -> io::Result<Option<Extent>> {
        let disk_address = first.disk_address;
        let disk_bytes = first.disk_bytes;
        let uncompressed_bytes = first.uncompressed_bytes;

        let refs = match fs.extent_refs(disk_address, disk_bytes)? {
            None => return Ok(None),
            Some(count) if count <= 1 => vec![first],
            Some(_) => match fs.all_refs(disk_address)? {
                Some(refs) if !refs.is_empty() => refs,
                _ => return Ok(None),
            },
        };

        Ok(Some(Extent::new(
            disk_address,
            disk_bytes,
            uncompressed_bytes,
            refs,
        )))
    }

    /// Uncompressed bytes of this extent still referenced, counted once
    /// however many files reference them.
    pub fn live_uncompressed_bytes(&self) -> u64 {
        self.live_ranges.iter().map(|r| r.end - r.start).sum()
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

    /// On-disk bytes of this extent nothing references any more.
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
    pub fn holders(&self) -> Vec<Holder<'_>> {
        let mut refs: Vec<&ExtentRef> = self.refs.iter().collect();
        refs.sort_unstable_by(|a, b| (&*a.path, a.file_offset).cmp(&(&*b.path, b.file_offset)));

        let mut holders: Vec<Holder> = Vec::new();
        for r in refs {
            match holders.last_mut() {
                // Refs for the same file can come from different `file_extents`
                // calls (the scan's own walk, and a backref resolution elsewhere),
                // so they are not always the same `Rc`: compare the paths, not
                // their pointers.
                Some(holder) if holder.path.as_path() == r.path.as_path() => holder.refs.push(r),
                _ => holders.push(Holder {
                    path: &r.path,
                    refs: vec![r],
                }),
            }
        }
        holders
    }

    /// Whether reallocating this extent is worth what it costs.
    pub fn worth_rewriting(&self) -> bool {
        let free_bytes = self.disk_free_bytes();

        // Check if the extent has at least MIN_RECLAIM_BYTES reclaimable bytes
        if free_bytes < MIN_RECLAIM_BYTES {
            return false;
        }

        // Check that we're not going to copy much to reclaim little
        if free_bytes * MAX_COPY_RATIO < self.live_uncompressed_bytes() {
            return false;
        }

        // A nodatacow holder can never be rewritten, so the extent will not be freed
        !self.refs.iter().any(|r| r.nocow)
    }
}

/// One file holding an extent, and its references into it in file order.
pub struct Holder<'a> {
    pub path: &'a Rc<PathBuf>,
    pub refs: Vec<&'a ExtentRef>,
}

/// Every stretch of the extent some reference still covers, merged, in order.
fn live_ranges(refs: &[ExtentRef]) -> Vec<Range<u64>> {
    let mut ranges: Vec<Range<u64>> = refs
        .iter()
        .map(|r| r.extent_offset..r.extent_offset + r.num_bytes)
        .collect();
    ranges.sort_unstable_by_key(|r| (r.start, r.end));

    let mut merged: Vec<Range<u64>> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extent_ref(path: &Rc<PathBuf>, start: u64, len: u64) -> ExtentRef {
        ExtentRef {
            path: Rc::clone(path),
            disk_address: 0,
            disk_bytes: 1024,
            uncompressed_bytes: 1024,
            file_offset: 0,
            extent_offset: start,
            num_bytes: len,
            nocow: false,
        }
    }

    fn extent(refs: &[(u64, u64)]) -> Extent {
        let path = Rc::new(PathBuf::from("f"));
        let refs = refs
            .iter()
            .map(|&(off, len)| extent_ref(&path, off, len))
            .collect();
        Extent::new(0, 1024, 1024, refs)
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
    fn separate_stretches_stay_separate() {
        assert_eq!(
            extent(&[(0, 100), (900, 124)]).live_ranges,
            vec![0..100, 900..1024]
        );
    }

    #[test]
    fn touching_stretches_merge() {
        assert_eq!(extent(&[(0, 512), (512, 512)]).live_ranges, vec![0..1024]);
    }

    #[test]
    fn identical_references_are_one_range() {
        let e = extent(&[(256, 256), (256, 256)]);
        assert_eq!(e.disk_used_bytes(), 256);
        assert_eq!(e.disk_bytes - e.disk_used_bytes(), 768);
    }
}
