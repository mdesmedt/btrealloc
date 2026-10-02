use std::fs::File;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::Options;
use crate::extent::{Extent, ExtentRef, Holder, LeftAlone};
use crate::format::human;
use crate::kernel::{self, Filesystem};

/// Processes extents one at a time, as the scan hands them over, and keeps
/// the account of what it did.
///
/// `fs` is here to find each holder's path, for its sector size (a holder
/// ending mid-sector needs [`redirect_tail`], and where that boundary falls is
/// the filesystem's to say), and for the mount the temporary copies are made
/// in.
pub struct Runner {
    options: Options,
    fs: Arc<Filesystem>,
    report: Report,
}

impl Runner {
    pub fn new(options: Options, fs: &Arc<Filesystem>) -> Runner {
        if options.apply {
            println!("rewriting extents as they are found");
        } else {
            println!("dry run: nothing will be written");
        }
        Runner {
            options,
            fs: Arc::clone(fs),
            report: Report::default(),
        }
    }

    /// Reallocates one extent, or on a dry run says what doing so would do.
    /// Returns why it was left alone, if it was.
    pub fn process(&mut self, extent: &Extent) -> Option<LeftAlone> {
        let (options, report) = (&self.options, &mut self.report);
        let holders = match locate_holders(extent, &self.fs) {
            Ok(Ok(holders)) => holders,
            Ok(Err(reason)) => {
                if options.verbose {
                    println!(
                        "extent {:#x}: {}, leaving it alone",
                        extent.disk_address,
                        reason.label()
                    );
                }
                return Some(reason);
            }
            Err(failure) => {
                report.failed(failure);
                return Some(LeftAlone::Failed);
            }
        };

        if options.apply {
            // Reallocate the extent for real
            let result = realloc_extent(extent, &holders, self.fs.as_ref());
            // Check for failure, and move on to the next extent if it failed
            if let Err(failure) = result {
                report.failed(failure);
                return Some(LeftAlone::Failed);
            }
        }

        println!("{}", describe(extent, &holders, options));

        report.copied_bytes += extent.live_uncompressed_bytes();
        report.freed_bytes += extent.disk_free_bytes();
        report.rewritten.push(extent.disk_address);

        if options.verbose {
            println!(
                "    extent {:#x}, {} on disk",
                extent.disk_address,
                human(extent.disk_bytes),
            );
            for holder in &holders {
                for r in &holder.holder.refs {
                    println!(
                        "    {} at offset {} in {}",
                        human(r.num_bytes),
                        r.file_offset,
                        holder.path.display(),
                    );
                }
            }
        }
        None
    }

    /// Prints the summary of the whole run and returns its account.
    pub fn finish(self) -> Report {
        let report = self.report;
        let extents = report.rewritten.len();
        println!();
        if self.options.apply {
            println!(
                "copied {} to free {} from {extents} extents",
                human(report.copied_bytes),
                human(report.freed_bytes),
            );
        } else {
            println!(
                "would copy {} to free {} from {extents} extents",
                human(report.copied_bytes),
                human(report.freed_bytes),
            );
        }
        if !report.skipped.is_empty() {
            println!("{} extents could not be rewritten", report.skipped.len());
        }
        if !report.modified.is_empty() {
            println!(
                "{} files were modified by something else while they were being rewritten",
                report.modified.len()
            );
        }
        report
    }
}

/// A file holding the extent, and the path it was found at.
struct Located<'a> {
    /// Relative to the top-level subvolume, which is how it is opened.
    name: PathBuf,
    /// Where it is under the mount the tool was pointed at, for naming it.
    path: PathBuf,
    holder: Holder<'a>,
    fs: &'a Filesystem,
}

