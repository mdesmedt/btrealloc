use std::fs;
use std::io;
use std::iter;
use std::os::unix::fs::{DirEntryExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::vec;

use rayon::prelude::*;
use roaring::RoaringTreemap;

use crate::console::Console;
use crate::extent::{Extent, LeftAlone};
use crate::format::human;
use crate::kernel::{self, Filesystem, Unresolved};
use crate::run::Report;

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

    /// Every reason added together.
    pub fn total(&self) -> Bucket {
        let mut total = Bucket::ZERO;
        for bucket in &self.reasons {
            total.count += bucket.count;
            total.allocated_bytes += bucket.allocated_bytes;
            total.used_bytes += bucket.used_bytes;
            total.unreachable_bytes += bucket.unreachable_bytes;
        }
        total
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

    /// Prints what the scan found: the totals, then the same split by extent
    /// size class. `run` is the dry run that went with it, for what of the
    /// unreachable space it would free; the rest is left alone.
    pub fn report(&self, run: &Report, console: &Console) {
        let totals = &self.totals;
        console.print(format!("extents:            {}", self.extents));
        console.print(format!(
            "allocated:          {}",
            human(totals.allocated_bytes)
        ));
        console.print(format!("used:               {}", human(totals.used_bytes)));
        console.print(format!(
            "unreachable:        {}",
            human(totals.unreachable_bytes)
        ));
        console.print(format!("  reclaimable:      {}", human(run.freed_bytes)));
        console.print(format!(
            "  left alone:       {}",
            human(self.left_alone.total().unreachable_bytes)
        ));
        console.print(format!("  bytes to copy:    {}", human(run.copied_bytes)));

        console.print("");
        console.print("Extent size rounded down to a power of two:");
        console.print(format!(
            "{:>12}  {:>10}  {:>12}  {:>12}  {:>12}",
            "size", "count", "allocated", "used", "unreachable"
        ));
        for (log2, bucket) in totals.buckets.iter().enumerate() {
            if bucket.count == 0 {
                continue;
            }
            console.print(format!(
                "{:>12}  {:>10}  {:>12}  {:>12}  {:>12}",
                human(1 << log2),
                bucket.count,
                human(bucket.allocated_bytes),
                human(bucket.used_bytes),
                human(bucket.unreachable_bytes),
            ));
        }

        self.report_left_alone(console);
    }

    /// Prints the extents with unreachable space that were not rewritten, by
    /// reason. Unlike the totals, this still holds after an `--apply`: these
    /// are the extents the run did not rewrite.
    pub fn report_left_alone(&self, console: &Console) {
        if !self.left_alone.is_empty() {
            console.print("");
            console.print("Extents with unreachable space left alone:");
            console.print(format!(
                "{:>32}  {:>10}  {:>12}  {:>12}  {:>12}",
                "reason", "count", "allocated", "used", "unreachable"
            ));
            for reason in LeftAlone::ALL {
                let bucket = self.left_alone.get(reason);
                if bucket.count == 0 {
                    continue;
                }
                console.print(format!(
                    "{:>32}  {:>10}  {:>12}  {:>12}  {:>12}",
                    reason.label(),
                    bucket.count,
                    human(bucket.allocated_bytes),
                    human(bucket.used_bytes),
                    human(bucket.unreachable_bytes),
                ));
            }
            let total = self.left_alone.total();
            console.print(format!(
                "{:>32}  {:>10}  {:>12}  {:>12}  {:>12}",
                "total",
                total.count,
                human(total.allocated_bytes),
                human(total.used_bytes),
                human(total.unreachable_bytes),
            ));
        }
    }
}

/// The data extents a run covers, fully resolved, counted into the stats as
/// they are handed over and then forgotten: every one on the filesystem, or
/// those of one file or of every file in a directory tree. See `ExtentWalk`.
pub struct Scanner {
    fs: Arc<Filesystem>,
    console: Rc<Console>,
    walk: ExtentWalk,
    /// How many bytes of extents there are to scan, for the progress line, if
    /// that is known up front.
    pub total: Option<u64>,
    pub stats: ScanStats,
    /// Why the walk ended early, if it did.
    pub error: Option<io::Error>,
}

impl Scanner {
    /// Over every data extent on the filesystem, which is shown as `path`.
    pub fn whole_filesystem(
        fs: &Arc<Filesystem>,
        path: &Path,
        console: &Rc<Console>,
    ) -> io::Result<Scanner> {
        console.print(format!("Scanning whole filesystem: {}", path.display()));
        // The total is only for the progress line, and the run goes on
        // without it.
        let total = if console.interactive() {
            fs.data_used().unwrap_or_else(|e| {
                console.info(format_args!("no total for the progress line: {e}"));
                None
            })
        } else {
            None
        };
        Ok(Scanner {
            fs: Arc::clone(fs),
            console: Rc::clone(console),
            walk: ExtentWalk::whole_filesystem(fs)?,
            total,
            stats: ScanStats::ZERO,
            error: None,
        })
    }

    /// Over the extents of the regular file `name` in the directory `fs` was
    /// opened through, which is shown as `path`, read a page of extent items
    /// at a time.
    ///
    /// Only the file is chosen. Who else holds each of its extents is found
    /// the same way as on a scan of the whole filesystem, and a rewrite moves
    /// every holder, wherever it is: a file elsewhere sharing an extent with
    /// this one, a snapshot's included, is rewritten along with it.
    ///
    /// The file is looked up through the private mount, which has nothing
    /// mounted in it: a file mounted over this one is not what is scanned.
    pub fn file(
        fs: &Arc<Filesystem>,
        name: &Path,
        path: &Path,
        console: &Rc<Console>,
    ) -> io::Result<Scanner> {
        console.print(format!("Scanning file: {}", path.display()));
        let meta = fs.entry(name)?.metadata()?;
        if !meta.is_file() {
            return Err(io::Error::other(
                "not a regular file, under what is mounted over it",
            ));
        }
        let extents = file_extents(Arc::clone(fs), Rc::clone(console), meta.ino(), path.into());
        Ok(Scanner {
            fs: Arc::clone(fs),
            console: Rc::clone(console),
            walk: ExtentWalk::addresses(fs, extents)?,
            total: None,
            stats: ScanStats::ZERO,
            error: None,
        })
    }

    /// Over the extents of every regular file in the directory tree `fs` was
    /// opened through, which is shown as `dir`: what [`Scanner::file`] does
    /// for one file, done for every file the walk comes across. The walk
    /// stays in the subvolume the tree is in, sees past anything mounted
    /// inside it, and does not follow symbolic links.
    ///
    /// The walk reads a directory entry at a time, and a file a page of extent
    /// items at a time. An extent several files in the tree point into, or one
    /// file from many places, is met once: the walk keeps the address of every
    /// extent it has met, in a compressed bitmap of a few bytes each, and
    /// nothing else that grows with the tree. See `ExtentWalk`.
    pub fn tree(fs: &Arc<Filesystem>, dir: &Path, console: &Rc<Console>) -> io::Result<Scanner> {
        console.print(format!("Recursively scanning: {}", dir.display()));
        let files = Files::new(fs, dir, console)?;
        let extents = {
            let (fs, console) = (Arc::clone(fs), Rc::clone(console));
            files.flat_map(move |(inode, path)| {
                file_extents(Arc::clone(&fs), Rc::clone(&console), inode, path)
            })
        };
        Ok(Scanner {
            fs: Arc::clone(fs),
            console: Rc::clone(console),
            walk: ExtentWalk::addresses(fs, extents)?,
            total: None,
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
                self.leave_unresolved(extent.disk_address, error, Some(extent));
                None
            }
        }
    }

    /// Counts an extent whose holders could not be resolved, and warns why,
    /// unless it merely changed under us: on a filesystem in use that is
    /// routine, and the table at the end counts it, so it is only an info.
    ///
    /// An error that means the filesystem is failing ends the walk instead:
    /// see [`kernel::fatal`].
    fn leave_unresolved(&mut self, disk_address: u64, error: io::Error, extent: Option<&Extent>) {
        if let Some(hint) = kernel::fatal(&error) {
            let message = format!("stopped at extent {disk_address:#x}: {error}: {hint}");
            self.error = Some(io::Error::new(error.kind(), message));
            return;
        }
        let reason = if kernel::is_changed(&error) {
            self.console.info(format_args!(
                "extent {disk_address:#x}: {error}, leaving it alone"
            ));
            LeftAlone::Changed
        } else {
            self.console.warn(format!(
                "extent {disk_address:#x}: {error}, leaving it alone"
            ));
            LeftAlone::Unresolved
        };
        self.stats.left_alone.add(reason, extent);
    }
}

impl Iterator for Scanner {
    type Item = Extent;

    /// The next extent the walk discovers, with every reference to it,
    /// wherever on the filesystem that reference is. Extents left alone are
    /// counted and passed over. Nothing more once the walk has ended in
    /// [`Scanner::error`].
    fn next(&mut self) -> Option<Extent> {
        loop {
            if self.error.is_some() {
                return None;
            }
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
                    self.leave_unresolved(disk_address, error, None);
                }
                Err(e) => {
                    self.error = Some(e);
                    return None;
                }
            }
        }
    }
}

