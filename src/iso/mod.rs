//! ISO 9660 / Joliet / UDF / El Torito reading, WIM metadata, image inspection and the synthetic image builder.
//!
//! * [`IsoImage`] opens an optical image and exposes one flattened file tree,
//!   taken from the best name space present (UDF, then Joliet, then ISO 9660
//!   with Rock Ridge names, then plain ISO 9660).
//! * [`eltorito`] parses the El Torito boot catalog.
//! * [`hybrid`] finds an MBR / GPT inside the image (isohybrid, raw disk images).
//! * [`wim`] reads the image list out of a Windows `install.wim` header.
//! * [`inspect`] ties these together into a [`crate::model::ImageInfo`].
//! * [`build`] produces small synthetic images for tests and the self-test.
//!
//! Everything here only reads: nothing in this module writes to a device.
//! The parsers treat the image as untrusted: every offset is checked against
//! the image size, directory walks are bounded and malformed structures give
//! `Error::Invalid` instead of a panic.

pub mod build;
pub mod eltorito;
mod fatmini;
pub mod hybrid;
mod inspect;
mod iso9660;
mod udf;
pub mod wim;

#[cfg(test)]
mod tests;

pub use inspect::{inspect, inspect_source};

use crate::blockdev::ReadAt;
use crate::error::{Error, Result};
use crate::model::BootEntry;
use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Mutex;

/// Size of an optical sector in bytes.
pub const SECTOR_SIZE: u64 = 2048;

/// Deepest directory nesting that is followed.
pub(crate) const MAX_DEPTH: usize = 64;
/// Largest single directory that is read.
pub(crate) const MAX_DIR_BYTES: u64 = 64 * 1024 * 1024;
/// Directory bytes read in total for one tree.
pub(crate) const MAX_DIR_BYTES_TOTAL: u64 = 256 * 1024 * 1024;
/// Most entries one tree may hold.
pub(crate) const MAX_ENTRIES: usize = 1_000_000;
/// Most extents one file may have.
pub(crate) const MAX_EXTENTS: usize = 65_536;
/// Most notes collected while reading one image.
pub(crate) const MAX_NOTES: usize = 24;

/// One contiguous run of file data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    /// Byte offset of the run in the image (not necessarily sector-aligned:
    /// small UDF files can be embedded in their file entry). Meaningless when
    /// `sparse` is set.
    pub offset: u64,
    /// Length of the run in bytes.
    pub len: u64,
    /// The run is not recorded in the image (UDF sparse / unrecorded extent):
    /// it reads as zeros. Always `false` for ISO 9660 and Joliet.
    pub sparse: bool,
}

/// A recording date and time as stored in the image (local time of the
/// recorder plus its offset from UTC).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateTime {
    pub year: u16,
    /// 1-12.
    pub month: u8,
    /// 1-31.
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    /// Offset of the recorded local time from UTC in minutes (0 when not recorded).
    pub utc_offset_minutes: i16,
}

impl DateTime {
    /// `None` unless the fields form a plausible date and time.
    pub fn checked(
        year: i32,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
        utc_offset_minutes: i32,
    ) -> Option<DateTime> {
        if !(1..=9999).contains(&year)
            || !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || hour > 23
            || minute > 59
            || second > 60
            || !(-1440..=1440).contains(&utc_offset_minutes)
        {
            return None;
        }
        Some(DateTime {
            year: year as u16,
            month,
            day,
            hour,
            minute,
            second: second.min(59),
            utc_offset_minutes: utc_offset_minutes as i16,
        })
    }
}

/// One file or directory of the image's file tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Path inside the image: forward slashes, no leading slash, case as
    /// recorded. Name components never contain slashes, backslashes or control
    /// characters and are never `.` or `..` (such names are rewritten with `_`).
    pub path: String,
    pub is_dir: bool,
    /// File size in bytes (0 for directories).
    pub size: u64,
    /// Where the data lives, in file order. For files the extent lengths add
    /// up to `size` exactly; adjacent runs are merged. Empty for directories
    /// and zero-length files.
    pub extents: Vec<Extent>,
    /// Modification time as recorded, when it is a valid date.
    pub mtime: Option<DateTime>,
}

impl Entry {
    /// The last path component.
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or("")
    }
}

/// The name space an [`IsoImage`] took its file tree from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameSpace {
    Udf,
    Joliet,
    /// ISO 9660 with Rock Ridge `NM` alternate names.
    RockRidge,
    /// Plain ISO 9660 names (version suffix and trailing dot removed).
    Iso9660,
}

