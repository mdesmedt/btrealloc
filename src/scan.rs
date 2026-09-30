use std::io;
use std::sync::Arc;

use crate::Options;
use crate::extent::{Extent, LeftAlone};
use crate::format::human;
use crate::kernel::{self, ExtentWalk, Filesystem, Resolved};

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

impl Totals {
    pub const ZERO: Totals = Totals {
        allocated_bytes: 0,
        used_bytes: 0,
        unreachable_bytes: 0,
        buckets: [Bucket::ZERO; 64],
    };

    /// Counts one extent into the totals and into its size class.
    pub fn add(&mut self, extent: &Extent) {
        let used_bytes = extent.disk_used_bytes();
        let free_bytes = extent.disk_free_bytes();

        self.allocated_bytes += extent.disk_bytes;
        self.used_bytes += used_bytes;
        self.unreachable_bytes += free_bytes;

        let bucket = &mut self.buckets[extent.disk_bytes.max(1).ilog2() as usize];
        bucket.count += 1;
        bucket.allocated_bytes += extent.disk_bytes;
        bucket.used_bytes += used_bytes;
        bucket.unreachable_bytes += free_bytes;
    }
}

/// Extents with unreachable space in them that were not rewritten, and how
/// much of that space they hold, by reason.
pub struct LeftAloneStats {
    /// Indexed by position in [`LeftAlone::ALL`].
    pub reasons: [Bucket; LeftAlone::ALL.len()],
}

impl LeftAloneStats {
    const ZERO: LeftAloneStats = LeftAloneStats {
        reasons: [Bucket::ZERO; LeftAlone::ALL.len()],
    };

    /// Counts one extent left alone. An extent the walk could not resolve is
    /// counted with no bytes: it is not in the totals either.
    pub fn add(&mut self, reason: LeftAlone, extent: Option<&Extent>) {
        let bucket = &mut self.reasons[reason as usize];
        bucket.count += 1;
        if let Some(extent) = extent {
            bucket.allocated_bytes += extent.disk_bytes;
            bucket.used_bytes += extent.disk_used_bytes();
            bucket.unreachable_bytes += extent.disk_free_bytes();
        }
    }

    pub fn is_empty(&self) -> bool {
        self.reasons.iter().all(|bucket| bucket.count == 0)
    }

    pub fn get(&self, reason: LeftAlone) -> &Bucket {
        &self.reasons[reason as usize]
    }
}

/// What the walk has seen so far, counted as it goes: every extent is counted
/// once, when it is discovered, and nothing about it is kept afterwards.
pub struct ScanStats {
    pub extents: u64,
    pub totals: Totals,
    /// Extents with unreachable space that were not rewritten, by reason.
    pub left_alone: LeftAloneStats,
}

impl ScanStats {
    const ZERO: ScanStats = ScanStats {
        extents: 0,
        totals: Totals::ZERO,
        left_alone: LeftAloneStats::ZERO,
    };

    /// Prints what the scan found: the totals, then the same split by extent size class.
    pub fn report(&self) {
        let totals = &self.totals;
        println!("extents:            {}", self.extents);
        println!("allocated:          {}", human(totals.allocated_bytes));
        println!("used:               {}", human(totals.used_bytes));
        println!("unreachable:        {}", human(totals.unreachable_bytes));

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

        self.report_left_alone();
    }

    /// Prints the extents with unreachable space that were not rewritten, by
    /// reason. Unlike the totals, this still holds after an `--apply`: these
    /// are the extents the run did not rewrite.
    pub fn report_left_alone(&self) {
        if !self.left_alone.is_empty() {
            println!();
            println!("extents with unreachable space left alone:");
            println!(
                "{:>32}  {:>10}  {:>12}  {:>12}  {:>12}",
                "reason", "count", "allocated", "used", "unreachable"
            );
            for reason in LeftAlone::ALL {
                let bucket = self.left_alone.get(reason);
                if bucket.count == 0 {
                    continue;
                }
                println!(
                    "{:>32}  {:>10}  {:>12}  {:>12}  {:>12}",
                    reason.label(),
                    bucket.count,
                    human(bucket.allocated_bytes),
                    human(bucket.used_bytes),
                    human(bucket.unreachable_bytes),
                );
            }
        }
    }
}

/// Every data extent on the filesystem, fully resolved, counted into the stats
/// as it is handed over and then forgotten. See [`ExtentWalk`].
pub struct Scanner {
    fs: Arc<Filesystem>,
    walk: ExtentWalk,
    options: Options,
    pub stats: ScanStats,
    /// Why the walk ended early, if it did.
    pub error: Option<io::Error>,
}

impl Scanner {
    pub fn new(fs: &Arc<Filesystem>, options: &Options) -> io::Result<Scanner> {
        Ok(Scanner {
            fs: Arc::clone(fs),
            walk: fs.walk()?,
            options: options.clone(),
            stats: ScanStats::ZERO,
            error: None,
        })
    }

    /// `extent` resolved again, as it is now rather than as the walk read it,
    /// for acting on: an earlier rewrite can have changed who holds it since.
    /// `None` if it cannot be, and then it is counted as left alone.
    pub fn resolve_again(&mut self, extent: &Extent) -> Option<Extent> {
        match self.fs.resolve_again(extent) {
            Ok(extent) => Some(extent),
            Err(error) => {
                self.leave_unresolved(extent.disk_address, &error, Some(extent));
                None
            }
        }
    }

    /// Counts an extent whose holders could not be resolved, and says why,
    /// unless it merely changed under us: on a filesystem in use that is
    /// routine, and the table at the end counts it.
    fn leave_unresolved(&mut self, disk_address: u64, error: &io::Error, extent: Option<&Extent>) {
        let reason = if kernel::is_changed(error) {
            LeftAlone::Changed
        } else {
            LeftAlone::Unresolved
        };
        if self.options.verbose || reason == LeftAlone::Unresolved {
            eprintln!("extent {disk_address:#x}: {error}, leaving it alone");
        }
        self.stats.left_alone.add(reason, extent);
    }
}

impl Iterator for Scanner {
    type Item = Extent;

    /// The next extent the walk discovers, with every reference to it,
    /// wherever on the filesystem that reference is. Extents left alone are
    /// counted and passed over.
    fn next(&mut self) -> Option<Extent> {
        loop {
            match self.walk.next()? {
                Ok(Resolved::Extent(extent)) => {
                    self.stats.extents += 1;
                    self.stats.totals.add(&extent);
                    return Some(extent);
                }
                Ok(Resolved::LeftAlone {
                    disk_address,
                    error,
                }) => {
                    self.leave_unresolved(disk_address, &error, None);
                }
                Err(e) => {
                    self.error = Some(e);
                    return None;
                }
            }
        }
    }
}