impl Located<'_> {
    /// Opens the file, as long as it is still the one the extent tree names.
    ///
    /// Read-only is enough: the kernel lets CAP_SYS_ADMIN dedupe into a file it
    /// has not opened for writing, a read-only snapshot's included. Opening for
    /// writing would fail on a running executable, and tell inotify watchers
    /// the file was written.
    ///
    /// Opened afresh wherever it is needed rather than held: a deduplicator can
    /// spread one extent over thousands of files, and a descriptor held for
    /// each of them is what runs into `RLIMIT_NOFILE`.
    fn open(&self) -> Result<File, Failure> {
        self.fs
            .open_inode(&self.name, self.holder.root, self.holder.inode)
            .map_err(|e| Failure::new(&self.path, e))
    }
}

/// Finds a path to every file holding the extent, or why one of them means it
/// cannot be rewritten.
fn locate_holders<'a>(
    extent: &'a Extent,
    fs: &'a Filesystem,
) -> Result<Result<Vec<Located<'a>>, LeftAlone>, Failure> {
    let mut located = Vec::new();
    for holder in extent.holders() {
        let name = match fs.inode_path(holder.root, holder.inode) {
            Ok(Some(name)) => name,
            Ok(None) => return Err(Failure::other(&unnamed(&holder), "no path leads to it")),
            // Deleted, perhaps still open somewhere. It cannot be opened, and
            // its extents are freed once it is closed.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Err(LeftAlone::Deleted)),
            Err(e) => return Err(Failure::new(&unnamed(&holder), e)),
        };
        let path = fs.display_path(&name);
        let holder = Located {
            name,
            path,
            holder,
            fs,
        };
        let file = holder.open()?;
        let error = |e| Failure::new(&holder.path, e);
        // A nodatacow file is overwritten in place and cannot be deduped.
        if kernel::is_nocow(&file).map_err(error)? {
            return Ok(Err(LeftAlone::NoDataCow));
        }
        // A dedupe stops at the file's last sector, so space reserved past it
        // can never be moved.
        let end = file.metadata().map_err(error)?.len();
        let end = end.next_multiple_of(fs.sectorsize);
        if holder
            .holder
            .refs
            .iter()
            .any(|r| r.file_offset + r.num_bytes > end)
        {
            return Ok(Err(LeftAlone::PastEnd));
        }
        located.push(holder);
    }
    Ok(Ok(located))
}

/// What a holder there is no path to is called instead.
fn unnamed(holder: &Holder) -> PathBuf {
    PathBuf::from(format!(
        "<inode {} of subvolume {}>",
        holder.inode, holder.root
    ))
}

/// Copies an extent's live ranges into temporary files, then points the live ranges from
/// every file holding that extent at the copy. No original inode is ever replaced. Each
/// keeps its identity, owner, times and xattrs, and only its extent references change.
fn realloc_extent(extent: &Extent, holders: &[Located], fs: &Filesystem) -> Result<(), Failure> {
    let liveranges = read_liveranges(extent, holders, fs)?;

    // What every holder looked like beforehand for checking after we reallocate.
    let before: Vec<HolderBefore> = holders
        .iter()
        .map(HolderBefore::read)
        .collect::<Result<_, _>>()?;

    // Iterate over live ranges
    for liverange in &liveranges {
        // Copy the live range into a temporary file
        let temp = stage(&holders[0].path, &liverange.bytes, fs)?;
        // Then point every holder at the newly allocated data
        for (holder, before) in holders.iter().zip(&before) {
            redirect_chunks(holder, &temp, liverange, before.filesize)?;
        }
    }

    for (holder, before) in holders.iter().zip(&before) {
        finish_holder(holder, before, fs)?;
    }

    // A dedupe reports success whether or not it moved anything, so only the
    // holders themselves can say the extent is free.
    for holder in holders {
        let refs = &holder.holder.refs;
        let from = refs.iter().map(|r| r.file_offset).min().unwrap_or(0);
        let to = refs
            .iter()
            .map(|r| r.file_offset + r.num_bytes - 1)
            .max()
            .unwrap_or(0);
        let (root, inode) = (holder.holder.root, holder.holder.inode);
        if fs
            .inode_holds_extent(root, inode, from, to, extent.disk_address)
            .map_err(|e| Failure::new(&holder.path, e))?
        {
            return Err(Failure::other(&holder.path, "still holds the extent"));
        }
    }
    Ok(())
}