impl NameSpace {
    pub fn label(self) -> &'static str {
        match self {
            NameSpace::Udf => "UDF",
            NameSpace::Joliet => "Joliet",
            NameSpace::RockRidge => "ISO 9660 with Rock Ridge",
            NameSpace::Iso9660 => "ISO 9660",
        }
    }
}

/// An opened ISO 9660 / UDF image.
///
/// The image is read through a mutex, so `read_at` and `open_file` take
/// `&self` and entries borrowed from [`IsoImage::entries`] can be passed
/// straight back in.
pub struct IsoImage {
    src: Mutex<Box<dyn ReadAt>>,
    size: u64,
    entries: Vec<Entry>,
    index: HashMap<String, usize>,
    name_space: NameSpace,
    iso_label: Option<String>,
    joliet_label: Option<String>,
    udf_label: Option<String>,
    has_rock_ridge: bool,
    udf_revision: Option<String>,
    volume_size: Option<u64>,
    boot_entries: Vec<BootEntry>,
    notes: Vec<String>,
}

impl std::fmt::Debug for IsoImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IsoImage")
            .field("size", &self.size)
            .field("name_space", &self.name_space)
            .field("entries", &self.entries.len())
            .finish()
    }
}

/// Which tree to try, in order of preference.
enum Candidate<'a> {
    Udf(&'a udf::Mount),
    Joliet(&'a iso9660::VolDesc),
    Iso(&'a iso9660::VolDesc),
}

impl Candidate<'_> {
    fn label(&self) -> &'static str {
        match self {
            Candidate::Udf(_) => "UDF",
            Candidate::Joliet(_) => "Joliet",
            Candidate::Iso(_) => "ISO 9660",
        }
    }
}

impl IsoImage {
    /// Opens an image of 2048-byte sectors.
    ///
    /// Fails with `Error::Invalid` when the source holds neither an ISO 9660
    /// nor a UDF volume, or when none of its file trees can be read at all.
    /// Problems that could be worked around (a damaged preferred tree, an
    /// unsupported UDF variant, a bad boot catalog, unreadable
    /// sub-directories) are reported through [`IsoImage::notes`] instead.
    pub fn open(mut src: Box<dyn ReadAt>) -> Result<IsoImage> {
        let size = src.size();
        let s: &mut dyn ReadAt = src.as_mut();
        let mut notes: Vec<String> = Vec::new();

        let desc = iso9660::read_descriptors(s);
        let udf_info = udf::probe(s, &mut notes);
        if desc.primary.is_none() && udf_info.is_none() {
            return Err(Error::invalid("This is not an ISO 9660 or UDF image."));
        }

        let has_rock_ridge = desc
            .primary
            .as_ref()
            .map(|v| iso9660::detect_rock_ridge(s, v))
            .unwrap_or(false);

        let mut candidates: Vec<Candidate> = Vec::new();
        let mut failures: Vec<(&'static str, String)> = Vec::new();
        if let Some(info) = &udf_info {
            match &info.mount {
                Ok(m) => candidates.push(Candidate::Udf(m)),
                Err(e) => failures.push(("UDF", e.to_string())),
            }
        }
        if let Some(v) = &desc.joliet {
            candidates.push(Candidate::Joliet(v));
        }
        if let Some(v) = &desc.primary {
            candidates.push(Candidate::Iso(v));
        }

        let mut chosen: Option<(NameSpace, Vec<Entry>)> = None;
        'passes: for lenient in [false, true] {
            for cand in &candidates {
                let mut local: Vec<String> = Vec::new();
                let result = match cand {
                    Candidate::Udf(m) => {
                        udf::read_tree(s, m, lenient, &mut local).map(|e| (NameSpace::Udf, e))
                    }
                    Candidate::Joliet(v) => iso9660::read_tree(s, v, false, lenient, &mut local)
                        .map(|(e, _)| (NameSpace::Joliet, e)),
                    Candidate::Iso(v) => {
                        iso9660::read_tree(s, v, true, lenient, &mut local).map(|(e, named)| {
                            let ns = if named {
                                NameSpace::RockRidge
                            } else {
                                NameSpace::Iso9660
                            };
                            (ns, e)
                        })
                    }
                };
                match result {
                    Ok(found) => {
                        for n in local {
                            push_note(&mut notes, n);
                        }
                        chosen = Some(found);
                        break 'passes;
                    }
                    Err(e) => {
                        if !lenient {
                            failures.push((cand.label(), e.to_string()));
                        }
                    }
                }
            }
        }

        let (name_space, entries) = match chosen {
            Some(c) => c,
            None => {
                let why = failures
                    .first()
                    .map(|(ns, e)| format!("{ns}: {e}"))
                    .unwrap_or_else(|| "no file system found".to_string());
                return Err(Error::invalid(format!(
                    "The image's file system cannot be read ({why})."
                )));
            }
        };

        let chosen_label = match name_space {
            NameSpace::Udf => "UDF",
            NameSpace::Joliet => "Joliet",
            _ => "ISO 9660",
        };
        for (ns, why) in &failures {
            if *ns != chosen_label {
                push_note(
                    &mut notes,
                    format!(
                        "The {ns} file system could not be used ({}); the {chosen_label} names are shown instead.",
                        why.trim_end_matches('.')
                    ),
                );
            } else {
                push_note(
                    &mut notes,
                    format!(
                        "Parts of the {ns} file system are damaged ({}).",
                        why.trim_end_matches('.')
                    ),
                );
            }
        }

        let mut boot_entries = Vec::new();
        if let Some(lba) = desc.boot_catalog {
            match eltorito::read_catalog(s, lba) {
                Ok(v) => boot_entries = v,
                Err(e) => push_note(
                    &mut notes,
                    format!(
                        "The El Torito boot catalog is damaged ({}).",
                        e.to_string().trim_end_matches('.')
                    ),
                ),
            }
        }

        let volume_size = match &desc.primary {
            Some(v) => Some(v.space_blocks as u64 * v.block_size as u64),
            None => udf_info.as_ref().and_then(|u| u.volume_end),
        };

        let mut index = HashMap::with_capacity(entries.len());
        for (i, e) in entries.iter().enumerate() {
            index.entry(e.path.to_lowercase()).or_insert(i);
        }

        Ok(IsoImage {
            size,
            entries,
            index,
            name_space,
            iso_label: desc.primary.as_ref().map(|v| v.label.clone()),
            joliet_label: desc.joliet.as_ref().map(|v| v.label.clone()),
            udf_label: udf_info.as_ref().and_then(|u| u.label.clone()),
            has_rock_ridge,
            udf_revision: udf_info
                .as_ref()
                .map(|u| u.revision.clone().unwrap_or_else(|| "unknown".to_string())),
            volume_size,
            boot_entries,
            notes,
            src: Mutex::new(src),
        })
    }