/// One extent, as [`ExtentWalk`] resolves it.
enum Resolved {
    /// The extent, with every reference to it, wherever on the filesystem.
    /// Unless it is fully used: then perhaps only those that show it is.
    Extent(Extent),
    /// Nothing safe can be said about the extent at this address, for the
    /// reason `error` gives: it changed while it was being read, or looking it
    /// up failed.
    LeftAlone { disk_address: u64, error: io::Error },
}

impl Resolved {
    fn from(fs: &Filesystem, extent: Unresolved) -> Resolved {
        let disk_address = extent.disk_address();
        match fs.resolve(extent) {
            Ok(extent) => Resolved::Extent(extent),
            Err(error) => Resolved::LeftAlone {
                disk_address,
                error,
            },
        }
    }
}

/// Data extents read from the extent tree a batch at a time as they are asked
/// for, each resolved on rayon's global thread pool: every one on the
/// filesystem in address order, or the ones at the addresses an iterator
/// hands over (see [`Source`]).
///
/// Each extent is met once, however many files and snapshots reference it,
/// and is handed over with every one of those references, or for a fully used
/// extent perhaps only enough of them to show it is. A walk of the whole tree
/// meets each extent once by reading the tree in order, and keeps nothing about
/// it afterwards. A walk over addresses, which can name an extent any number
/// of times, keeps the address of every extent it has met, and nothing else.
/// An extent can be dealt with, rewritten even, before the next batch is read.
///
/// A batch's extents are resolved all at once, in parallel, and handed over
/// only when every one of them is. Nothing is resolved while a batch is being
/// handed over, so whatever is done with an extent, a rewrite included, never
/// runs alongside a backreference walk. The kernel can hang when a dedupe and
/// a backreference walk meet on the same inode or extent, and which inodes a
/// walk will reach is not known ahead of it. The hang is unfixed on every
/// kernel so far: see "Hangs with concurrent `LOGICAL_INO` and dedupe/clone"
/// in <https://github.com/Zygo/bees/blob/master/docs/btrfs-kernel.md>.
///
/// The walk runs over a tree that changes under it, our own rewrites
/// included. It resumes from an address rather than a place in the tree, so
/// extents freed or allocated behind it make no difference. Extents allocated
/// ahead of it after it started, our own copies among them, are passed over:
/// they are fully referenced when they are made, and counting them would count
/// the same data twice.
///
/// An `Err` is a search over the extent tree that failed outright. It ends the
/// walk.
struct ExtentWalk {
    fs: Arc<Filesystem>,
    /// Where the extents still to come are found.
    source: Source,
    /// Extents allocated after this generation are passed over.
    generation: u64,
    /// The batch resolved last, less the extents already handed over.
    batch: vec::IntoIter<Resolved>,
}

