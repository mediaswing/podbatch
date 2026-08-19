//! The two log files, in `~/Podbatch/Logging` — alongside the folder the
//! episodes themselves are saved in, so everything the app produces is under one
//! roof the user can find without being told where their platform hides
//! application data.
//!
//! * `output.log` — what the run did. One line per operation that succeeded,
//!   was skipped or failed: the same account the output box gives, kept after
//!   the window has been closed and past the point the box stops scrolling back.
//! * `debug.log` — how it did it. Every request, retry, decision and file name,
//!   for when the output log says something went wrong and the question is why.
//!
//! Written to from the UI thread and from every download task at once, so the
//! handles live behind a mutex in a global rather than being threaded through
//! the engine: a log is a property of the process, not of a run.
//!
//! Nothing here can fail loudly. A read-only home directory or a full disk
//! costs the user their logs, which is a shame; it must not cost them their
//! downloads, so every write is best-effort and a logger that never opened
//! quietly discards what it is given.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::util;

/// The folder the two files go in, under the home directory.
const FOLDER: &[&str] = &["Podbatch", "Logging"];

/// Where they go instead on a system with no home directory at all, under
/// whatever the platform calls its application data folder.
const FALLBACK_FOLDER: &str = "PodBatch";

/// A log already bigger than this when the app starts is moved aside, so a
/// machine that runs this every night doesn't quietly grow a log until the disk
/// notices. Checked once, at startup: a single session that writes past it
/// keeps writing, which takes a very long run and costs only the disk space a
/// log of that run was always going to take.
const ROTATE_AT: u64 = 2 * 1024 * 1024;

/// How an operation turned out. The tag it writes is fixed-width, so a log read
/// in a plain editor lines up and can be grepped for `FAIL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Skipped,
    Failed,
    /// Not an operation — the run starting, a file being loaded, a total.
    Note,
}

impl Outcome {
    fn tag(self) -> &'static str {
        match self {
            Outcome::Done => "DONE",
            Outcome::Skipped => "SKIP",
            Outcome::Failed => "FAIL",
            Outcome::Note => "----",
        }
    }
}

struct Logs {
    debug: Mutex<File>,
    output: Mutex<File>,
}

static LOGS: OnceLock<Logs> = OnceLock::new();
/// Where the logs went, or why they didn't. Set once, by [`open`].
static STATUS: OnceLock<Result<PathBuf, String>> = OnceLock::new();

/// Open both files, creating the folder if it isn't there yet. Returns the
/// folder they are in, or the reason there are no logs this time.
///
/// Calling it twice is harmless and does nothing the second time.
pub fn open() -> Result<PathBuf, String> {
    let dir = folder_path();
    STATUS.get_or_init(|| {
        let dir = dir.ok_or_else(|| "no application data folder on this system".to_string())?;
        start(&dir)?;
        Ok(dir)
    });

    status()
}

/// [`open`], into a folder of the caller's choosing. For tests, which have no
/// business writing into the real one.
#[cfg(test)]
fn open_in(dir: &Path) -> Result<PathBuf, String> {
    STATUS.get_or_init(|| start(dir).map(|()| dir.to_path_buf()));
    status()
}

fn start(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;

    let debug = append_to(&dir.join("debug.log"))?;
    let output = append_to(&dir.join("output.log"))?;
    // A second call can only lose the race with the first, and either file is
    // as good as the other; the loser's handles are simply dropped.
    let _ = LOGS.set(Logs { debug: Mutex::new(debug), output: Mutex::new(output) });

    Ok(())
}

/// Where the logs went, or why they didn't — for the window to say so once.
pub fn status() -> Result<PathBuf, String> {
    match STATUS.get() {
        Some(status) => status.clone(),
        None => Err("logging was never started".to_string()),
    }
}

fn folder_path() -> Option<PathBuf> {
    if let Some(home) = dirs::home_dir() {
        return Some(FOLDER.iter().fold(home, |path, part| path.join(part)));
    }

    // No home directory is vanishingly rare — a service account, a locked-down
    // kiosk — but it is the one case where insisting on `~` would mean no logs
    // at all, so the platform's own data folder stands in.
    dirs::data_dir()
        .or_else(dirs::config_dir)
        .map(|dir| dir.join(FALLBACK_FOLDER))
}

