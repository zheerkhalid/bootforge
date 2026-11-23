//! MD5 / SHA-1 / SHA-256 of files.
//!
//! All three digests are computed in a single pass ([`MultiHasher`]) so that a
//! multi-gigabyte image is read only once. [`compare`] matches a checksum the
//! user pasted from a download page against the computed ones.

use crate::error::{IoContext, Result};
use crate::model::{Cancel, Checksums, ProgressFn};
use crate::util::hex;
use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Chunk size used by [`hash_file`].
const CHUNK: usize = 1024 * 1024;

/// MD5, SHA-1 and SHA-256 computed together in one pass.
#[derive(Clone, Default)]
pub struct MultiHasher {
    md5: Md5,
    sha1: Sha1,
    sha256: Sha256,
}

impl MultiHasher {
    pub fn new() -> MultiHasher {
        MultiHasher::default()
    }

    /// Feeds the next piece of data to all three digests.
    pub fn update(&mut self, data: &[u8]) {
        self.md5.update(data);
        self.sha1.update(data);
        self.sha256.update(data);
    }

    /// The three digests as lower-case hex.
    pub fn finish(self) -> Checksums {
        Checksums {
            md5: hex(&self.md5.finalize()),
            sha1: hex(&self.sha1.finalize()),
            sha256: hex(&self.sha256.finalize()),
        }
    }
}

/// Hashes a whole file in 1 MiB chunks.
///
/// `progress` receives the number of bytes hashed so far (ending at the file
/// size). `cancel` is checked between chunks; a cancelled run returns
/// `Error::Cancelled`.
pub fn hash_file(path: &Path, cancel: &Cancel, progress: ProgressFn) -> Result<Checksums> {
    let mut file = File::open(path).ctx("Could not open the file to hash it")?;
    let mut hasher = MultiHasher::new();
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0u64;
    loop {
        cancel.check()?;
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(crate::error::Error::io("Could not read the file", e)),
        };
        hasher.update(&buf[..n]);
        done += n as u64;
        progress(done);
    }
    Ok(hasher.finish())
}

/// Result of comparing a pasted checksum with the computed ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HashMatch {
    /// The pasted text is the file's MD5.
    Md5,
    /// The pasted text is the file's SHA-1.
    Sha1,
    /// The pasted text is the file's SHA-256.
    Sha256,
    /// The pasted text is a well-formed MD5/SHA-1/SHA-256 but none of them
    /// matches the file.
    Mismatch,
    /// The pasted text does not contain a 32, 40 or 64 digit hex checksum.
    NotAHash,
}

/// `Some("md5" | "sha1" | "sha256")` when `word` names one of the algorithms
/// ("SHA256", "sha-256", "SHA_1:", "MD5" ...).
fn algorithm_name(word: &str) -> Option<&'static str> {
    let w: String = word
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    match w.as_str() {
        "md5" | "md5sum" => Some("md5"),
        "sha1" | "sha1sum" => Some("sha1"),
        "sha256" | "sha256sum" | "sha2256" => Some("sha256"),
        _ => None,
    }
}

fn is_hash_len(n: usize) -> bool {
    matches!(n, 32 | 40 | 64)
}

/// Every reading of one line as a checksum, lower-case.
///
/// Leading algorithm names are skipped, then hex tokens are joined (so
/// checksums printed in groups of digits work) until a token that is not hex
/// is reached, e.g. the file name of `sha256sum` output. Each prefix of the
/// joined tokens that has a checksum length is a candidate, because a file
/// name can itself be made of hex digits.
fn candidates_in_line(line: &str, out: &mut Vec<String>) {
    // BSD style: "SHA256 (file.iso) = 9f86d0..."
    let line = match line.rfind('=') {
        Some(i) => &line[i + 1..],
        None => line,
    };
    let mut joined = String::new();
    let mut started = false;
    for token in line.split_whitespace() {
        let mut token = token;
        if !started {
            // "SHA256:", "SHA-256" or "sha256:9f86d0..." in front of the digits.
            if algorithm_name(token).is_some() {
                continue;
            }
            if let Some((name, rest)) = token.split_once(':') {
                if algorithm_name(name).is_some() {
                    if rest.is_empty() {
                        continue;
                    }
                    token = rest;
                }
            }
        }
        if token.is_empty() || !token.chars().all(|c| c.is_ascii_hexdigit()) {
            break;
        }
        started = true;
        joined.push_str(&token.to_ascii_lowercase());
        if is_hash_len(joined.len()) {
            out.push(joined.clone());
        }
        if joined.len() >= 64 {
            break;
        }
    }
}

