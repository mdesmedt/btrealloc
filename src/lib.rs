pub mod extent;
pub mod format;
pub mod kernel;
pub mod run;
pub mod scan;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use kernel::Filesystem;
use run::{Report, Runner};
use scan::{ScanStats, Scanner};

#[derive(Clone)]
pub struct Options {
    /// Where the filesystem's top-level subvolume is mounted.
    pub path: PathBuf,
    pub dryrun: bool,
    pub apply: bool,
    pub verbose: bool,
}

/// Main entry point. Scans the filesystem and performs the reallocation operation.
pub fn run(options: &Options) -> io::Result<(ScanStats, Report)> {
    let fs = Arc::new(Filesystem::open(&options.path)?);
    if options.apply && fs.read_only {
        return Err(io::Error::other(
            "mounted read-only: --apply needs the filesystem mounted read-write",
        ));
    }
    let mut scanner = Scanner::new(&fs, options)?;
    let mut runner = (options.dryrun || options.apply).then(|| Runner::new(options.clone(), &fs));

    while let Some(extent) = scanner.next() {
        // Nothing to reclaim, so nothing is being left behind.
        if extent.disk_free_bytes() == 0 {
            continue;
        }
        if let Some(reason) = extent.not_worth_rewriting() {
            scanner.stats.left_alone.add(reason, Some(&extent));
            continue;
        }
        let Some(runner) = runner.as_mut() else {
            continue;
        };
        // Resolve the extent again before processing
        let Some(extent) = scanner.resolve_again(&extent) else {
            continue;
        };
        if extent.disk_free_bytes() == 0 {
            continue;
        }
        let reason = extent
            .not_worth_rewriting()
            .or_else(|| runner.process(&extent));
        if let Some(reason) = reason {
            scanner.stats.left_alone.add(reason, Some(&extent));
        }
    }

    let report = runner.map(Runner::finish).unwrap_or_default();
    if let Some(e) = scanner.error.take() {
        return Err(e);
    }
    Ok((scanner.stats, report))
}
