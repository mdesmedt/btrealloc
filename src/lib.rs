pub mod extent;
pub mod format;
pub mod kernel;
pub mod run;
pub mod scan;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use extent::LeftAlone;
use kernel::Filesystem;
use run::{Report, Runner};
use scan::{ScanStats, Scanner};

#[derive(Clone)]
pub struct Options {
    /// Where the filesystem's top-level subvolume is mounted.
    pub path: PathBuf,
    pub apply: bool,
}

/// Main entry point. Scans the filesystem and performs the reallocation operation.
pub fn run(options: &Options) -> io::Result<(ScanStats, Report)> {
    let fs = Arc::new(Filesystem::open(&options.path)?);
    if options.apply && fs.read_only {
        return Err(io::Error::other(
            "mounted read-only: --apply needs the filesystem mounted read-write",
        ));
    }
    let mut scanner = Scanner::new(&fs)?;
    let mut runner = Runner::new(options.clone(), &fs);

    while let Some(extent) = scanner.next() {
        // Nothing to reclaim, so nothing is being left behind.
        if extent.disk_free_bytes() == 0 {
            continue;
        }
        if let Some(reason) = extent.not_worth_rewriting() {
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
        let reason = match extent.not_worth_rewriting() {
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
