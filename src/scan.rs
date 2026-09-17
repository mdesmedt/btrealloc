use std::collections::HashSet;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::Options;
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

/// Walks a tree one directory at a time, handing each extent it discovers to
/// the caller and then forgetting it.
///
/// Memory stays small however big the tree: what is kept is the directories
/// still to read, the addresses of shared extents already handed out, and the
/// inodes of hardlinked files already read.
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
    /// The path to scan, when it is a single file rather than a directory.
    root_file: Option<(PathBuf, u64)>,
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
            root_file,
            handled: HashSet::new(),
            hardlinks: HashSet::new(),
            stats: ScanStats::ZERO,
        })
    }

    /// Reads one directory: every file in it is scanned, every directory in it
    /// is queued. Each extent discovered along the way goes to `on_extent`.
    ///
    /// Returns `false`, having done nothing, once the walk is complete.
    pub fn scan(&mut self, on_extent: &mut impl FnMut(Extent)) -> bool {
        if let Some((path, size)) = self.root_file.take() {
            self.add_file(&path, size, on_extent);
            return true;
        }
        let Some(dir) = self.queue.pop() else {
            return false;
        };

        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!("skipping {}: {e}", dir.display());
                return true;
            }
        };

        for entry in entries.flatten() {
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
                self.add_file(&path, meta.size(), on_extent);
            }
        }
        true
    }

    /// Scans a single file, looking up the extents it holds. Every extent not
    /// already handled is fully discovered right here, accounting for every
    /// reference to it wherever on the filesystem that reference is, and then
    /// handed to `on_extent`.
    fn add_file(&mut self, path: &Path, size: u64, on_extent: &mut impl FnMut(Extent)) {
        let path = Rc::new(path.to_path_buf());

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
        for r in refs {
            let disk_address = r.disk_address;
            if self.handled.contains(&disk_address) {
                continue;
            }
            match Extent::discover(self.fs.as_ref(), r) {
                Ok(Some(extent)) => {
                    if extent.refs.len() > 1 {
                        self.handled.insert(disk_address);
                    }
                    self.stats.extents += 1;
                    self.stats.totals.add(&extent);
                    on_extent(extent);
                }
                Ok(None) => {
                    eprintln!(
                        "extent {disk_address:#x}: shared with a snapshot or otherwise \
                         unresolvable, leaving it alone"
                    );
                    self.handled.insert(disk_address);
                    self.stats.skipped += 1;
                }
                Err(e) => {
                    eprintln!("extent {disk_address:#x}: {e}");
                    self.handled.insert(disk_address);
                    self.stats.skipped += 1;
                }
            }
        }
    }
}