/// Compares a checksum pasted by the user with the computed checksums.
///
/// Tolerant of what people actually paste: upper case, surrounding or grouping
/// spaces, a trailing file name as printed by `sha256sum` / `md5sum`
/// (`<hash>  file.iso`, `<hash> *file.iso`), prefixes such as `SHA256:` or
/// `SHA-256 =`, the BSD form `SHA256 (file) = <hash>` and multi-line output
/// such as that of `certutil -hashfile`.
pub fn compare(pasted: &str, sums: &Checksums) -> HashMatch {
    let mut candidates = Vec::new();
    for line in pasted.lines() {
        candidates_in_line(line, &mut candidates);
    }
    if candidates.is_empty() {
        return HashMatch::NotAHash;
    }
    for c in &candidates {
        if !sums.md5.is_empty() && c.eq_ignore_ascii_case(&sums.md5) {
            return HashMatch::Md5;
        }
        if !sums.sha1.is_empty() && c.eq_ignore_ascii_case(&sums.sha1) {
            return HashMatch::Sha1;
        }
        if !sums.sha256.is_empty() && c.eq_ignore_ascii_case(&sums.sha256) {
            return HashMatch::Sha256;
        }
    }
    HashMatch::Mismatch
}

// --------------------------------------------------------------------------------------
// Checksum files next to an image
// --------------------------------------------------------------------------------------

/// A checksum found in a checksum file that sits next to an image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sidecar {
    /// The checksum file.
    pub file: PathBuf,
    /// The checksum listed for the image, lower-case hex.
    pub digest: String,
}

/// Checksum files larger than this are not read (real ones are a few lines).
const SIDECAR_MAX_BYTES: u64 = 1024 * 1024;

/// Lists that name several files, as distributions publish them.
const LIST_NAMES: [&str; 10] = [
    "SHA256SUMS",
    "SHA256SUMS.txt",
    "sha256sum.txt",
    "sha256sums.txt",
    "CHECKSUM",
    "CHECKSUMS",
    "checksums.txt",
    "SHA1SUMS",
    "MD5SUMS",
    "md5sum.txt",
];

/// Extensions of a checksum file that belongs to one image (`image.iso.sha256`).
const OWN_EXTENSIONS: [&str; 6] = ["sha256", "sha256sum", "sha256.txt", "sha1", "md5", "md5sum"];

