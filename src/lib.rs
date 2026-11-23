pub mod console;
pub mod extent;
pub mod format;
pub mod kernel;
pub mod run;
pub mod scan;

use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use console::Console;
use extent::{LeftAlone, WorthChecks};
use kernel::Filesystem;
use run::{Report, Runner};
use scan::{ScanStats, Scanner};

#[derive(Clone)]
pub struct Options {
    /// Where a subvolume of the filesystem is mounted: the top-level one, or
    /// with `whole_filesystem` any other.
    pub path: PathBuf,
    /// Scan the whole filesystem through `path` even though it is a subvolume
    /// other than the top-level one. Asked for explicitly, because anywhere
    /// else a subvolume could be taken to mean only that subvolume.
    pub whole_filesystem: bool,
    pub apply: bool,
    /// Which cost heuristics leave extents alone.
    pub worth_checks: WorthChecks,
}

/// Main entry point. Scans the filesystem and performs the reallocation operation.
pub fn run(options: &Options, console: &Rc<Console>) -> io::Result<(ScanStats, Report)> {
    let fs = Arc::new(Filesystem::open(&options.path)?);
    if fs.root != kernel::TOP_LEVEL && !options.whole_filesystem {
        return Err(io::Error::other(format!(
            "subvolume {}, not the top-level subvolume: pass --whole-filesystem to scan \
             the whole filesystem through it, every other subvolume and snapshot included",
            fs.root
        )));
    }
    if options.apply && fs.read_only {
        return Err(io::Error::other(
            "mounted read-only: --apply needs the filesystem mounted read-write",
        ));
    }
    if options.apply && fs.subvolume_read_only {
        return Err(io::Error::other(format!(
            "subvolume {} is read-only: --apply makes its copies in the subvolume it is \
             pointed at, so it needs a writable one",
            fs.root
        )));
    }
    let mut scanner = Scanner::new(&fs, console)?;
    let mut runner = Runner::new(options.clone(), &fs, console);
    // The total is only for the progress line, and the run goes on without it.
    if console.interactive() {
        let total = fs.data_used().unwrap_or_else(|e| {
            console.info(format_args!("no total for the progress line: {e}"));
            None
        });
        console.start_progress(total);
    }

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
