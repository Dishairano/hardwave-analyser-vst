//! A short log of what the editor did, written to a file the user can send us.
//!
//! Everything the editor knows about a failed window — whether a token was
//! found, which URL it loaded, whether the WebView was created, whether the
//! interface answered — was already printed to stderr. No DAW shows stderr to
//! the person using it, so on the machines where the window stays empty none of
//! it ever reaches us, and every report costs another round of questions.
//!
//! The same lines are now appended to
//! `<data dir>/hardwave/analyser-editor.log`, which on Windows is
//! `%APPDATA%\hardwave\analyser-editor.log`. It holds the last few runs: the
//! file is truncated once it passes `MAX_BYTES`, so it cannot grow without
//! bound on a machine that opens the plug-in every day.
//!
//! It contains no audio, no project data and no token: the URL is logged
//! without its query string.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Truncate the log once it passes this. A run writes well under 2 KiB.
const MAX_BYTES: u64 = 256 * 1024;

pub fn log_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("hardwave").join("analyser-editor.log"))
}

/// Append one line. Never fails loudly: a plug-in that cannot write its log
/// still has to open.
pub fn record(line: &str) {
    if let Some(path) = log_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > MAX_BYTES {
            let _ = std::fs::write(&path, b"");
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{} {}", timestamp(), line);
        }
    }
}

/// `2026-09-22 12:31:07Z`, from the clock alone: a date is what makes a line in
/// this file match a moment the user describes, and no dependency here has one.
fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let (y, m, d, hh, mm, ss) = civil_from_unix(secs);
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z", y, m, d, hh, mm, ss)
}

/// Days-to-civil after Howard Hinnant's algorithm, which is exact for every
/// date the Gregorian calendar defines and needs no table.
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };

    (
        y,
        m,
        d,
        (rem / 3_600) as u32,
        ((rem % 3_600) / 60) as u32,
        (rem % 60) as u32,
    )
}

/// Write one line to stderr and ignore any failure.
///
/// `eprintln!` panics when the write fails. On Windows a host that started the
/// plug-in with stderr on a pipe and then closed the other end turns every
/// line into `The pipe is being closed. (os error 232)`, and the panic takes
/// the DAW down with it. Nobody reads stderr in a DAW, so a line that cannot be
/// written is simply dropped.
pub fn to_stderr(line: &str) {
    let _ = writeln!(std::io::stderr(), "{}", line);
}

/// Print a line to stderr, as before, and put it in the log file as well.
#[macro_export]
macro_rules! elog {
    ($($arg:tt)*) => {{
        let line = format!($($arg)*);
        $crate::diag::to_stderr(&line);
        $crate::diag::record(&line);
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_right() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // 2026-09-22 12:31:07Z
        assert_eq!(civil_from_unix(1_790_080_267), (2026, 9, 22, 12, 31, 7));
        // A leap day, which a naive month table gets wrong.
        assert_eq!(civil_from_unix(1_709_208_000), (2024, 2, 29, 12, 0, 0));
    }

    #[test]
    fn the_log_holds_a_date_and_the_line() {
        let line = format!("{} hello", timestamp());
        assert!(line.contains("hello"));
        assert!(line.contains('Z'), "a time without a zone is ambiguous");
        assert_eq!(line.matches('-').count(), 2, "YYYY-MM-DD");
    }

    /// Set only on the child process that [`a_closed_stderr_does_not_panic`] starts.
    const CLOSED_STDERR_CHILD: &str = "ANALYSER_TEST_CLOSED_STDERR_CHILD";

    /// Runs in a child whose stderr is a pipe nobody reads any more. On Windows
    /// that write fails with os error 232; elsewhere with a broken pipe.
    /// `eprintln!` panics on it; `to_stderr` must not.
    #[test]
    fn closed_stderr_child() {
        if std::env::var_os(CLOSED_STDERR_CHILD).is_none() {
            return;
        }
        // Give the parent time to close its end of the pipe.
        std::thread::sleep(std::time::Duration::from_millis(500));
        // The pipe has to be broken already, or this test proves nothing.
        assert!(
            writeln!(std::io::stderr(), "probe").is_err(),
            "stderr is still writable, the parent did not close the pipe"
        );
        for _ in 0..3 {
            to_stderr("[HardwaveAnalyser] a line nobody will read");
        }
    }

    #[test]
    fn a_closed_stderr_does_not_panic() {
        use std::process::{Command, Stdio};

        let exe = std::env::current_exe().expect("test binary path");
        let mut child = Command::new(exe)
            .args(["diag::tests::closed_stderr_child", "--exact", "--nocapture"])
            .env(CLOSED_STDERR_CHILD, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the child test");
        // Close the read end so every write to the child's stderr fails.
        drop(child.stderr.take());
        let status = child.wait().expect("wait for the child test");
        assert!(
            status.success(),
            "writing to a closed stderr panicked: {status}"
        );
    }

    #[test]
    fn the_log_lives_beside_the_token() {
        // Both are under <data dir>/hardwave, so telling someone where one is
        // tells them where the other is.
        if let Some(p) = log_path() {
            assert!(p.ends_with("hardwave/analyser-editor.log"));
        }
    }
}