/// The file name at the end of a path written with either kind of slash.
fn base_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// The checksum a checksum file lists for `file_name`.
///
/// Understands `sha256sum` style lines (`<hash>  name`, `<hash> *name`, with
/// or without a folder in front of the name) and the BSD style
/// (`SHA256 (name) = <hash>`). With `single_ok`, a file that holds exactly
/// one checksum and names no file at all is taken to be about `file_name`.
pub fn expected_in(text: &str, file_name: &str, single_ok: bool) -> Option<String> {
    let mut lone: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // BSD style: "SHA256 (name) = hash".
        if let (Some(open), Some(close)) = (line.find('('), line.rfind(')')) {
            if open < close && algorithm_name(line[..open].trim()).is_some() {
                let name = base_name(line[open + 1..close].trim());
                let digest = line[close + 1..].trim().trim_start_matches('=').trim();
                if is_hash_len(digest.len()) && digest.chars().all(|c| c.is_ascii_hexdigit()) {
                    if name.eq_ignore_ascii_case(file_name) {
                        return Some(digest.to_ascii_lowercase());
                    }
                    continue;
                }
            }
        }
        // sha256sum style: "hash  name" / "hash *name"; a bare hash names nothing.
        let (digest, rest) = match line.split_once(char::is_whitespace) {
            Some((d, r)) => (d, r.trim()),
            None => (line, ""),
        };
        if !is_hash_len(digest.len()) || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        if rest.is_empty() {
            lone.push(digest.to_ascii_lowercase());
        } else if base_name(rest.trim_start_matches('*')).eq_ignore_ascii_case(file_name) {
            return Some(digest.to_ascii_lowercase());
        }
    }
    if single_ok && lone.len() == 1 {
        return lone.pop();
    }
    None
}