    /// The flattened file tree of the chosen name space. A directory comes
    /// before its children (depth first); within a directory the order is the
    /// recorded one.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Looks a path up, ignoring case and accepting either slash (a leading
    /// or trailing slash is ignored too). With duplicate names the first wins.
    pub fn find(&self, path: &str) -> Option<&Entry> {
        let key = path.replace('\\', "/").trim_matches('/').to_lowercase();
        self.index.get(&key).map(|i| &self.entries[*i])
    }

    /// Fills `buf` with file data starting at `offset` within the file.
    /// Fails when the range does not lie inside the file or the data lies
    /// beyond the end of the image (a truncated download).
    pub fn read_at(&self, entry: &Entry, offset: u64, buf: &mut [u8]) -> Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .filter(|e| *e <= entry.size)
            .ok_or_else(|| Error::invalid(format!("read past the end of {}", entry.path)))?;
        let mut src = self.src.lock().unwrap_or_else(|e| e.into_inner());
        let mut pos = 0u64; // file offset of the current extent
        let mut done = 0usize;
        for x in &entry.extents {
            let x_end = pos.saturating_add(x.len);
            let from = offset + done as u64;
            if from >= end {
                break;
            }
            if from < x_end && from >= pos {
                let within = from - pos;
                let take = (x.len - within).min(end - from) as usize;
                let out = &mut buf[done..done + take];
                if x.sparse {
                    out.fill(0);
                } else {
                    let at = x.offset.checked_add(within).ok_or_else(|| {
                        Error::invalid(format!("bad data location for {}", entry.path))
                    })?;
                    read_bytes(src.as_mut(), at, out, &entry.path)?;
                }
                done += take;
            }
            pos = x_end;
        }
        if done != buf.len() {
            return Err(Error::invalid(format!(
                "the data of {} is not fully described by the image",
                entry.path
            )));
        }
        Ok(())
    }

    /// A streaming reader over the whole file (all extents; sparse UDF
    /// extents read as zeros). Also implements `Seek`.
    pub fn open_file(&self, entry: &Entry) -> FileReader<'_> {
        FileReader {
            image: self,
            entry: entry.clone(),
            pos: 0,
        }
    }

    /// The name space the tree was taken from.
    pub fn name_space(&self) -> NameSpace {
        self.name_space
    }

    /// Volume label: the UDF logical volume identifier, else the Joliet one,
    /// else the ISO 9660 one (trimmed; empty labels are skipped).
    pub fn label(&self) -> &str {
        [&self.udf_label, &self.joliet_label, &self.iso_label]
            .into_iter()
            .flatten()
            .map(|s| s.as_str())
            .find(|s| !s.is_empty())
            .unwrap_or("")
    }

    /// Label of the ISO 9660 primary volume descriptor, if there is one.
    pub fn iso_label(&self) -> Option<&str> {
        self.iso_label.as_deref()
    }
    /// Label of the Joliet supplementary volume descriptor, if there is one.
    pub fn joliet_label(&self) -> Option<&str> {
        self.joliet_label.as_deref()
    }
    /// UDF logical volume identifier, if there is a UDF volume.
    pub fn udf_label(&self) -> Option<&str> {
        self.udf_label.as_deref()
    }

    /// An ISO 9660 primary volume descriptor is present.
    pub fn has_iso9660(&self) -> bool {
        self.iso_label.is_some()
    }
    /// A Joliet supplementary volume descriptor is present.
    pub fn has_joliet(&self) -> bool {
        self.joliet_label.is_some()
    }
    /// The ISO 9660 tree carries Rock Ridge (SUSP) records.
    pub fn has_rock_ridge(&self) -> bool {
        self.has_rock_ridge
    }
    /// UDF revision ("1.02", "2.50", "unknown") when a UDF volume is present,
    /// even if its tree could not be used.
    pub fn udf_revision(&self) -> Option<&str> {
        self.udf_revision.as_deref()
    }

    /// El Torito entries in catalog order (empty when the image is not bootable).
    pub fn boot_entries(&self) -> &[BootEntry] {
        &self.boot_entries
    }

    /// Size in bytes of the volume as recorded in its descriptors (ISO 9660
    /// volume space, else the end of the UDF partition).
    pub fn volume_size(&self) -> Option<u64> {
        self.volume_size
    }

    /// Size of the image file in bytes.
    pub fn image_size(&self) -> u64 {
        self.size
    }

    /// Things that went wrong but could be worked around.
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// Reads raw bytes of the image itself (not of a file in it).
    pub fn read_image(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut src = self.src.lock().unwrap_or_else(|e| e.into_inner());
        read_bytes(src.as_mut(), offset, buf, "image data")
    }

    /// Runs `f` with the underlying source.
    pub(crate) fn with_source<T>(&self, f: impl FnOnce(&mut dyn ReadAt) -> T) -> T {
        let mut src = self.src.lock().unwrap_or_else(|e| e.into_inner());
        f(src.as_mut())
    }
}