/// Open a log for appending, moving it aside first if it has grown too big.
fn append_to(path: &Path) -> Result<File, String> {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > ROTATE_AT) {
        // One generation back is kept. Two runs' worth of history is enough to
        // compare a run that worked against the one that didn't, and this way
        // the folder can never hold more than a handful of megabytes.
        let _ = std::fs::rename(path, path.with_extension("log.old"));
    }

    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// A line for `debug.log`: how the program got where it is.
pub fn debug(message: impl AsRef<str>) {
    let Some(logs) = LOGS.get() else { return };
    write_line(&logs.debug, &format!("{} {}", stamp(), message.as_ref()));
}

/// A line for `output.log`: an operation, and how it turned out.
///
/// Everything recorded here is also written to `debug.log`, so the debug file
/// reads as one story rather than as the half of it the output file left out.
pub fn record(outcome: Outcome, message: impl AsRef<str>) {
    let Some(logs) = LOGS.get() else { return };
    let line = format!("{} {} {}", stamp(), outcome.tag(), message.as_ref());
    write_line(&logs.output, &line);
    write_line(&logs.debug, &line);
}

fn stamp() -> String {
    util::utc_stamp(std::time::SystemTime::now())
}

/// Write one line and flush it.
///
/// Flushed every time on purpose: a log is most wanted after a crash, and a
/// buffered line is exactly the line that wouldn't be there. At a few hundred
/// lines a run the cost of this is not measurable.
fn write_line(file: &Mutex<File>, line: &str) {
    // A poisoned mutex means another thread panicked mid-write. The file is
    // still perfectly writable, and refusing to log from here on would throw
    // away the record of the very thing that went wrong.
    let mut file = match file.lock() {
        Ok(file) => file,
        Err(poisoned) => poisoned.into_inner(),
    };
    let _ = writeln!(file, "{line}");
    let _ = file.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both files, opened and written to for real.
    ///
    /// The logger is process-wide and opened once, so this is the only test
    /// that may open it — and it must, or every other test in the crate would
    /// be writing into the user's own log folder as a side effect.
    #[test]
    fn each_file_gets_what_belongs_in_it() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("podbatch-log-files-{nanos}"));

        let opened = open_in(&dir).expect("open the logs");
        assert_eq!(opened, dir);

        debug("a detail nobody needs to see");
        record(Outcome::Done, "an episode that landed");
        record(Outcome::Failed, "one that didn't");

        let output = std::fs::read_to_string(dir.join("output.log")).expect("output.log");
        let debug = std::fs::read_to_string(dir.join("debug.log")).expect("debug.log");

        // Operations, with their outcome, in the file that is a record of them.
        assert!(output.contains("DONE an episode that landed"), "{output}");
        assert!(output.contains("FAIL one that didn't"), "{output}");
        // The detail belongs only to the debug log.
        assert!(!output.contains("a detail nobody needs"), "{output}");

        // Which the debug log has, along with everything the other one said —
        // it is meant to read as the whole story.
        assert!(debug.contains("a detail nobody needs to see"), "{debug}");
        assert!(debug.contains("DONE an episode that landed"), "{debug}");
        assert!(debug.contains("FAIL one that didn't"), "{debug}");

        // Every line is stamped, so a log kept across runs can be read in order.
        for line in output.lines() {
            assert!(line.starts_with("20") && line.contains('Z'), "unstamped: {line}");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The logs live with the downloads, under one folder in the home
    /// directory, rather than wherever the platform files application data —
    /// which is a place most people cannot name, let alone find.
    #[test]
    fn the_logs_go_under_the_home_directory() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        assert_eq!(folder_path(), Some(home.join("Podbatch").join("Logging")));
    }

    #[test]
    fn outcomes_tag_the_same_width() {
        let widths: Vec<usize> = [Outcome::Done, Outcome::Skipped, Outcome::Failed, Outcome::Note]
            .iter()
            .map(|o| o.tag().len())
            .collect();
        assert!(widths.iter().all(|&w| w == widths[0]), "{widths:?}");
    }

    /// The rotation, the folder creation and the two handles, on a folder of
    /// the test's own — `open` itself can't be called here, since it would put
    /// the files in the real one.
    #[test]
    fn a_full_log_is_moved_aside_rather_than_grown() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("podbatch-logs-{nanos}"));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("debug.log");

        std::fs::write(&path, vec![b'x'; (ROTATE_AT + 1) as usize]).expect("write log");
        let file = append_to(&path).expect("open log");
        drop(file);

        assert_eq!(std::fs::metadata(&path).expect("new log").len(), 0);
        assert_eq!(
            std::fs::metadata(dir.join("debug.log.old")).expect("old log").len(),
            ROTATE_AT + 1
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
