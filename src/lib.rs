pub mod format;
pub mod kernel;
pub mod run;
pub mod scan;
pub mod worklist;

use std::collections::HashSet;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use kernel::Filesystem;
use run::Report;
use scan::Scan;

pub struct Options {
    pub path: PathBuf,
    pub dryrun: bool,
    pub apply: bool,
    pub verify: bool,
    pub verbose: bool,
}

/// Produces the Scan results for a given path.
///
/// Nothing is printed here: what the scan found is a result, returned whole, and
/// whoever asked for it decides how to show it.
pub fn scan(options: &Options) -> io::Result<Scan> {
    let path = &options.path;

    // Read metadata
    let meta = std::fs::metadata(path)?;

    // Open a handle anywhere on the filesystem for fetching extent metadata later
    let fs = Filesystem::open(path)?;

    // Scan
    let mut scan = Scan::new(fs);
    if meta.is_dir() {
        scan.walk(path, meta.dev(), &mut HashSet::new());
    } else {
        scan.add_file(path, meta.size());
    }

    // Classify
    scan.classify();

    Ok(scan)
}

/// Creates the worklist and processes it, if --dryrun or --apply is specified.
/// Returns what the run did, which is empty when it was asked to do nothing.
pub fn run(options: &Options, scan: &Scan) -> Report {
    if options.dryrun || options.apply {
        // Create the jobs from the scan results
        let jobs = worklist::create_jobs(scan);
        // Process each extent
        run::run(&jobs, options, &scan.fs)
    } else {
        Report::default()
    }
}
