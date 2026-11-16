//! The session log shown in the log pane and saved next to the settings.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A broken-down local-agnostic (UTC) time; enough for log stamps and file names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub year: u32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl Stamp {
    /// From seconds since 1970-01-01 (UTC).
    pub fn from_unix(secs: u64) -> Stamp {
        let days = secs / 86400;
        let rem = secs % 86400;
        // Civil-from-days (proleptic Gregorian), valid far beyond any real use.
        let z = days as i64 + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let year = (yoe + era * 400 + if month <= 2 { 1 } else { 0 }) as u32;
        Stamp {
            year,
            month,
            day,
            hour: (rem / 3600) as u32,
            minute: (rem % 3600 / 60) as u32,
            second: (rem % 60) as u32,
        }
    }

    pub fn now() -> Stamp {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Shift to local time using the bias Windows reports through the
        // environment-independent std API is not available; UTC is used and
        // labelled as such in saved logs.
        Stamp::from_unix(secs)
    }

    pub fn time(&self) -> String {
        format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second)
    }

    pub fn file_part(&self) -> String {
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// "HH:MM:SS" (UTC).
    pub time: String,
    pub text: String,
}

/// An in-memory log with a size cap.
#[derive(Debug, Default)]
pub struct Logbook {
    lines: Vec<Line>,
    /// Lines dropped from the front because the cap was reached.
    dropped: usize,
}

impl Logbook {
    const CAP: usize = 20_000;

    pub fn new() -> Logbook {
        Logbook::default()
    }

    pub fn push(&mut self, text: impl Into<String>) {
        self.push_at(Stamp::now(), text);
    }

    pub fn push_at(&mut self, stamp: Stamp, text: impl Into<String>) {
        let text = text.into();
        for part in text.lines() {
            self.lines.push(Line {
                time: stamp.time(),
                text: part.to_string(),
            });
        }
        if self.lines.len() > Self::CAP {
            let cut = self.lines.len() - Self::CAP;
            self.lines.drain(..cut);
            self.dropped += cut;
        }
    }

    pub fn lines(&self) -> &[Line] {
        &self.lines
    }
    pub fn len(&self) -> usize {
        self.lines.len()
    }
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
    pub fn clear(&mut self) {
        self.lines.clear();
        self.dropped = 0;
    }

    /// The whole log as text (CRLF line ends, for Notepad).
    pub fn text(&self) -> String {
        let mut out = format!(
            "BootForge {} log (times are UTC)\r\n",
            env!("CARGO_PKG_VERSION")
        );
        if self.dropped > 0 {
            out.push_str(&format!("[{} earlier lines dropped]\r\n", self.dropped));
        }
        for l in &self.lines {
            out.push_str(&l.time);
            out.push_str("  ");
            out.push_str(&l.text);
            out.push_str("\r\n");
        }
        out
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, self.text()).map_err(|e| format!("Could not save the log: {e}"))
    }

    /// Saves into `<app_dir>\logs\bootforge-<stamp>.log` and returns the path.
    pub fn save_auto(&self, app_dir: &Path) -> Result<PathBuf, String> {
        let path = logs_dir(app_dir).join(format!("bootforge-{}.log", Stamp::now().file_part()));
        self.save_to(&path)?;
        Ok(path)
    }
}

pub fn logs_dir(app_dir: &Path) -> PathBuf {
    app_dir.join("logs")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps() {
        let s = Stamp::from_unix(0);
        assert_eq!((s.year, s.month, s.day, s.hour), (1970, 1, 1, 0));
        // 2026-11-16 13:45:10 UTC
        let s = Stamp::from_unix(1_794_836_710);
        assert_eq!(
            (s.year, s.month, s.day, s.hour, s.minute, s.second),
            (2026, 11, 16, 13, 45, 10)
        );
        assert_eq!(s.file_part(), "20261116-134510");
        // Leap day.
        let s = Stamp::from_unix(1_709_164_800);
        assert_eq!((s.year, s.month, s.day), (2024, 2, 29));
    }

    #[test]
    fn log_text_and_cap() {
        let mut l = Logbook::new();
        l.push_at(Stamp::from_unix(3661), "first\nsecond");
        assert_eq!(l.len(), 2);
        assert_eq!(l.lines()[1].time, "01:01:01");
        let t = l.text();
        assert!(t.contains("01:01:01  second\r\n"));
        for i in 0..Logbook::CAP + 5 {
            l.push(format!("line {i}"));
        }
        assert_eq!(l.len(), Logbook::CAP);
        assert!(l.text().contains("earlier lines dropped"));
        let dir = std::env::temp_dir().join(format!("bootforge-log-{}", std::process::id()));
        let path = l.save_auto(&dir).unwrap();
        assert!(path.starts_with(dir.join("logs")));
        assert!(std::fs::metadata(&path).unwrap().len() > 1000);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
