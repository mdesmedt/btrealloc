//! Everything btrealloc shows the user goes through a [`Console`].

use std::cell::RefCell;
use std::fmt::Display;
use std::io;
use std::time::{Duration, Instant};

use crate::format::{duration, human};
use crate::kernel;

/// How often the progress line is redrawn at most.
const INTERVAL: Duration = Duration::from_millis(100);

/// Where btrealloc writes to: what a run finds and does, its result, to
/// stdout; diagnostics to stderr, errors and warnings named as such, and more
/// detail with `-v`.
///
/// An interactive console keeps a progress line at the bottom of stderr while
/// the walk runs. Everything written here takes it down first and puts it back
/// after, so nothing ends up on the same row.
pub struct Console {
    verbose: bool,
    interactive: bool,
    progress: RefCell<Option<Progress>>,
}

/// The progress line, while it is up.
struct Progress {
    /// Bytes the walk will have counted once it has read every extent, if
    /// that is known.
    total_bytes: Option<u64>,
    start: Instant,
    last_draw: Option<Instant>,
    /// The line as last drawn, to put back after something else is written.
    text: String,
}

impl Console {
    /// With `verbose`, [`Console::info`] is shown too. With `interactive`,
    /// which only makes sense when stderr is a terminal, so is a progress line.
    pub fn new(verbose: bool, interactive: bool) -> Console {
        Console {
            verbose,
            interactive,
            progress: RefCell::new(None),
        }
    }

    /// A line of the result, on stdout.
    pub fn print(&self, line: impl Display) {
        self.clear_progress();
        println!("{line}");
        self.draw_progress();
    }

    /// Detail, shown only with `-v`.
    pub fn info(&self, message: impl Display) {
        if self.verbose {
            self.print_stderr(&format!("{message}"));
        }
    }

    pub fn warn(&self, message: impl Display) {
        self.print_stderr(&format!("warning: {message}"));
    }

    pub fn error(&self, message: impl Display) {
        self.print_stderr(&format!("error: {message}"));
    }

    /// Whether this console shows a progress line.
    pub fn interactive(&self) -> bool {
        self.interactive
    }

    /// Puts up the progress line, towards `total_bytes` if that is known, if
    /// this console is interactive.
    pub fn start_progress(&self, total_bytes: Option<u64>) {
        if self.interactive {
            *self.progress.borrow_mut() = Some(Progress {
                total_bytes,
                start: Instant::now(),
                last_draw: None,
                text: String::new(),
            });
        }
    }

    /// Redraws the progress line with `bytes` read in `extents` extents so
    /// far, unless it was drawn too recently.
    pub fn update_progress(&self, bytes: u64, extents: u64) {
        let mut guard = self.progress.borrow_mut();
        let Some(progress) = guard.as_mut() else {
            return;
        };
        let now = Instant::now();
        if progress.last_draw.is_some_and(|last| now - last < INTERVAL) {
            return;
        }
        progress.last_draw = Some(now);
        progress.text = progress.describe(bytes, extents, now - progress.start);
        // A line that wraps cannot be erased again, so it is cut to fit. One
        // column is kept free: some terminals wrap on reaching the last one.
        if let Some(width) = kernel::terminal_width(&io::stderr()) {
            let fits = progress.text.len().min(width.saturating_sub(1));
            progress.text.truncate(fits);
        }
        // Drop before we call print_stderr
        drop(guard);
        self.print_stderr("");
    }

    /// Takes the progress line down for good.
    pub fn end_progress(&self) {
        self.clear_progress();
        *self.progress.borrow_mut() = None;
    }

    /// Writes `line` to stderr in place of the progress line, if one is up, and
    /// draws the progress line again under it, all in one write. An empty
    /// `line` only draws the progress line again, over itself.
    ///
    /// Each piece overwrites what was there and then erases the rest of the
    /// row, so the row is never blank in between.
    fn print_stderr(&self, line: &str) {
        let progress = self.progress.borrow();
        let text = progress.as_ref().map(|progress| progress.text.as_str());
        let output = match (line.is_empty(), text) {
            (true, None) => return,
            (true, Some(text)) => format!("\r{text}\x1b[K"),
            (false, None) => format!("{line}\n"),
            (false, Some(text)) => format!("\r{line}\x1b[K\n{text}"),
        };
        eprint!("{output}");
    }

    /// Erases the progress line, if one is up, leaving the cursor at the start
    /// of its row for whatever is written next.
    fn clear_progress(&self) {
        if self.progress.borrow().is_some() {
            eprint!("\r\x1b[2K");
        }
    }

    /// Draws the progress line as last described, if one is up.
    fn draw_progress(&self) {
        if let Some(progress) = self.progress.borrow().as_ref() {
            eprint!("{}", progress.text);
        }
    }
}

impl Progress {
    /// The text of the line, all ASCII. The total is what the block groups
    /// said at the start, which the walk can overshoot when extents are written
    /// meanwhile, so the fraction stops short of done until the walk is.
    ///
    /// Without a total there is no fraction or estimate, only what has been
    /// read so far.
    fn describe(&self, bytes: u64, extents: u64, elapsed: Duration) -> String {
        let Some(total_bytes) = self.total_bytes.filter(|&total| total > 0) else {
            return format!("{}  {} extents", human(bytes), extents);
        };
        let fraction = (bytes as f64 / total_bytes as f64).min(0.999);
        let mut text = format!(
            "{:.1}%  {} / {}  {} extents",
            fraction * 100.0,
            human(bytes),
            human(total_bytes),
            extents,
        );
        if fraction > 0.0 {
            let left = elapsed.as_secs_f64() * (1.0 - fraction) / fraction;
            text += &format!("  ~{} left", duration(Duration::from_secs_f64(left)));
        }
        text
    }
}
