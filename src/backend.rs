//! The few system-facing calls the UI needs, in one place.
//!
//! The UI never calls the device, image or system modules directly: it goes
//! through here, so demo mode can be kept apart from the real machine.

use crate::error::Result;
use crate::model::{Cancel, Checksums, Disk, ImageInfo, ProgressFn};
use crate::plan::Env;
use crate::{device, hash, iso, sys};
use std::path::Path;

/// Read-only enumeration of all physical disks (filter with `safety::listable`).
pub fn list_disks() -> Result<Vec<Disk>> {
    device::list_disks()
}

/// Keeps a hot-plug watcher alive; drop it to stop watching.
pub struct WatchHandle(#[allow(dead_code)] Option<Box<dyn Send>>);

/// Calls `on_change` (from a background thread) whenever a disk or volume
/// arrives or leaves.
pub fn watch_devices(on_change: impl Fn() + Send + 'static) -> WatchHandle {
    WatchHandle(Some(Box::new(device::Watcher::spawn(on_change))))
}

/// Inspects an image file (fast: reads directory structures only).
pub fn inspect(path: &Path) -> Result<ImageInfo> {
    iso::inspect(path)
}

/// Facts about this PC for `plan::advise`.
pub fn plan_env() -> Env {
    Env {
        dism_available: sys::find_dism().is_some(),
    }
}

/// MD5, SHA-1 and SHA-256 of a file in one pass.
pub fn hash_file(path: &Path, cancel: &Cancel, progress: ProgressFn) -> Result<Checksums> {
    hash::hash_file(path, cancel, progress)
}

/// Result of comparing a pasted checksum with the computed ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HashVerdict {
    /// Nothing (useful) was pasted yet.
    Empty,
    /// Matches the named algorithm ("MD5", "SHA-1", "SHA-256").
    Match(&'static str),
    Mismatch,
    /// The text is not a checksum of a known length.
    NotAHash,
}

pub fn compare_hash(pasted: &str, sums: &Checksums) -> HashVerdict {
    if pasted.trim().is_empty() {
        return HashVerdict::Empty;
    }
    match hash::compare(pasted, sums) {
        hash::HashMatch::Md5 => HashVerdict::Match("MD5"),
        hash::HashMatch::Sha1 => HashVerdict::Match("SHA-1"),
        hash::HashMatch::Sha256 => HashVerdict::Match("SHA-256"),
        hash::HashMatch::Mismatch => HashVerdict::Mismatch,
        hash::HashMatch::NotAHash => HashVerdict::NotAHash,
    }
}

/// A checksum published next to `image` (in `image.sha256`, `SHA256SUMS`, ...):
/// the checksum file's name and the checksum it lists for the image.
pub fn published_checksum(image: &Path) -> Option<(String, String)> {
    let found = hash::find_sidecar(image)?;
    let name = found
        .file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Some((name, found.digest))
}

/// The lines `--hash` prints after the checksums when there is something to
/// compare them with: the value given with `--expect`, else a checksum
/// published next to the file. The flag is false when the comparison failed.
pub fn hash_check_lines(
    sums: &Checksums,
    expect: Option<&str>,
    published: Option<(String, String)>,
) -> (Vec<String>, bool) {
    let (source, text) = match (expect, published) {
        (Some(e), _) => ("the expected checksum".to_string(), e.to_string()),
        (None, Some((file, digest))) => (format!("the checksum listed in {file}"), digest),
        (None, None) => return (Vec::new(), true),
    };
    match compare_hash(&text, sums) {
        HashVerdict::Match(algo) => (vec![format!("OK      {algo} matches {source}")], true),
        HashVerdict::Mismatch => (
            vec![format!("FAILED  the file does not match {source}")],
            false,
        ),
        HashVerdict::Empty | HashVerdict::NotAHash => (
            vec![format!(
                "FAILED  {source} is not an MD5, SHA-1 or SHA-256 checksum"
            )],
            false,
        ),
    }
}

pub fn is_elevated() -> bool {
    sys::is_elevated()
}

/// Restarts BootForge elevated with the given arguments (the caller then closes this instance).
pub fn relaunch_elevated(args: &[String]) -> Result<()> {
    sys::relaunch_elevated(args)
}

/// Opens a file or folder with the shell.
pub fn open_path(path: &Path) {
    sys::open_path(path)
}

/// Free space in bytes on the volume holding `dir`.
pub fn free_space(dir: &Path) -> Option<u64> {
    sys::free_space(dir)
}

/// Lets the windowed release build print to the console it was started from.
pub fn attach_console() {
    sys::attach_parent_console()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_checksum_check() {
        let sums = Checksums {
            md5: "900150983cd24fb0d6963f7d28e17f72".into(),
            sha1: "a9993e364706816aba3e25717850c26c9cd0d89d".into(),
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
        };
        // Nothing to compare with: nothing is printed and nothing fails.
        assert_eq!(hash_check_lines(&sums, None, None), (Vec::new(), true));
        let (lines, ok) = hash_check_lines(&sums, Some(&sums.sha1.to_uppercase()), None);
        assert!(ok && lines[0].starts_with("OK") && lines[0].contains("SHA-1"));
        let (lines, ok) = hash_check_lines(&sums, Some(&"0".repeat(64)), None);
        assert!(!ok && lines[0].starts_with("FAILED"));
        let (lines, ok) = hash_check_lines(&sums, Some("not a checksum"), None);
        assert!(!ok && lines[0].contains("is not an MD5"));
        // A published checksum is used when none was given, and loses to one that was.
        let published = Some(("SHA256SUMS".to_string(), sums.sha256.clone()));
        let (lines, ok) = hash_check_lines(&sums, None, published.clone());
        assert!(ok && lines[0].contains("listed in SHA256SUMS"));
        let (_, ok) = hash_check_lines(&sums, Some(&"0".repeat(32)), published);
        assert!(!ok);
    }

    #[test]
    fn verdicts() {
        let sums = Checksums {
            md5: "900150983cd24fb0d6963f7d28e17f72".into(),
            sha1: "a9993e364706816aba3e25717850c26c9cd0d89d".into(),
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
        };
        assert_eq!(compare_hash("  ", &sums), HashVerdict::Empty);
        assert_eq!(
            compare_hash("A9993E364706816ABA3E25717850C26C9CD0D89D", &sums),
            HashVerdict::Match("SHA-1")
        );
        assert_eq!(
            compare_hash("900150983cd24fb0d6963f7d28e17f73", &sums),
            HashVerdict::Mismatch
        );
        assert_eq!(compare_hash("hello", &sums), HashVerdict::NotAHash);
    }
}
