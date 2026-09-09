use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use sha2::{Digest, Sha256};

use crate::Options;
use crate::format::{hex, human};
use crate::kernel::{self, Filesystem};
use crate::worklist::{Chunk, Holder, Job, Worklist};

/// Process the worklist, one extent at a time.
///
/// `fs` is here for its sector size: a holder ending mid-sector needs
/// [`redirect_tail`], and where that boundary falls is the filesystem's to say.
pub fn run(worklist: &Worklist, options: &Options, fs: &Filesystem) -> Report {
    let jobs = &worklist.jobs;
    let mut report = Report::default();

    println!();
    if options.apply {
        println!(
            "rewriting {} extents: copying {} to free {}",
            jobs.len(),
            human(worklist.uncompressed_bytes),
            human(worklist.reclaimable_bytes),
        );
        if options.verify {
            println!("verifying: every file is hashed before and after it is rewritten");
        }
    } else {
        println!("dry run: nothing will be written");
    }

    for job in jobs {
        if options.apply {
            // Reallocate the extent for real
            let result = realloc_extent(job, options, fs);
            // Check for failure
            if let Err(failure) = result {
                // Print errors and move to the next extent
                let entry = (failure.path.to_path_buf(), failure.error.to_string());
                if failure.error.kind() == io::ErrorKind::InvalidData {
                    eprintln!("CORRUPTED {}: {}", failure.path.display(), failure.error);
                    report.corrupted.push(entry);
                } else {
                    eprintln!("skipped {}: {}", failure.path.display(), failure.error);
                    report.skipped.push(entry);
                }
                continue;
            }
        }

        println!("{}", describe(job, options));

        report.copied_bytes += job.uncompressed_bytes;
        report.freed_bytes += job.reclaimable_bytes;
        report.rewritten.push(job.disk_address);

        if options.verbose {
            println!(
                "    extent {:#x}, {} on disk",
                job.disk_address,
                human(job.disk_bytes),
            );
            for holder in &job.holders {
                for chunk in &holder.chunks {
                    println!(
                        "    {} at offset {} in {}",
                        human(chunk.len),
                        chunk.file_offset,
                        holder.path.display(),
                    );
                }
            }
        }
    }

    let extents = report.rewritten.len();
    if options.apply {
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
    if !report.corrupted.is_empty() {
        println!(
            "{} files no longer hold the contents they did",
            report.corrupted.len()
        );
    }
    report
}

/// Copies an extent's live ranges into temporary files, then points the live ranges from
/// every file holding that extent at the copy. No original inode is ever replaced. Each
/// keeps its identity, owner, times and xattrs, and only its extent references change.
fn realloc_extent(job: &Job, options: &Options, fs: &Filesystem) -> Result<(), Failure> {
    let liveranges = read_liveranges(job, fs)?;

    // What every holder looked like beforehand for checking after we reallocate.
    let before: Vec<HolderBefore> = job
        .holders
        .iter()
        .map(|holder| HolderBefore::read(holder, options))
        .collect::<Result<_, _>>()?;

    // Iterate over live ranges
    for liverange in &liveranges {
        // Copy the live range into a temporary file
        let temp = stage(job, liverange)?;
        // Then point every holder at the newly allocated data
        for (holder, before) in job.holders.iter().zip(&before) {
            redirect_chunks(holder, &temp, liverange, before.filesize)?;
        }
    }

    for (holder, before) in job.holders.iter().zip(&before) {
        finish_holder(holder, before, fs)?;
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
    /// `[start, end)` of the extent, which is what a staged temporary holds at
    /// its own offset zero.
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
fn read_liveranges(job: &Job, fs: &Filesystem) -> Result<Vec<LiveRange>, Failure> {
    let mut chunks: Vec<(&Rc<PathBuf>, &Chunk)> = job
        .holders
        .iter()
        .flat_map(|holder| holder.chunks.iter().map(move |chunk| (&holder.path, chunk)))
        .collect();
    chunks.sort_unstable_by_key(|(_, chunk)| chunk.extent_offset);

    let mut copies = Vec::new();
    for (start, end) in live_ranges(&chunks) {
        let mut bytes = vec![0u8; (end - start) as usize];
        let end = fill(&chunks, start, end, &mut bytes)?;
        // Holders are pointed at whole sectors, so a part sector at the end of a
        // stretch cut short by a file's end is one nothing can ever reference.
        // Dropping it here keeps it from ever being written out; the holder it
        // belongs to gets its last sector from [`redirect_tail`] instead.
        let end = end / fs.sectorsize * fs.sectorsize;
        if end > start {
            bytes.truncate((end - start) as usize);
            copies.push(LiveRange { bytes, start, end });
        }
    }
    Ok(copies)
}

/// The directory holding `path`, which is where its temporary copy goes.
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        // A path of one component has an empty parent, not a missing one.
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

fn copy_range(
    src: &File,
    src_offset: u64,
    dest: &File,
    dest_offset: u64,
    len: u64,
) -> io::Result<()> {
    let mut buf = vec![0u8; 1 << 20];
    let mut done = 0;
    while done < len {
        let n = ((len - done) as usize).min(buf.len());
        src.read_exact_at(&mut buf[..n], src_offset + done)?;
        dest.write_all_at(&buf[..n], dest_offset + done)?;
        done += n as u64;
    }
    Ok(())
}

/// The SHA-256 of a file's contents, read through to the end.
fn checksum(file: &File) -> io::Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut offset = 0u64;
    loop {
        let n = file.read_at(&mut buf, offset)?;
        if n == 0 {
            return Ok(hasher.finalize().into());
        }
        hasher.update(&buf[..n]);
        offset += n as u64;
    }
}

/// A rewrite that did not happen, and the file it is charged to.
struct Failure {
    path: Rc<PathBuf>,
    error: io::Error,
}

impl Failure {
    fn new(path: &Rc<PathBuf>, error: impl Into<io::Error>) -> Failure {
        Failure {
            path: Rc::clone(path),
            error: error.into(),
        }
    }

    fn other(path: &Rc<PathBuf>, message: &str) -> Failure {
        Failure::new(path, io::Error::other(message))
    }

    fn invalid(path: &Rc<PathBuf>, message: String) -> Failure {
        Failure::new(path, io::Error::new(io::ErrorKind::InvalidData, message))
    }
}

/// What a holder held before anything moved, to check it against afterwards.
struct HolderBefore {
    checksum: Option<[u8; 32]>,
    filesize: u64,
    modified: std::time::SystemTime,
}

impl HolderBefore {
    /// Reads a holder's contents and metadata, and refuses one a rewrite cannot
    /// touch at all.
    ///
    /// Taken once for the whole job rather than per stretch: the checks in
    /// [`finish`] need something from before anything moved, and a stretch is
    /// staged at a time, so there is no one place downstream that still sees the
    /// file untouched.
    fn read(holder: &Holder, options: &Options) -> Result<HolderBefore, Failure> {
        let path = &holder.path;
        let file = File::open(&**path).map_err(|e| Failure::new(path, e))?;
        if kernel::is_nocow(&file).map_err(|e| Failure::new(path, e))? {
            return Err(Failure::other(path, "nodatacow"));
        }
        let checksum = if options.verify {
            Some(checksum(&file).map_err(|e| Failure::new(path, e))?)
        } else {
            None
        };
        let metadata = file.metadata().map_err(|e| Failure::new(path, e))?;
        Ok(HolderBefore {
            checksum,
            filesize: metadata.len(),
            modified: metadata.modified().map_err(|e| Failure::new(path, e))?,
        })
    }
}

/// Every stretch of the extent some file still references, merged, in order.
fn live_ranges(chunks: &[(&Rc<PathBuf>, &Chunk)]) -> Vec<(u64, u64)> {
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (_, chunk) in chunks {
        let (start, end) = (chunk.extent_offset, chunk.extent_offset + chunk.len);
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// Writes one stretch out to a temporary of its own, ready to be deduped from
/// and dropped again.
///
/// The temporary inherits the directory's attributes, and btrfs refuses to
/// dedupe between two inodes that disagree about checksums. A datacow file in a
/// nodatacow directory is the one shape that reaches this, and it is worth
/// naming rather than leaving as a bare EINVAL.
fn stage(job: &Job, range: &LiveRange) -> Result<File, Failure> {
    let temp =
        kernel::temp_file(parent_dir(job.path())).map_err(|e| Failure::new(job.path(), e))?;
    if kernel::is_nocow(&temp).map_err(|e| Failure::new(job.path(), e))? {
        return Err(Failure::other(
            job.path(),
            "temporary file inherited nodatacow from its directory",
        ));
    }
    temp.write_all_at(&range.bytes, 0)
        .map_err(|e| Failure::new(job.path(), e))?;
    Ok(temp)
}

/// Reads `[start, end)` of the extent into `buf`, taking each part from a file
/// that holds it. Returns how far it got: a holder's last extent can run past
/// the end of its data, so there may be less to read than the stretch claims.
fn fill(
    chunks: &[(&Rc<PathBuf>, &Chunk)],
    start: u64,
    end: u64,
    buf: &mut [u8],
) -> Result<u64, Failure> {
    let mut at = start;
    for (path, chunk) in chunks {
        if at >= end {
            break;
        }
        if chunk.extent_offset > at || chunk.extent_offset + chunk.len <= at {
            continue;
        }
        let file = File::open(path.as_path()).map_err(|e| Failure::new(path, e))?;
        let size = file.metadata().map_err(|e| Failure::new(path, e))?.len();
        let file_offset = chunk.file_offset + (at - chunk.extent_offset);
        let to = end
            .min(chunk.extent_offset + chunk.len)
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
/// so it cannot serve as the source; a temporary cut to exactly this tail can.
///
/// The sector it leaves behind is that holder's alone. Two files can share a
/// partial sector only if they end at the same offset, which is not something a
/// rewrite gets to arrange.
fn redirect_tail(
    path: &Rc<PathBuf>,
    file: &File,
    from: u64,
    size: u64,
    fs: &Filesystem,
) -> Result<(), Failure> {
    let len = size - from;
    let temp = kernel::temp_file(parent_dir(path)).map_err(|e| Failure::new(path, e))?;
    // The tail sits a sector into the temporary, with a hole before it, rather
    // than at its start.
    //
    // Two things have to hold at once, and the obvious arrangement cannot manage
    // both. The kernel rounds a sub-sector request up to a whole sector only when
    // the source range ends at the source file's end, so the temporary has to
    // end exactly where the tail does. But a file that short is written into the
    // metadata leaf instead of an extent of its own, and an inline extent has
    // nothing to share: the dedupe compares equal, reports every byte, and
    // leaves the sector where it was. Writing a whole sector and truncating back
    // does not help either, because the truncate inlines it again.
    //
    // Holding the data one sector in satisfies both. The file still ends at the
    // tail, and at over a sector long it is never a candidate for inlining. The
    // hole costs nothing.
    copy_range(file, from, &temp, fs.sectorsize, len).map_err(|e| Failure::new(path, e))?;
    temp.sync_all().map_err(|e| Failure::new(path, e))?;

    let deduped =
        kernel::dedupe(&temp, fs.sectorsize, len, file, from).map_err(|e| Failure::new(path, e))?;
    if deduped != len {
        return Err(Failure::other(
            path,
            &format!("the last sector moved {deduped} of {len} bytes"),
        ));
    }
    Ok(())
}

/// Points the parts of a holder's chunks that fall inside one staged stretch at
/// the temporary holding it.
///
/// Called once per stretch per holder, so it opens the file each time rather
/// than holding it: the whole point of staging one stretch at a time is that
/// nothing accumulates descriptors.
fn redirect_chunks(
    holder: &Holder,
    temp: &File,
    liverange: &LiveRange,
    size: u64,
) -> Result<(), Failure> {
    let path = &holder.path;
    // FIDEDUPERANGE replaces this file's extents, so it has to be open for
    // writing as well as reading.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&**path)
        .map_err(|e| Failure::new(path, e))?;

    for chunk in &holder.chunks {
        // Clamp the end of the byte range to the file size. The final chunk can
        // extend past it.
        let end = (chunk.file_offset + chunk.len).min(size);
        if end <= chunk.file_offset {
            // The file size has changed between the scan and now? Skip this
            // chunk for safety.
            continue;
        }
        let mut at = chunk.extent_offset.max(liverange.start);
        let to = (chunk.extent_offset + chunk.len).min(liverange.end);
        while at < to {
            let cursor = chunk.file_offset + (at - chunk.extent_offset);
            if cursor >= end {
                break;
            }
            let len = (to - at).min(end - cursor);
            // Call FIDEDUPERANGE
            let deduped = kernel::dedupe(temp, at - liverange.start, len, &file, cursor)
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

/// Processes each holder of an extent, performing any checks requested and
/// optionally processing the final sector of the file with [`redirect_tail`].
fn finish_holder(holder: &Holder, before: &HolderBefore, fs: &Filesystem) -> Result<(), Failure> {
    let path = &holder.path;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&**path)
        .map_err(|e| Failure::new(path, e))?;

    // Check if this extent overlaps the last sector of the file, and if so, redirect it to a copy of that sector.
    let tail_start = before.filesize / fs.sectorsize * fs.sectorsize;
    let holds_tail = holder
        .chunks
        .iter()
        .any(|chunk| chunk.file_offset <= tail_start && tail_start < chunk.file_offset + chunk.len);
    if !before.filesize.is_multiple_of(fs.sectorsize) && holds_tail {
        redirect_tail(path, &file, tail_start, before.filesize, fs)?;
    }

    // Check the metadata hasn't changed
    let metadata_after = file.metadata().map_err(|e| Failure::new(path, e))?;
    if before.filesize != metadata_after.size() {
        return Err(Failure::invalid(
            path,
            "size changed during rewrite".to_string(),
        ));
    }
    let after_time = metadata_after
        .modified()
        .map_err(|e| Failure::new(path, e))?;
    if before.modified != after_time {
        return Err(Failure::invalid(
            path,
            "modified time changed during rewrite".to_string(),
        ));
    }

    // Check the file contents haven't changed
    if let Some(checksum_before) = before.checksum {
        let checksum_after = checksum(&file).map_err(|e| Failure::new(path, e))?;
        if checksum_after != checksum_before {
            return Err(Failure::invalid(
                path,
                format!(
                    "contents changed: {} before, {} after",
                    hex(&checksum_before),
                    hex(&checksum_after),
                ),
            ));
        }
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
    /// Files which came back holding different bytes. Nothing else belongs
    /// here: this is the one failure that means data was lost.
    pub corrupted: Vec<(PathBuf, String)>,
}

/// The head line for one extent: the file it is named after, what its rewrite
/// copies, and what it frees.
fn describe(job: &Job, options: &Options) -> String {
    let shared_with = match job.holders.len() - 1 {
        0 => String::new(),
        1 => " (shared with 1 other file)".to_string(),
        n => format!(" (shared with {n} other files)"),
    };
    format!(
        "{}: {} {}, {} {}{shared_with}",
        job.path().display(),
        if options.apply { "copied" } else { "copying" },
        human(job.uncompressed_bytes),
        if options.apply {
            "freed part of"
        } else {
            "frees part of"
        },
        human(job.reclaimable_bytes),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(extent_offset: u64, len: u64) -> Chunk {
        Chunk {
            extent_offset,
            file_offset: 0,
            len,
        }
    }

    fn ranges(chunks: &[Chunk]) -> Vec<(u64, u64)> {
        let path = Rc::new(PathBuf::from("f"));
        let pairs: Vec<(&Rc<PathBuf>, &Chunk)> = chunks.iter().map(|c| (&path, c)).collect();
        live_ranges(&pairs)
    }

    #[test]
    fn separate_stretches_stay_separate() {
        assert_eq!(
            ranges(&[chunk(0, 4096), chunk(1 << 20, 4096)]),
            vec![(0, 4096), (1 << 20, (1 << 20) + 4096)],
        );
    }

    #[test]
    fn overlapping_and_touching_stretches_merge() {
        assert_eq!(
            ranges(&[chunk(0, 8192), chunk(4096, 8192)]),
            vec![(0, 12288)]
        );
        assert_eq!(
            ranges(&[chunk(0, 4096), chunk(4096, 4096)]),
            vec![(0, 8192)]
        );
    }

    /// Larger than the read buffer, so a checksum that stopped at one bufferful
    /// would not match.
    #[test]
    fn checksum_covers_the_whole_file() {
        let contents: Vec<u8> = (0..(3 << 20) as u32).map(|i| i as u8).collect();
        let path = std::env::temp_dir().join("btrealloc-checksum-test");
        std::fs::write(&path, &contents).expect("write the test file");
        let file = File::open(&path).expect("open the test file");
        let digest = checksum(&file).expect("checksum the test file");
        std::fs::remove_file(&path).expect("remove the test file");

        let mut expected = Sha256::new();
        expected.update(&contents);
        let expected: [u8; 32] = expected.finalize().into();
        assert_eq!(hex(&digest), hex(&expected));
    }
}