fn read_small(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > SIDECAR_MAX_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Looks next to `image` for a checksum file that lists it: first a file of
/// its own (`image.iso.sha256`, ...), then the usual lists (`SHA256SUMS`, ...).
/// Only small text files in the image's own folder are read.
pub fn find_sidecar(image: &Path) -> Option<Sidecar> {
    let dir = image.parent()?;
    let name = image.file_name()?.to_string_lossy().into_owned();
    for ext in OWN_EXTENSIONS {
        let file = dir.join(format!("{name}.{ext}"));
        if let Some(digest) = read_small(&file).and_then(|t| expected_in(&t, &name, true)) {
            return Some(Sidecar { file, digest });
        }
    }
    for list in LIST_NAMES {
        let file = dir.join(list);
        if let Some(digest) = read_small(&file).and_then(|t| expected_in(&t, &name, false)) {
            return Some(Sidecar { file, digest });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    #[test]
    fn checksum_lists_are_searched_for_the_file_name() {
        let list = format!(
            "# release 24.04\n{ABC_SHA256} *other.iso\n{EMPTY_SHA256}  nimbus-24.04.iso\n{ABC_SHA1}  ./pool/third.iso\n"
        );
        assert_eq!(
            expected_in(&list, "nimbus-24.04.iso", false).as_deref(),
            Some(EMPTY_SHA256)
        );
        // Binary marker, folders in front of the name and letter case do not matter.
        assert_eq!(
            expected_in(&list, "OTHER.ISO", false).as_deref(),
            Some(ABC_SHA256)
        );
        assert_eq!(
            expected_in(&list, "third.iso", false).as_deref(),
            Some(ABC_SHA1)
        );
        assert_eq!(expected_in(&list, "missing.iso", false), None);
        // A name that merely ends the same is another file.
        assert_eq!(expected_in(&list, "04.iso", false), None);

        let bsd = format!(
            "SHA256 (a.iso) = {ABC_SHA256}\nMD5 (b.iso) = {}\n",
            ABC_MD5.to_uppercase()
        );
        assert_eq!(
            expected_in(&bsd, "a.iso", false).as_deref(),
            Some(ABC_SHA256)
        );
        assert_eq!(expected_in(&bsd, "b.iso", true).as_deref(), Some(ABC_MD5));
        assert_eq!(expected_in(&bsd, "c.iso", true), None);

        // A file with one bare checksum is about its image, but only when asked
        // for (a list never is) and only when it is unambiguous.
        let bare = format!("\u{feff}{ABC_SHA256}\r\n");
        assert_eq!(
            expected_in(&bare, "x.iso", true).as_deref(),
            Some(ABC_SHA256)
        );
        assert_eq!(expected_in(&bare, "x.iso", false), None);
        let two = format!("{ABC_SHA256}\n{EMPTY_SHA256}\n");
        assert_eq!(expected_in(&two, "x.iso", true), None);
        // Not checksums at all.
        assert_eq!(
            expected_in("hello world\nabc123  x.iso\n", "x.iso", true),
            None
        );
    }

    #[test]
    fn sidecar_files_are_found_next_to_the_image() {
        let dir = std::env::temp_dir().join(format!("bootforge-sidecar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("nimbus.iso");
        std::fs::write(&image, b"abc").unwrap();
        assert_eq!(find_sidecar(&image), None);

        // A list that names the image.
        let list = dir.join("SHA256SUMS");
        std::fs::write(
            &list,
            format!("{EMPTY_SHA256}  else.iso\n{ABC_SHA256} *nimbus.iso\n"),
        )
        .unwrap();
        let found = find_sidecar(&image).unwrap();
        assert_eq!(found.digest, ABC_SHA256);
        assert_eq!(found.file, list);
        // The checksum really is the file's: the comparison says so.
        let sums = hash_file(&image, &Cancel::new(), &mut |_| {}).unwrap();
        assert_eq!(compare(&found.digest, &sums), HashMatch::Sha256);

        // The image's own checksum file wins over the list.
        let own = dir.join("nimbus.iso.md5");
        std::fs::write(&own, ABC_MD5).unwrap();
        let found = find_sidecar(&image).unwrap();
        assert_eq!(found.digest, ABC_MD5);
        assert_eq!(found.file, own);

        // A list that does not mention the image is ignored, as is a huge file.
        std::fs::remove_file(&own).unwrap();
        std::fs::write(&list, format!("{EMPTY_SHA256}  else.iso\n")).unwrap();
        assert_eq!(find_sidecar(&image), None);
        std::fs::write(&list, vec![b'a'; SIDECAR_MAX_BYTES as usize + 1]).unwrap();
        assert_eq!(find_sidecar(&image), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    const EMPTY_MD5: &str = "d41d8cd98f00b204e9800998ecf8427e";
    const EMPTY_SHA1: &str = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const ABC_MD5: &str = "900150983cd24fb0d6963f7d28e17f72";
    const ABC_SHA1: &str = "a9993e364706816aba3e25717850c26c9cd0d89d";
    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn abc() -> Checksums {
        let mut h = MultiHasher::new();
        h.update(b"a");
        h.update(b"bc");
        h.finish()
    }

    #[test]
    fn known_digests() {
        let empty = MultiHasher::new().finish();
        assert_eq!(empty.md5, EMPTY_MD5);
        assert_eq!(empty.sha1, EMPTY_SHA1);
        assert_eq!(empty.sha256, EMPTY_SHA256);
        let abc = abc();
        assert_eq!(abc.md5, ABC_MD5);
        assert_eq!(abc.sha1, ABC_SHA1);
        assert_eq!(abc.sha256, ABC_SHA256);
    }

    #[test]
    fn hash_file_matches_in_memory_hash_and_reports_progress() {
        let path = std::env::temp_dir().join(format!("bootforge-hash-{}.bin", std::process::id()));
        // Two full chunks and a partial one.
        let data: Vec<u8> = (0..2 * CHUNK + 12345).map(|i| (i % 253) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let mut seen = Vec::new();
        let sums = hash_file(&path, &Cancel::new(), &mut |n| seen.push(n)).unwrap();
        let mut h = MultiHasher::new();
        h.update(&data);
        assert_eq!(sums, h.finish());
        assert_eq!(seen.last().copied(), Some(data.len() as u64));
        assert!(seen.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(seen.len(), 3);

        let cancelled = Cancel::new();
        cancelled.cancel();
        assert!(matches!(
            hash_file(&path, &cancelled, &mut |_| {}),
            Err(Error::Cancelled)
        ));
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(
            hash_file(&path, &Cancel::new(), &mut |_| {}),
            Err(Error::Io(..))
        ));
    }

    #[test]
    fn hash_of_an_empty_file() {
        let path =
            std::env::temp_dir().join(format!("bootforge-hash-empty-{}.bin", std::process::id()));
        std::fs::write(&path, b"").unwrap();
        let sums = hash_file(&path, &Cancel::new(), &mut |_| {}).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(sums.sha256, EMPTY_SHA256);
    }

    #[test]
    fn compare_plain_and_decorated() {
        let s = abc();
        assert_eq!(compare(ABC_MD5, &s), HashMatch::Md5);
        assert_eq!(compare(ABC_SHA1, &s), HashMatch::Sha1);
        assert_eq!(compare(ABC_SHA256, &s), HashMatch::Sha256);
        assert_eq!(
            compare(&format!("  {}\r\n", ABC_SHA256.to_uppercase()), &s),
            HashMatch::Sha256
        );
        // sha256sum / md5sum output, text and binary mode.
        assert_eq!(
            compare(&format!("{ABC_SHA256}  some image.iso"), &s),
            HashMatch::Sha256
        );
        assert_eq!(
            compare(&format!("{ABC_MD5} *image.iso"), &s),
            HashMatch::Md5
        );
        // Prefixes.
        assert_eq!(
            compare(&format!("SHA256: {ABC_SHA256}"), &s),
            HashMatch::Sha256
        );
        assert_eq!(
            compare(&format!("sha256:{ABC_SHA256}"), &s),
            HashMatch::Sha256
        );
        assert_eq!(compare(&format!("SHA-1 = {ABC_SHA1}"), &s), HashMatch::Sha1);
        assert_eq!(
            compare(&format!("MD5 (image.iso) = {ABC_MD5}"), &s),
            HashMatch::Md5
        );
        // PowerShell Get-FileHash row.
        assert_eq!(
            compare(
                &format!(
                    "SHA256          {}       D:\\isos\\a.iso",
                    ABC_SHA256.to_uppercase()
                ),
                &s
            ),
            HashMatch::Sha256
        );
    }

    #[test]
    fn compare_grouped_digits_and_multi_line_output() {
        let s = abc();
        // certutil prints the digest on its own line (older versions in byte pairs).
        let spaced: Vec<String> = ABC_SHA1
            .as_bytes()
            .chunks(2)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect();
        let text = format!(
            "SHA1 hash of file image.iso:\r\n{}\r\nCertUtil: -hashfile command completed successfully.",
            spaced.join(" ")
        );
        assert_eq!(compare(&text, &s), HashMatch::Sha1);
        // A file name made of hex digits does not confuse the match.
        assert_eq!(compare(&format!("{ABC_MD5}  deadbeef"), &s), HashMatch::Md5);
        assert_eq!(compare(&format!("{ABC_MD5} cafe"), &s), HashMatch::Md5);
    }

    #[test]
    fn compare_mismatch_and_garbage() {
        let s = abc();
        assert_eq!(compare(EMPTY_SHA256, &s), HashMatch::Mismatch);
        assert_eq!(compare(EMPTY_MD5, &s), HashMatch::Mismatch);
        // One digit short, one digit wrong.
        assert_eq!(compare(&ABC_SHA256[1..], &s), HashMatch::NotAHash);
        let mut wrong = ABC_SHA256.to_string();
        wrong.replace_range(0..1, "0");
        assert_eq!(compare(&wrong, &s), HashMatch::Mismatch);
        assert_eq!(compare("", &s), HashMatch::NotAHash);
        assert_eq!(compare("   \n", &s), HashMatch::NotAHash);
        assert_eq!(compare("hello world", &s), HashMatch::NotAHash);
        assert_eq!(compare("abcdef", &s), HashMatch::NotAHash);
        assert_eq!(compare("image.iso", &s), HashMatch::NotAHash);
        // Checksums that were never computed never match.
        assert_eq!(compare("", &Checksums::default()), HashMatch::NotAHash);
        assert_eq!(compare(ABC_MD5, &Checksums::default()), HashMatch::Mismatch);
    }
}