/// Streaming reader returned by [`IsoImage::open_file`].
pub struct FileReader<'a> {
    image: &'a IsoImage,
    entry: Entry,
    pos: u64,
}

impl FileReader<'_> {
    /// Total size of the file.
    pub fn size(&self) -> u64 {
        self.entry.size
    }
}

impl io::Read for FileReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.entry.size.saturating_sub(self.pos);
        let n = (buf.len() as u64).min(left) as usize;
        if n == 0 {
            return Ok(0);
        }
        self.image
            .read_at(&self.entry, self.pos, &mut buf[..n])
            .map_err(|e| match e {
                Error::Io(_, e) => e,
                other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
            })?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl io::Seek for FileReader<'_> {
    fn seek(&mut self, to: io::SeekFrom) -> io::Result<u64> {
        let target = match to {
            io::SeekFrom::Start(n) => Some(n),
            io::SeekFrom::End(d) => self.entry.size.checked_add_signed(d),
            io::SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        match target {
            Some(t) => {
                self.pos = t;
                Ok(t)
            }
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start of the file",
            )),
        }
    }
}

// --------------------------------------------------------------------------------------
// Helpers shared by the parsers
// --------------------------------------------------------------------------------------

/// Reads exactly `buf.len()` bytes at `offset`, after checking the range
/// against the size of the source.
pub(crate) fn read_bytes(
    src: &mut dyn ReadAt,
    offset: u64,
    buf: &mut [u8],
    what: &str,
) -> Result<()> {
    match offset.checked_add(buf.len() as u64) {
        Some(end) if end <= src.size() => {}
        _ => {
            return Err(Error::invalid(format!(
                "{what} lies beyond the end of the image"
            )))
        }
    }
    src.read_at(offset, buf)
        .map_err(|e| Error::io(format!("Reading {what}"), e))
}

