//! Windows imaging (WIM / ESD / SWM) metadata: the image list from the XML
//! data resource named in the file header.
//!
//! Only the 208-byte header and the XML (stored uncompressed, UTF-16LE with
//! a byte order mark) are read; nothing is decompressed.

use super::{le16, le32, le64};
use crate::error::{Error, Result};
use crate::model::Arch;

/// Size of the WIM header.
pub const WIM_HEADER_LEN: usize = 208;
/// Largest XML resource that is read.
pub const MAX_XML_BYTES: u64 = 8 * 1024 * 1024;

/// One image (edition) of a WIM file.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct WimImage {
    /// 1-based index (the `INDEX` attribute; position when missing).
    pub index: u32,
    /// `DISPLAYNAME`, else `NAME` ("Windows 11 Pro").
    pub name: String,
    /// From `ARCH`: 0 x86, 9 x64, 5 ARM, 12 ARM64, 6 Itanium.
    pub arch: Option<Arch>,
    /// "MAJOR.MINOR.BUILD" ("10.0.26100"); empty when not recorded.
    pub version: String,
}

/// What the header and XML of a WIM file say.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct WimInfo {
    /// Image count from the header.
    pub image_count: u32,
    /// Part number and total parts (1 of 1 unless the file is a split `.swm`).
    pub part: u16,
    pub total_parts: u16,
    /// Images described by the XML, in file order.
    pub images: Vec<WimImage>,
}

impl WimInfo {
    /// Distinct architectures of the images, sorted.
    pub fn archs(&self) -> Vec<Arch> {
        let mut v: Vec<Arch> = self.images.iter().filter_map(|i| i.arch).collect();
        v.sort();
        v.dedup();
        v
    }
}

/// Maps the `ARCH` number of the WIM XML.
pub fn arch_from_code(code: u32) -> Option<Arch> {
    match code {
        0 => Some(Arch::X86),
        9 => Some(Arch::X64),
        5 => Some(Arch::Arm),
        12 => Some(Arch::Arm64),
        6 => Some(Arch::Ia64),
        _ => None,
    }
}

/// Reads the WIM metadata of a file of `size` bytes through `read`, which
/// fills a buffer from a byte offset within that file.
///
/// Fails with `Error::Invalid` when the header is not a WIM header, the XML
/// resource lies outside the file, is compressed or larger than
/// [`MAX_XML_BYTES`].
pub fn read_wim_info(
    size: u64,
    read: &mut dyn FnMut(u64, &mut [u8]) -> Result<()>,
) -> Result<WimInfo> {
    if size < WIM_HEADER_LEN as u64 {
        return Err(Error::invalid("the file is too small to be a WIM image"));
    }
    let mut h = [0u8; WIM_HEADER_LEN];
    read(0, &mut h)?;
    if &h[0..8] != b"MSWIM\0\0\0" {
        return Err(Error::invalid("the file has no WIM header"));
    }
    // XML data resource entry: 7 bytes of stored size, 1 byte of flags,
    // then the offset and the original size.
    let stored = le64(&h, 72) & 0x00FF_FFFF_FFFF_FFFF;
    let flags = h[79];
    let offset = le64(&h, 80);
    let original = le64(&h, 88);
    if flags & 0x04 != 0 || stored != original {
        return Err(Error::invalid("the WIM XML data is compressed"));
    }
    if stored == 0 {
        return Err(Error::invalid("the WIM file has no XML data"));
    }
    if stored > MAX_XML_BYTES {
        return Err(Error::invalid("the WIM XML data is implausibly large"));
    }
    match offset.checked_add(stored) {
        Some(end) if end <= size && offset >= WIM_HEADER_LEN as u64 => {}
        _ => {
            return Err(Error::invalid(
                "the WIM XML data lies outside the file (is this one part of a split or incomplete image?)",
            ))
        }
    }
    let mut xml = vec![0u8; stored as usize];
    read(offset, &mut xml)?;
    Ok(WimInfo {
        image_count: le32(&h, 44),
        part: le16(&h, 40),
        total_parts: le16(&h, 42),
        images: parse_xml(&xml),
    })
}

/// Extracts the image list from the raw XML bytes (UTF-16LE, with or
/// without a byte order mark). Tolerant: unknown or missing tags are skipped.
pub fn parse_xml(bytes: &[u8]) -> Vec<WimImage> {
    let body = bytes.strip_prefix(&[0xFF, 0xFE]).unwrap_or(bytes);
    let units: Vec<u16> = body
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let text = String::from_utf16_lossy(&units);
    parse_xml_text(&text)
}

/// Same as [`parse_xml`] for already decoded text.
pub fn parse_xml_text(text: &str) -> Vec<WimImage> {
    let mut images = Vec::new();
    let mut rest = text;
    while let Some(start) = find_open(rest, "IMAGE") {
        let after = &rest[start..];
        let Some(open_end) = after.find('>') else {
            break;
        };
        let attrs = &after[..open_end];
        let body_and_more = &after[open_end + 1..];
        let (body, next) = match body_and_more.find("</IMAGE>") {
            Some(end) => (&body_and_more[..end], &body_and_more[end + 8..]),
            None => (body_and_more, ""),
        };
        let index = attribute(attrs, "INDEX")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(images.len() as u32 + 1);
        let name = tag_text(body, "DISPLAYNAME")
            .filter(|s| !s.is_empty())
            .or_else(|| tag_text(body, "NAME"))
            .unwrap_or_default();
        let arch = tag_text(body, "ARCH")
            .and_then(|s| s.parse::<u32>().ok())
            .and_then(arch_from_code);
        let version = match tag_raw(body, "VERSION") {
            Some(v) => {
                let num = |tag: &str| tag_text(v, tag).and_then(|s| s.parse::<u32>().ok());
                match (num("MAJOR"), num("MINOR"), num("BUILD")) {
                    (Some(a), Some(b), Some(c)) => format!("{a}.{b}.{c}"),
                    (Some(a), Some(b), None) => format!("{a}.{b}"),
                    _ => String::new(),
                }
            }
            None => String::new(),
        };
        images.push(WimImage {
            index,
            name,
            arch,
            version,
        });
        if images.len() >= 4096 {
            break;
        }
        rest = next;
    }
    images
}

/// Position of `<NAME` followed by `>`, `/` or white space.
fn find_open(text: &str, name: &str) -> Option<usize> {
    let pat = format!("<{name}");
    let mut from = 0;
    while let Some(i) = text[from..].find(&pat) {
        let at = from + i;
        let next = text[at + pat.len()..].chars().next();
        match next {
            Some(c) if c == '>' || c == '/' || c.is_whitespace() => return Some(at),
            _ => from = at + pat.len(),
        }
    }
    None
}

/// The raw text between `<NAME ...>` and `</NAME>`.
fn tag_raw<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let start = find_open(text, name)?;
    let after = &text[start..];
    let open_end = after.find('>')?;
    if after[..open_end].ends_with('/') {
        return Some("");
    }
    let body = &after[open_end + 1..];
    let end = body.find(&format!("</{name}>"))?;
    Some(&body[..end])
}

/// The trimmed text of a tag with the predefined entities decoded.
fn tag_text(text: &str, name: &str) -> Option<String> {
    tag_raw(text, name).map(|s| unescape(s.trim()))
}

fn attribute<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let at = attrs.find(&format!("{name}="))?;
    let rest = &attrs[at + name.len() + 1..];
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let rest = &rest[1..];
    rest.find(quote).map(|end| &rest[..end])
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}
