pub mod extent;
pub mod format;
pub mod kernel;
pub mod run;
pub mod scan;

use std::io;
use std::path::PathBuf;
use std::rc::Rc;

use kernel::Filesystem;
use run::{Report, Runner};
use scan::{ScanStats, Scanner};

#[derive(Clone)]
pub struct Options {
    pub path: PathBuf,
    pub dryrun: bool,
    pub apply: bool,
    pub verify: bool,
    pub verbose: bool,
}

/// Main entry point. Scans the filesystem and performs the reallocation operation.
pub fn run(options: &Options) -> io::Result<(ScanStats, Report)> {
    // Open a handle anywhere on the filesystem for fetching extent metadata
    let fs = Rc::new(Filesystem::open(&options.path)?);
    let mut scanner = Scanner::new(options.clone(), fs.clone())?;
    let mut runner = (options.dryrun || options.apply).then(|| Runner::new(options.clone(), fs));

    // Loop the scanner
    while scanner.scan(&mut |extent| {
        if let Some(runner) = runner.as_mut()
            && extent.worth_rewriting()
        {
            runner.process(&extent);
        }
    }) {}

    let report = runner.map(Runner::finish).unwrap_or_default();
    Ok((scanner.stats, report))
}
