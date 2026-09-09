use std::path::PathBuf;
use std::rc::Rc;

use crate::scan::Scan;

// Hard-coded tunables for now

/// Minimum amount of bytes to reclaim in an extent.
const MIN_RECLAIM_BYTES: u64 = 65536;

/// Move at most this many bytes to free one. We currently choose to skip copying 100MB to reclaim 1MB.
const MAX_COPY_RATIO: u64 = 4;

/// One stretch of an extent, and where in a holder's file it appears.
pub struct Chunk {
    /// Offset in bytes in the extent (uncompressed)
    pub extent_offset: u64,
    /// Offset in bytes in the file
    pub file_offset: u64,
    /// Length in bytes
    pub len: u64,
}

/// One file holding an extent, and the stretches of it that file uses.
pub struct Holder {
    pub path: Rc<PathBuf>,
    pub chunks: Vec<Chunk>,
}

/// One extent to reallocate, with every file that holds it.
pub struct Job {
    /// Physical address of the extent, which is its identity.
    pub disk_address: u64,
    /// On-disk size of the extent, compressed size if it is compressed.
    pub disk_bytes: u64,
    /// The live bytes to copy, counted once however many holders share them.
    pub uncompressed_bytes: u64,
    /// On-disk bytes released once every holder has been rewritten.
    pub reclaimable_bytes: u64,
    pub holders: Vec<Holder>,
}

impl Job {
    /// The file this job is named after in reports: the first of its holders.
    pub fn path(&self) -> &Rc<PathBuf> {
        &self.holders[0].path
    }
}

/// Every extent selected for reallocating.
pub struct Worklist {
    /// The extents to process
    pub jobs: Vec<Job>,
    /// Uncompressed bytes walking the whole list would copy.
    pub uncompressed_bytes: u64,
    /// On-disk bytes walking the whole list would free.
    pub reclaimable_bytes: u64,
}

/// Select the extents to reallocate
pub fn create_jobs(scan: &Scan) -> Worklist {
    let mut jobs: Vec<Job> = Vec::new();

    for (&disk_address, extent) in &scan.extents {
        let free_bytes = extent.disk_free_bytes();
        let uncompressed_bytes = extent.live_uncompressed_bytes();

        // Check if the extent has at least MIN_RECLAIM_BYTES reclaimable bytes
        if free_bytes < MIN_RECLAIM_BYTES {
            continue;
        }

        // Check that we're not going to copy much to reclaim little
        if free_bytes * MAX_COPY_RATIO < uncompressed_bytes {
            continue;
        }

        // Check to pass on extents which have unknown refs (outside our path most likely)
        if extent.unknown_refs {
            continue;
        }

        // A nodatacow holder can never be rewritten, so the extent will not be freed
        if extent.refs.iter().any(|r| r.nocow) {
            continue;
        }

        jobs.push(Job {
            disk_address,
            disk_bytes: extent.disk_bytes,
            uncompressed_bytes,
            reclaimable_bytes: free_bytes,
            holders: extent
                .holders()
                .into_iter()
                .map(|(path, refs)| Holder {
                    path: Rc::clone(path),
                    chunks: refs
                        .iter()
                        .map(|r| Chunk {
                            extent_offset: r.start,
                            file_offset: r.file_offset,
                            len: r.end - r.start,
                        })
                        .collect(),
                })
                .collect(),
        });
    }

    // Sort by reclaimable bytes, from high to low.
    jobs.sort_by_key(|job| (std::cmp::Reverse(job.reclaimable_bytes), job.disk_address));
    Worklist {
        uncompressed_bytes: jobs.iter().map(|job| job.uncompressed_bytes).sum(),
        reclaimable_bytes: jobs.iter().map(|job| job.reclaimable_bytes).sum(),
        jobs,
    }
}