/// Where an [`ExtentWalk`] finds its extents.
enum Source {
    /// The whole extent tree, from this address on, or nothing more once it
    /// is read to its end (`None`).
    ExtentTree(Option<u64>),
    /// The addresses `addresses` hands over, each looked up in the extent tree
    /// on its own.
    Addresses {
        addresses: Box<dyn Iterator<Item = u64>>,
        /// Every address met so far, each divided by the sector size: an
        /// extent is named again every time another file extent item points
        /// into it, as a file partly overwritten again and again does from all
        /// over, and is to be met only once.
        ///
        /// Extents start a sector apart at the closest, and are seldom that
        /// close, so numbered by sector they are sparse but in clusters, which
        /// is what a compressed bitmap keeps in a few bytes each.
        seen: RoaringTreemap,
    },
}

/// How many addresses not met before are resolved at once, as one batch. About
/// as many as one search of the extent tree reads.
const ADDRESS_BATCH: usize = 1024;

impl ExtentWalk {
    /// Every data extent on the filesystem, in address order.
    fn whole_filesystem(fs: &Arc<Filesystem>) -> io::Result<ExtentWalk> {
        ExtentWalk::new(fs, Source::ExtentTree(Some(0)))
    }

    /// The data extents at the addresses `addresses` hands over, as it hands
    /// them over, each once however many times it is named. An address with
    /// no data extent at it any more is passed over.
    ///
    /// `addresses` is only read from once the filesystem is synced, so what it
    /// reads from the trees includes everything written before the walk.
    fn addresses(
        fs: &Arc<Filesystem>,
        addresses: impl Iterator<Item = u64> + 'static,
    ) -> io::Result<ExtentWalk> {
        let source = Source::Addresses {
            addresses: Box::new(addresses),
            seen: RoaringTreemap::new(),
        };
        ExtentWalk::new(fs, source)
    }