/// Reads `len` bytes at `offset` into a new buffer; `len` must not exceed `cap`.
pub(crate) fn read_vec(
    src: &mut dyn ReadAt,
    offset: u64,
    len: u64,
    cap: u64,
    what: &str,
) -> Result<Vec<u8>> {
    if len > cap {
        return Err(Error::invalid(format!("{what} is implausibly large")));
    }
    match offset.checked_add(len) {
        Some(end) if end <= src.size() => {}
        _ => {
            return Err(Error::invalid(format!(
                "{what} lies beyond the end of the image"
            )))
        }
    }
    let mut v = vec![0u8; len as usize];
    src.read_at(offset, &mut v)
        .map_err(|e| Error::io(format!("Reading {what}"), e))?;
    Ok(v)
}

/// Little-endian readers that give 0 instead of panicking when out of range.
pub(crate) fn le16(b: &[u8], o: usize) -> u16 {
    match b.get(o..o.wrapping_add(2)) {
        Some(s) if s.len() == 2 => u16::from_le_bytes([s[0], s[1]]),
        _ => 0,
    }
}
pub(crate) fn le32(b: &[u8], o: usize) -> u32 {
    match b.get(o..o.wrapping_add(4)) {
        Some(s) if s.len() == 4 => u32::from_le_bytes([s[0], s[1], s[2], s[3]]),
        _ => 0,
    }
}
pub(crate) fn le64(b: &[u8], o: usize) -> u64 {
    le32(b, o) as u64 | (le32(b, o.wrapping_add(4)) as u64) << 32
}
pub(crate) fn be32(b: &[u8], o: usize) -> u32 {
    match b.get(o..o.wrapping_add(4)) {
        Some(s) if s.len() == 4 => u32::from_be_bytes([s[0], s[1], s[2], s[3]]),
        _ => 0,
    }
}

/// Adds a note unless it is already there or the list is full.
pub(crate) fn push_note(notes: &mut Vec<String>, note: String) {
    if notes.len() < MAX_NOTES && !notes.contains(&note) {
        notes.push(note);
    }
}

/// Makes a recorded name safe to use as one path component.
pub(crate) fn clean_name(raw: &str) -> String {
    let mut s: String = raw
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    if s.is_empty() || s == "." || s == ".." {
        s = "_".repeat(s.len().max(1));
    }
    s
}

/// Decodes big-endian UTF-16 (Joliet, UDF 16-bit names), replacing bad units.
pub(crate) fn utf16be(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_be_bytes(*c))
        .collect();
    String::from_utf16_lossy(&units)
}

/// Limits shared by one tree walk.
pub(crate) struct Budget {
    dir_bytes: u64,
    entries: usize,
    visited: HashSet<u64>,
}

impl Budget {
    pub(crate) fn new() -> Budget {
        Budget {
            dir_bytes: MAX_DIR_BYTES_TOTAL,
            entries: MAX_ENTRIES,
            visited: HashSet::new(),
        }
    }
    /// Accounts for a directory of `len` bytes about to be read.
    pub(crate) fn take_dir(&mut self, len: u64) -> Result<()> {
        if len > MAX_DIR_BYTES || len > self.dir_bytes {
            return Err(Error::invalid("a directory is implausibly large"));
        }
        self.dir_bytes -= len;
        Ok(())
    }
    pub(crate) fn take_entry(&mut self) -> Result<()> {
        if self.entries == 0 {
            return Err(Error::invalid("the image holds too many files"));
        }
        self.entries -= 1;
        Ok(())
    }
    /// Marks a directory (by its location) as seen; fails on a second visit.
    pub(crate) fn visit(&mut self, key: u64) -> Result<()> {
        if !self.visited.insert(key) {
            return Err(Error::invalid("the directory tree loops back on itself"));
        }
        Ok(())
    }
}

/// Appends a run to an extent list, merging it with the previous one when contiguous.
pub(crate) fn push_extent(list: &mut Vec<Extent>, x: Extent) -> Result<()> {
    if x.len == 0 {
        return Ok(());
    }
    if let Some(last) = list.last_mut() {
        let joins = if x.sparse {
            last.sparse
        } else {
            !last.sparse && last.offset.checked_add(last.len) == Some(x.offset)
        };
        if joins {
            last.len = last.len.saturating_add(x.len);
            return Ok(());
        }
    }
    if list.len() >= MAX_EXTENTS {
        return Err(Error::invalid("a file is split into too many pieces"));
    }
    list.push(x);
    Ok(())
}