/// One live stretch of the extent, held in memory until it is staged.
///
/// Read once, up front, and written out to a temporary only for as long as the
/// holders of that stretch are being pointed at it. An extent whose survivors
/// are scattered has a stretch for each of them, and a file open for every one
/// of those at once is what runs into `RLIMIT_NOFILE` on the filesystems this
/// tool is for.
struct LiveRange {
    /// `[start, end)` of the extent, which is what a staged temporary holds
    /// one sector in.
    bytes: Vec<u8>,
    start: u64,
    end: u64,
}

/// Reads each live stretch of the extent into memory, ready to be staged one at
/// a time.
///
/// A stretch keeps its own copy rather than being packed in with the others:
/// stretches held by different files have different lifetimes, and putting them
/// in one extent would rebuild the part-dead extent this is meant to take apart.
fn read_liveranges(
    extent: &Extent,
    holders: &[Located],
    fs: &Filesystem,
) -> Result<Vec<LiveRange>, Failure> {
    let mut refs: Vec<(&ExtentRef, &Located)> = holders
        .iter()
        .flat_map(|holder| holder.holder.refs.iter().map(move |r| (*r, holder)))
        .collect();
    refs.sort_unstable_by_key(|(r, _)| r.extent_offset);

    let mut copies = Vec::new();
    for range in &extent.live_ranges {
        let mut start = range.start;
        while start < range.end {
            let mut bytes = vec![0u8; (range.end - start) as usize];
            let read = fill(&refs, start, range.end, &mut bytes)?;
            // Holders are pointed at whole sectors, so a part sector at the end
            // of a stretch cut short by a file's end is one nothing can ever
            // reference. Dropping it here keeps it from ever being written out;
            // the holder it belongs to gets its last sector from
            // [`redirect_tail`] instead.
            let end = read / fs.sectorsize * fs.sectorsize;
            if end > start {
                bytes.truncate((end - start) as usize);
                copies.push(LiveRange { bytes, start, end });
            }
            // A file's end can leave a gap in the stretch. What follows it
            // starts with the next reference past the gap.
            start = refs
                .iter()
                .map(|(r, _)| r.extent_offset)
                .filter(|&offset| offset > read)
                .min()
                .unwrap_or(range.end);
        }
    }
    Ok(copies)
}

/// A rewrite that did not happen, and the file it is charged to.
struct Failure {
    path: PathBuf,
    error: io::Error,
}

impl Failure {
    fn new(path: &Path, error: impl Into<io::Error>) -> Failure {
        Failure {
            path: path.to_path_buf(),
            error: error.into(),
        }
    }

    fn other(path: &Path, message: &str) -> Failure {
        Failure::new(path, io::Error::other(message))
    }

    /// The file was written to by something else while it was being rewritten.
    fn modified(path: &Path, message: &str) -> Failure {
        Failure::new(path, io::Error::new(io::ErrorKind::InvalidData, message))
    }
}

/// What a holder held before anything moved, to check it against afterwards.
struct HolderBefore {
    filesize: u64,
    modified: std::time::SystemTime,
}

impl HolderBefore {
    /// Reads a holder's metadata.
    ///
    /// Taken once for the whole extent rather than per stretch: the checks in
    /// [`finish_holder`] need something from before anything moved, and a
    /// stretch is staged at a time, so there is no one place downstream that
    /// still sees the file untouched.
    fn read(holder: &Located) -> Result<HolderBefore, Failure> {
        let path = &holder.path;
        let file = holder.open()?;
        let metadata = file.metadata().map_err(|e| Failure::new(path, e))?;
        Ok(HolderBefore {
            filesize: metadata.len(),
            modified: metadata.modified().map_err(|e| Failure::new(path, e))?,
        })
    }
}

