use std::collections::HashSet;
use std::fs::ReadDir;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::rc::Rc;

use crate::Options;
use crate::extent::Extent;
use crate::format::human;
use crate::kernel::{self, Filesystem, Resolved, Resolver};

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

/// What the walk has seen so far, counted as it goes: every extent is counted
/// once, when it is discovered, and nothing about it is kept afterwards.
pub struct ScanStats {
    pub files: u64,
    pub file_bytes: u64,
    pub extents: u64,
    /// Extents left alone: freed while we scanned, or shared with an inode
    /// outside this subvolume.
    pub skipped: u64,
    pub totals: Totals,
}

impl ScanStats {
    const ZERO: ScanStats = ScanStats {
        files: 0,
        file_bytes: 0,
        extents: 0,
        skipped: 0,
        totals: Totals::ZERO,
    };

    /// Prints what the scan found: the totals, then the same split by extent size class.
    pub fn report(&self) {
        let totals = &self.totals;
        println!("files:              {}", self.files);
        println!("file size:          {}", human(self.file_bytes));
        println!("extents:            {}", self.extents);
        println!("allocated:          {}", human(totals.allocated_bytes));
        println!("used:               {}", human(totals.used_bytes));
        println!("unreachable:        {}", human(totals.unreachable_bytes));
        if self.skipped != 0 {
            println!("left alone:         {} extents", self.skipped);
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

/// Walks a tree depth-first, yielding each extent it discovers, fully resolved,
/// and then forgetting it.
///
/// The walk is lazy: nothing is read ahead of the extent asked for beyond the
/// stretch of the current file it was looked up with, so whatever is done with
/// one extent, rewriting it included, is done before the walk goes on.
///
/// Memory stays small however big the tree: what is kept is the directories
/// still to read, the one being read, the file being resolved, the addresses of
/// shared extents already handed out, and the inodes of hardlinked files
/// already read.
pub struct Scanner {
    /// Options
    options: Options,
    /// Handle to the filesystem for looking up extents and their references.
    fs: Rc<Filesystem>,
    /// The device being walked: entries on any other are skipped.
    dev: u64,
    /// Directories not read yet, taken from the end, so the walk is depth-first
    /// and this stays about as long as the tree is deep.
    queue: Vec<PathBuf>,
    /// The directory being read.
    dir: Option<ReadDir>,
    /// The path to scan, when it is a single file rather than a directory.
    root_file: Option<(PathBuf, u64)>,
    /// The file whose extents are being resolved.
    file: Option<(Rc<PathBuf>, Resolver)>,
    /// Extents already dealt with that another file could lead us back to:
    /// those with more than one reference, and those left alone. An extent
    /// with a single reference can only be reached through the one file
    /// holding it, so it is never recorded here.
    handled: HashSet<u64>,
    /// Hardlinked files already read, by (device, inode). A file with a
    /// single link can only be reached once, so it is never recorded here.
    hardlinks: HashSet<(u64, u64)>,
    pub stats: ScanStats,
}

impl Scanner {
    pub fn new(options: Options, fs: Rc<Filesystem>) -> io::Result<Scanner> {
        let path = &options.path;
        let meta = std::fs::metadata(path)?;
        let (queue, root_file) = if meta.is_dir() {
            (vec![path.to_path_buf()], None)
        } else {
            (Vec::new(), Some((path.to_path_buf(), meta.size())))
        };
        Ok(Scanner {
            options,
            fs,
            dev: meta.dev(),
            queue,
            dir: None,
            root_file,
            file: None,
            handled: HashSet::new(),
            hardlinks: HashSet::new(),
            stats: ScanStats::ZERO,
        })
    }

    /// The next file to scan, with its size: the next regular file in the
    /// directory being read, or in the next directory with one. Directories met
    /// along the way are queued. `None` once the walk is complete.
    fn next_file(&mut self) -> Option<(PathBuf, u64)> {
        if let Some(file) = self.root_file.take() {
            return Some(file);
        }
        loop {
            if let Some(dir) = &mut self.dir {
                for entry in dir.flatten() {
                    let path = entry.path();
                    let Ok(meta) = entry.metadata() else { continue };

                    if meta.dev() != self.dev {
                        continue;
                    }
                    if meta.is_dir() {
                        self.queue.push(path);
                    } else if meta.is_file()
                        && (meta.nlink() <= 1 || self.hardlinks.insert((meta.dev(), meta.ino())))
                    {
                        return Some((path, meta.size()));
                    }
                }
                self.dir = None;
            }

            let dir = self.queue.pop()?;
            match std::fs::read_dir(&dir) {
                Ok(entries) => self.dir = Some(entries),
                Err(e) => eprintln!("skipping {}: {e}", dir.display()),
            }
        }
    }

    /// Starts resolving the extents of a file, all but those already handled.
    fn open_file(&mut self, path: PathBuf, size: u64) {
        let refs = match kernel::file_extents(&path) {
            Ok(refs) => refs,
            Err(e) => {
                eprintln!("skipping {}: {e}", path.display());
                return;
            }
        };

        if self.options.verbose {
            println!(
                "{}: {} bytes {} refs",
                path.display(),
                human(size),
                refs.len()
            );
        }

        self.stats.files += 1;
        self.stats.file_bytes += size;
        let refs = refs
            .into_iter()
            .filter(|r| !self.handled.contains(&r.disk_address))
            .collect();
        self.file = Some((Rc::new(path), self.fs.resolve_extents(refs)));
    }

    /// The next extent of the file being resolved, if there is one left.
    /// Extents left alone are counted and passed over.
    fn next_in_file(&mut self) -> Option<Extent> {
        let (path, resolver) = self.file.as_mut()?;
        for resolved in resolver {
            match resolved {
                Ok(Resolved::Extent(extent)) => {
                    if extent.refs.len() > 1 {
                        self.handled.insert(extent.disk_address);
                    }
                    self.stats.extents += 1;
                    self.stats.totals.add(&extent);
                    return Some(extent);
                }
                Ok(Resolved::LeftAlone {
                    disk_address,
                    error,
                }) => {
                    match error {
                        Some(e) => eprintln!("extent {disk_address:#x}: {e}"),
                        None => eprintln!(
                            "extent {disk_address:#x}: shared with a snapshot or otherwise \
                             unresolvable, leaving it alone"
                        ),
                    }
                    self.handled.insert(disk_address);
                    self.stats.skipped += 1;
                }
                Err(e) => {
                    eprintln!("skipping the rest of {}: {e}", path.display());
                    break;
                }
            }
        }
        self.file = None;
        None
    }
}

impl Iterator for Scanner {
    type Item = Extent;

    /// The next extent the walk discovers, with every reference to it,
    /// wherever on the filesystem that reference is.
    fn next(&mut self) -> Option<Extent> {
        loop {
            if let Some(extent) = self.next_in_file() {
                return Some(extent);
            }
            let (path, size) = self.next_file()?;
            self.open_file(path, size);
        }
    }
}
