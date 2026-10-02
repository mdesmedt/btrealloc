use std::ops::Range;

use crate::format::human;

// Hard-coded tunables for now

/// Minimum amount of bytes to reclaim in an extent.
const MIN_RECLAIM_BYTES: u64 = 65536;

/// Move at most this many bytes to free one. We currently choose to skip copying 100MB to reclaim 1MB.
const MAX_COPY_RATIO: u64 = 4;

/// One reference from a file into a physical extent.
pub struct ExtentRef {
    /// Tree id of the subvolume holding the file.
    pub root: u64,
    /// Inode number of the holding file, in its subvolume.
    pub inode: u64,
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
    /// The generation of the tree leaf this reference was read from. A leaf no
    /// newer than the subvolume's last snapshot may be shared with another
    /// tree, which reaches the extent through it too.
    pub leaf_generation: u64,
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

    /// The extent `refs` point into, from every reference to it there is, as
    /// [`ExtentWalk`](crate::kernel::ExtentWalk) finds them. `refs` must not be
    /// empty.
    pub fn from_refs(refs: Vec<ExtentRef>) -> Extent {
        let first = &refs[0];
        let (disk_address, disk_bytes, uncompressed_bytes) = (
            first.disk_address,
            first.disk_bytes,
            first.uncompressed_bytes,
        );
        // Check that all refs match
        for r in &refs[1..] {
            debug_assert_eq!(r.disk_address, disk_address);
            debug_assert_eq!(r.disk_bytes, disk_bytes);
            debug_assert_eq!(r.uncompressed_bytes, uncompressed_bytes);
        }
        Extent::new(disk_address, disk_bytes, uncompressed_bytes, refs)
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

    /// Returns the files holding this extent, by subvolume and inode, each
    /// file's references in file order.
    ///
    /// Sorting first and grouping runs of the same file keeps this linear in
    /// the reference count past the sort; a deduplicator can spread one extent
    /// over thousands of files, where searching the groups built so far does
    /// not finish.
    pub fn holders(&self) -> Vec<Holder<'_>> {
        let mut refs: Vec<&ExtentRef> = self.refs.iter().collect();
        refs.sort_unstable_by_key(|r| (r.root, r.inode, r.file_offset));

        let mut holders: Vec<Holder> = Vec::new();
        for r in refs {
            match holders.last_mut() {
                Some(holder) if (holder.root, holder.inode) == (r.root, r.inode) => {
                    holder.refs.push(r)
                }
                _ => holders.push(Holder {
                    root: r.root,
                    inode: r.inode,
                    refs: vec![r],
                }),
            }
        }
        holders
    }

    /// Whether reallocating this extent is worth what it costs.
    pub fn worth_rewriting(&self) -> bool {
        self.not_worth_rewriting().is_none()
    }

    /// Why reallocating this extent is not worth what it costs, if it isn't.
    pub fn not_worth_rewriting(&self) -> Option<LeftAlone> {
        let free_bytes = self.disk_free_bytes();

        // Check if the extent has at least MIN_RECLAIM_BYTES reclaimable bytes
        if free_bytes < MIN_RECLAIM_BYTES {
            return Some(LeftAlone::TooLittleToFree);
        }

        // Check that we're not going to copy much to reclaim little
        if free_bytes * MAX_COPY_RATIO < self.live_uncompressed_bytes() {
            return Some(LeftAlone::TooMuchToCopy);
        }
        None
    }
}

/// Why an extent with unreachable space in it was not rewritten.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LeftAlone {
    /// It frees less than [`MIN_RECLAIM_BYTES`].
    TooLittleToFree,
    /// It copies more than [`MAX_COPY_RATIO`] times what it frees.
    TooMuchToCopy,
    /// A nodatacow file holds it.
    NoDataCow,
    /// Space reserved past a file's end holds it, and no dedupe reaches there.
    PastEnd,
    /// It was freed or its references moved while the scan was reading it.
    Changed,
    /// Looking up who holds it failed, during the walk or just before acting.
    Unresolved,
    /// Finding, opening or rewriting one of its holders failed.
    Failed,
}

impl LeftAlone {
    pub const ALL: [LeftAlone; 7] = [
        LeftAlone::TooLittleToFree,
        LeftAlone::TooMuchToCopy,
        LeftAlone::NoDataCow,
        LeftAlone::PastEnd,
        LeftAlone::Changed,
        LeftAlone::Unresolved,
        LeftAlone::Failed,
    ];

    /// The reason, for the table at the end of a run.
    pub fn label(self) -> String {
        match self {
            LeftAlone::TooLittleToFree => format!("frees under {}", human(MIN_RECLAIM_BYTES)),
            LeftAlone::TooMuchToCopy => format!("copies over {MAX_COPY_RATIO}x what it frees"),
            LeftAlone::NoDataCow => "held by a nodatacow file".to_string(),
            LeftAlone::PastEnd => "held past a file's end".to_string(),
            LeftAlone::Changed => "changed during the scan".to_string(),
            LeftAlone::Unresolved => "holders could not be looked up".to_string(),
            LeftAlone::Failed => "rewrite failed".to_string(),
        }
    }
}

/// One file holding an extent, and its references into it in file order.
pub struct Holder<'a> {
    /// Tree id of the subvolume the file is in.
    pub root: u64,
    /// Inode number of the file, in that subvolume.
    pub inode: u64,
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

    fn extent_ref(start: u64, len: u64) -> ExtentRef {
        ExtentRef {
            root: 5,
            inode: 257,
            disk_address: 0,
            disk_bytes: 1024,
            uncompressed_bytes: 1024,
            file_offset: 0,
            extent_offset: start,
            num_bytes: len,
            leaf_generation: 0,
        }
    }

    fn extent(refs: &[(u64, u64)]) -> Extent {
        let refs = refs
            .iter()
            .map(|&(off, len)| extent_ref(off, len))
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
    fn mostly_dead_extent_is_worth_rewriting() {
        let e = Extent::new(0, 1 << 20, 1 << 20, vec![extent_ref(0, 4096)]);
        assert_eq!(e.not_worth_rewriting(), None);
    }

    #[test]
    fn small_gap_frees_too_little() {
        assert_eq!(
            extent(&[(0, 1000)]).not_worth_rewriting(),
            Some(LeftAlone::TooLittleToFree)
        );
    }

    #[test]
    fn mostly_live_extent_copies_too_much() {
        let e = Extent::new(0, 1 << 20, 1 << 20, vec![extent_ref(0, 900 << 10)]);
        assert_eq!(e.not_worth_rewriting(), Some(LeftAlone::TooMuchToCopy));
    }

    #[test]
    fn identical_references_are_one_range() {
        let e = extent(&[(256, 256), (256, 256)]);
        assert_eq!(e.disk_used_bytes(), 256);
        assert_eq!(e.disk_bytes - e.disk_used_bytes(), 768);
    }
}