/// Writes `bytes` out to a temporary of its own, ready to be deduped from and
/// dropped again. A failure is charged to `path`.
///
/// The temporary is made at the top of the mount, which is writable wherever
/// the holders are: a read-only snapshot has no room for one.
///
/// The bytes sit one sector in, with a hole before them. A file of a sector or
/// less can be written into the metadata leaf instead of an extent of its own,
/// and a dedupe from an inline extent shares nothing: it rewrites the holder's
/// page, which keeps the old extent until writeback. At over a sector long the
/// temporary is never inlined, and still ends exactly where the bytes do, which
/// [`redirect_tail`] needs. The hole costs nothing.
fn stage(path: &Path, bytes: &[u8], fs: &Filesystem) -> Result<File, Failure> {
    let temp = fs.temp_file().map_err(|e| Failure::new(path, e))?;
    temp.write_all_at(bytes, fs.sectorsize)
        .map_err(|e| Failure::new(path, e))?;
    Ok(temp)
}

/// Reads `[start, end)` of the extent into `buf`, taking each part from a file
/// that holds it. Returns how far it got: a holder's last extent can run past
/// the end of its data, so there may be less to read than the stretch claims.
fn fill(
    refs: &[(&ExtentRef, &Located)],
    start: u64,
    end: u64,
    buf: &mut [u8],
) -> Result<u64, Failure> {
    let mut at = start;
    for (r, holder) in refs {
        if at >= end {
            break;
        }
        if r.extent_offset > at || r.extent_offset + r.num_bytes <= at {
            continue;
        }
        let path = &holder.path;
        let file = holder.open()?;
        let size = file.metadata().map_err(|e| Failure::new(path, e))?.len();
        let file_offset = r.file_offset + (at - r.extent_offset);
        let to = end
            .min(r.extent_offset + r.num_bytes)
            .min(at + size.saturating_sub(file_offset));
        if to <= at {
            continue;
        }
        let (lo, hi) = ((at - start) as usize, (to - start) as usize);
        file.read_exact_at(&mut buf[lo..hi], file_offset)
            .map_err(|e| Failure::new(path, e))?;
        at = to;
    }
    Ok(at)
}

/// Points a holder's last sector at a copy of its own, for a file whose size is
/// not a whole number of sectors.
///
/// A reference is sector-granular: the final sector of such a file is held in
/// full, and only a dedupe covering all of it can move it. The kernel accepts an
/// unaligned length only where the range ends at the end of the file it is
/// replacing, and extends it to the sector boundary only where the source range
/// ends at the source file's own end. The shared copy runs on past that point,
/// so it cannot serve as the source; a temporary staged with exactly this tail
/// can.
///
/// The sector it leaves behind is that holder's alone. Two files can share a
/// partial sector only if they end at the same offset, which is not something a
/// rewrite gets to arrange.
fn redirect_tail(
    path: &Path,
    file: &File,
    from: u64,
    size: u64,
    fs: &Filesystem,
) -> Result<(), Failure> {
    let mut tail = vec![0u8; (size - from) as usize];
    file.read_exact_at(&mut tail, from)
        .map_err(|e| Failure::new(path, e))?;
    let temp = stage(path, &tail, fs)?;
    kernel::dedupe(&temp, fs.sectorsize, size - from, file, from)
        .map_err(|e| Failure::new(path, e))?;
    Ok(())
}

/// Points the parts of a holder's chunks that fall inside one staged stretch at
/// the temporary holding it.
///
/// Called once per stretch per holder, so it opens the file each time rather
/// than holding it: the whole point of staging one stretch at a time is that
/// nothing accumulates descriptors.
fn redirect_chunks(
    holder: &Located,
    temp: &File,
    liverange: &LiveRange,
    size: u64,
) -> Result<(), Failure> {
    let path = &holder.path;
    let file = holder.open()?;

    for r in &holder.holder.refs {
        // Clamp the end of the byte range to the file size. The final reference
        // can extend past it.
        let end = (r.file_offset + r.num_bytes).min(size);
        if end <= r.file_offset {
            // The file size has changed between the scan and now? Skip this
            // reference for safety.
            continue;
        }
        let mut at = r.extent_offset.max(liverange.start);
        let to = (r.extent_offset + r.num_bytes).min(liverange.end);
        while at < to {
            let cursor = r.file_offset + (at - r.extent_offset);
            if cursor >= end {
                break;
            }
            let len = (to - at).min(end - cursor);
            // Call FIDEDUPERANGE
            let src_offset = holder.fs.sectorsize + at - liverange.start;
            let deduped = kernel::dedupe(temp, src_offset, len, &file, cursor)
                .map_err(|e| Failure::new(path, e))?;
            if deduped != len {
                eprintln!("Warning {}: deduped != len", path.display());
            }
            // A deduped of 0 should never happen? Report it.
            if deduped == 0 {
                return Err(Failure::other(
                    path,
                    &format!("dedupe made no progress at offset {cursor}"),
                ));
            }
            at += deduped;
        }
    }
    Ok(())
}

