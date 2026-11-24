pub mod console;
pub mod extent;
pub mod format;
pub mod kernel;
pub mod run;
pub mod scan;

use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use console::Console;
use extent::{LeftAlone, WorthChecks};
use kernel::Filesystem;
use run::{Report, Runner};
use scan::{ScanStats, Scanner};

#[derive(Clone)]
pub struct Options {
    /// What to scan: where the top-level subvolume is mounted for the whole
    /// filesystem, a directory for every file in its tree, or a single file.
    pub path: PathBuf,
    /// Scan the whole filesystem through `path`, the root of a subvolume other
    /// than the top-level one, rather than only that subvolume's tree.
    pub whole_filesystem: bool,
    pub apply: bool,
    /// Which cost heuristics leave extents alone.
    pub worth_checks: WorthChecks,
}

/// The filesystem `options.path` is on, and a scanner over what that path asks
/// for. Refused with what to do instead if it makes no sense.
///
/// - The top-level subvolume's root, or any subvolume's root with
///   `--whole-filesystem`: the whole filesystem, opened through it.
/// - Any other directory: every file in its tree, the filesystem opened
///   through it.
/// - A file: just that file, the filesystem opened through the directory it is
///   in.
///
/// A symbolic link given as the path is followed, to the file or directory it
/// leads to, wherever that is: the path is resolved to where it really is
/// before anything else, and is shown that way.
fn init_scan(options: &Options, console: &Rc<Console>) -> io::Result<(Arc<Filesystem>, Scanner)> {
    let path = std::fs::canonicalize(&options.path)?;
    let meta = std::fs::metadata(&path)?;
    if meta.is_file() {
        if options.whole_filesystem {
            return Err(io::Error::other(
                "not a directory: --whole-filesystem needs the mount point of a subvolume",
            ));
        }
        // A resolved path to a file always has a parent and a name.
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(io::Error::other("a file with no directory"));
        };
        let fs = Arc::new(Filesystem::open(dir)?);
        check_apply(options, &fs)?;
        let scanner = Scanner::file(&fs, Path::new(name), &path, console)?;
        return Ok((fs, scanner));
    }
    if !meta.is_dir() {
        return Err(io::Error::other("not a regular file or a directory"));
    }
    let fs = Arc::new(Filesystem::open(&path)?);
    let whole_filesystem = match (fs.at_subvolume_root, options.whole_filesystem) {
        // The top-level subvolume's tree reaches every other subvolume: the
        // whole filesystem, read far more cheaply from the extent tree.
        (true, _) if fs.root == kernel::TOP_LEVEL => true,
        (true, true) => true,
        (false, true) => {
            return Err(io::Error::other(format!(
                "a directory inside subvolume {}, not its root: --whole-filesystem needs \
                 the mount point of a subvolume",
                fs.root
            )));
        }
        (_, false) => false,
    };
    check_apply(options, &fs)?;
    let scanner = if whole_filesystem {
        Scanner::whole_filesystem(&fs, &path, console)?
    } else {
        Scanner::tree(&fs, &path, console)?
    };
    Ok((fs, scanner))
}

/// Refuses `--apply` on a filesystem it could not write to.
fn check_apply(options: &Options, fs: &Filesystem) -> io::Result<()> {
    if !options.apply {
        return Ok(());
    }
    if fs.read_only {
        return Err(io::Error::other(
            "mounted read-only: --apply needs the filesystem mounted read-write",
        ));
    }
    if fs.subvolume_read_only {
        return Err(io::Error::other(format!(
            "subvolume {} is read-only: --apply makes its copies in the subvolume it is \
             pointed at, so it needs a writable one",
            fs.root
        )));
    }
    Ok(())
}

/// Main entry point. Scans the filesystem and performs the reallocation operation.
pub fn run(options: &Options, console: &Rc<Console>) -> io::Result<(ScanStats, Report)> {
    let (fs, mut scanner) = init_scan(options, console)?;
    let mut runner = Runner::new(options.clone(), &fs, console);
    console.start_progress(scanner.total);

    while let Some(extent) = scanner.next() {
        console.update_progress(scanner.stats.totals.allocated_bytes, scanner.stats.extents);
        // Nothing to reclaim, so nothing is being left behind.
        if extent.disk_free_bytes() == 0 {
            continue;
        }
        if let Some(reason) = extent.not_worth_rewriting(options.worth_checks) {
            scanner.stats.left_alone.add(reason, Some(&extent));
            continue;
        }
        // Resolve the extent again before processing
        let Some(extent) = scanner.resolve_again(&extent) else {
            continue;
        };
        if extent.disk_free_bytes() == 0 {
            continue;
        }
        let reason = match extent.not_worth_rewriting(options.worth_checks) {
            Some(reason) => Some(reason),
            None => match runner.process(&extent) {
                Ok(reason) => reason,
                // Nothing after this extent could be rewritten either. The
                // walk ends here, and the run with it.
                Err(e) => {
                    scanner
                        .stats
                        .left_alone
                        .add(LeftAlone::Failed, Some(&extent));
                    scanner.error = Some(e);
                    break;
                }
            },
        };
        if let Some(reason) = reason {
            scanner.stats.left_alone.add(reason, Some(&extent));
        }
    }

    console.end_progress();
    let report = runner.finish();

    if let Some(e) = scanner.error.take() {
        return Err(e);
    }

    // Commit pending fs operations with a sync.
    if options.apply {
        fs.sync()?;
    }

    Ok((scanner.stats, report))
}