    fn new(fs: &Arc<Filesystem>, source: Source) -> io::Result<ExtentWalk> {
        // Commit what is pending first, so that everything allocated from here
        // on, our own copies included, is newer than the generation read next.
        fs.sync()?;
        Ok(ExtentWalk {
            fs: Arc::clone(fs),
            source,
            generation: fs.generation()?,
            batch: Vec::new().into_iter(),
        })
    }
}

impl Iterator for ExtentWalk {
    type Item = io::Result<Resolved>;

    fn next(&mut self) -> Option<io::Result<Resolved>> {
        loop {
            if let Some(resolved) = self.batch.next() {
                return Some(Ok(resolved));
            }

            let fs = &self.fs;
            let generation = self.generation;
            let resolved: Vec<Resolved> = match &mut self.source {
                Source::ExtentTree(next) => match fs.extent_batch((*next)?, generation) {
                    Ok((batch, after)) => {
                        *next = after;
                        // Collected in the order read, which is address order.
                        batch
                            .into_par_iter()
                            .map(|extent| Resolved::from(fs, extent))
                            .collect()
                    }
                    Err(e) => {
                        *next = None;
                        return Some(Err(e));
                    }
                },
                Source::Addresses { addresses, seen } => {
                    let sectorsize = fs.sectorsize;
                    let mut batch = Vec::with_capacity(ADDRESS_BATCH);
                    for address in addresses.by_ref() {
                        if seen.insert(address / sectorsize) {
                            batch.push(address);
                            if batch.len() == ADDRESS_BATCH {
                                break;
                            }
                        }
                    }
                    if batch.is_empty() {
                        return None;
                    }
                    // In address order within the batch.
                    batch.sort_unstable();
                    // A lookup that fails outright ends the walk, as it does a
                    // walk of the whole tree.
                    let resolved: io::Result<Vec<Option<Resolved>>> = batch
                        .into_par_iter()
                        .map(|address| {
                            let extent = fs.extent_at(address, generation)?;
                            Ok(extent.map(|extent| Resolved::from(fs, extent)))
                        })
                        .collect();
                    match resolved {
                        Ok(resolved) => resolved.into_iter().flatten().collect(),
                        Err(e) => {
                            *addresses = Box::new(iter::empty());
                            return Some(Err(e));
                        }
                    }
                }
            };
            self.batch = resolved.into_iter();
        }
    }
}