/// Processes each holder of an extent, checking its size and mtime did not
/// change and optionally processing the final sector of the file with
/// [`redirect_tail`].
fn finish_holder(holder: &Located, before: &HolderBefore, fs: &Filesystem) -> Result<(), Failure> {
    let path = &holder.path;
    let file = holder.open()?;

    // Check if this extent overlaps the last sector of the file, and if so, redirect it to a copy of that sector.
    let tail_start = before.filesize / fs.sectorsize * fs.sectorsize;
    let holds_tail = holder
        .holder
        .refs
        .iter()
        .any(|r| r.file_offset <= tail_start && tail_start < r.file_offset + r.num_bytes);
    if !before.filesize.is_multiple_of(fs.sectorsize) && holds_tail {
        redirect_tail(path, &file, tail_start, before.filesize, fs)?;
    }

    // Check the metadata hasn't changed
    let metadata_after = file.metadata().map_err(|e| Failure::new(path, e))?;
    if before.filesize != metadata_after.size() {
        return Err(Failure::modified(path, "size changed during rewrite"));
    }
    let after_time = metadata_after
        .modified()
        .map_err(|e| Failure::new(path, e))?;
    if before.modified != after_time {
        return Err(Failure::modified(
            path,
            "modified time changed during rewrite",
        ));
    }
    Ok(())
}

/// What a run did, for whoever asked for it: the progress lines below say the
/// same thing as they happen, this is the same account in a form a caller can
/// act on.
#[derive(Default)]
pub struct Report {
    /// The extents rewritten, by disk address. On a dry run, the ones that
    /// would have been.
    pub rewritten: Vec<u64>,
    /// Uncompressed bytes copied to do it.
    pub copied_bytes: u64,
    /// On-disk bytes released by it.
    pub freed_bytes: u64,
    /// Extents left alone, named after the file the failure is charged to,
    /// with the reason.
    pub skipped: Vec<(PathBuf, String)>,
    /// Files something else wrote to while they were being rewritten, going by
    /// their size or mtime. The kernel refuses to dedupe bytes that differ, so
    /// this is a race with another writer, not lost data.
    pub modified: Vec<(PathBuf, String)>,
}

impl Report {
    /// Prints a failure and files it under what it means.
    fn failed(&mut self, failure: Failure) {
        let entry = (failure.path, failure.error.to_string());
        if failure.error.kind() == io::ErrorKind::InvalidData {
            eprintln!("warning: modified {}: {}", entry.0.display(), entry.1);
            self.modified.push(entry);
        } else {
            eprintln!("skipped {}: {}", entry.0.display(), entry.1);
            self.skipped.push(entry);
        }
    }
}

/// The head line for one extent: the file it is named after, what its rewrite
/// copies, and what it frees.
fn describe(extent: &Extent, holders: &[Located], options: &Options) -> String {
    let shared_with = match holders.len() - 1 {
        0 => String::new(),
        1 => " (shared with 1 other file)".to_string(),
        n => format!(" (shared with {n} other files)"),
    };
    format!(
        "{}: {} {}, {} {}{shared_with}",
        holders[0].path.display(),
        if options.apply { "copied" } else { "copying" },
        human(extent.live_uncompressed_bytes()),
        if options.apply {
            "freed part of"
        } else {
            "frees part of"
        },
        human(extent.disk_free_bytes()),
    )
}