/// The extents of inode `inode` of the subvolume `fs` was opened in, shown as
/// `path`, read a page of extent items at a time.
///
/// A file that cannot be read is warned about and left where it failed. What
/// came before is kept: each extent is resolved again before anything is done
/// with it, so a file read only in part is no different from one shrunk since.
fn file_extents(
    fs: Arc<Filesystem>,
    console: Rc<Console>,
    inode: u64,
    path: PathBuf,
) -> impl Iterator<Item = u64> {
    let mut from = Some(0);
    iter::from_fn(move || {
        let mut page = Vec::new();
        match fs.inode_extent_page(inode, from?, |address| page.push(address)) {
            Ok(next) => from = next,
            Err(e) => {
                console.warn(format!("skipping the rest of {}: {e}", path.display()));
                from = None;
            }
        }
        Some(page)
    })
    .flatten()
}

/// The regular files in a directory tree, depth first, each as its inode
/// number and the path it is shown by. A file with more than one name in the
/// tree, a hardlink, comes up once for each.
///
/// The tree is read through the private mount, which has nothing mounted in
/// it: what is mounted inside the tree, a bind mount of somewhere else on the
/// same subvolume included, is neither entered nor in the way of what it
/// hides. The walk stays in the subvolume the tree is in: a directory that is
/// the root of another subvolume nested inside this one, as snapper keeps its
/// snapshots, shows as another device and is not entered. Symbolic links are
/// not followed.
struct Files {
    fs: Arc<Filesystem>,
    console: Rc<Console>,
    /// The tree's root, as shown.
    dir: PathBuf,
    /// The directories being read, the tree's root first and the one being
    /// read now last, each by its path from the tree's root.
    open: Vec<(PathBuf, fs::ReadDir)>,
}

impl Files {
    fn new(fs: &Arc<Filesystem>, dir: &Path, console: &Rc<Console>) -> io::Result<Files> {
        Ok(Files {
            fs: Arc::clone(fs),
            console: Rc::clone(console),
            dir: dir.into(),
            open: vec![(PathBuf::new(), fs::read_dir(fs.private_path())?)],
        })
    }
}

impl Iterator for Files {
    type Item = (u64, PathBuf);

    fn next(&mut self) -> Option<(u64, PathBuf)> {
        loop {
            let (rel, entries) = self.open.last_mut()?;
            let entry = match entries.next() {
                Some(Ok(entry)) => entry,
                Some(Err(e)) => {
                    let shown = self.dir.join(rel);
                    self.console
                        .warn(format!("skipping the rest of {}: {e}", shown.display()));
                    self.open.pop();
                    continue;
                }
                None => {
                    self.open.pop();
                    continue;
                }
            };
            let rel = rel.join(entry.file_name());
            let shown = self.dir.join(&rel);
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(e) => {
                    self.console
                        .warn(format!("skipping {}: {e}", shown.display()));
                    continue;
                }
            };
            if file_type.is_file() {
                // The directory entry names the inode: no need to stat it.
                return Some((entry.ino(), shown));
            }
            if !file_type.is_dir() {
                continue;
            }
            let entered = entry.metadata().and_then(|meta| {
                if meta.dev() != self.fs.device {
                    return Ok(None);
                }
                fs::read_dir(self.fs.private_path().join(&rel)).map(Some)
            });
            match entered {
                Ok(Some(entries)) => self.open.push((rel, entries)),
                Ok(None) => self.console.info(format_args!(
                    "not entering {}: another subvolume",
                    shown.display()
                )),
                Err(e) => self
                    .console
                    .warn(format!("skipping {}: {e}", shown.display())),
            }
        }
    }
}
