//! Synthetic image builder: small ISO 9660 / Joliet / Rock Ridge / UDF 1.02
//! images assembled in memory from the standards, for tests and the
//! self-test. Output is deterministic (fixed dates and identifiers).
//!
//! The images are valid enough for this crate's reader and follow the
//! standards' layouts, but the builder is not a mastering tool: names are
//! mangled simply, and there is no support for files of 1 GiB or more of
//! real data (use a sparse tail for size tests).

use super::udf::{
    crc16, tag_checksum, TAG_ALLOC_EXTENT, TAG_ANCHOR, TAG_EXT_FILE_ENTRY, TAG_FILE_ENTRY,
    TAG_FILE_ID, TAG_FILE_SET, TAG_IMPL_USE, TAG_INTEGRITY, TAG_LOGICAL_VOLUME, TAG_PARTITION,
    TAG_PRIMARY, TAG_TERMINATOR, TAG_UNALLOCATED,
};
use crate::model::Arch;

const SEC: usize = 2048;
/// Largest length one UDF allocation descriptor can describe (block multiple).
const MAX_AD_LEN: u64 = 0x3FFF_F800;
/// Sector of the main and reserve UDF volume descriptor sequences and the
/// logical volume integrity descriptor.
const UDF_MAIN_VDS: u32 = 32;
const UDF_RESERVE_VDS: u32 = 48;
const UDF_LVID: u32 = 64;

fn w16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn w32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn w64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}
fn both16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
    b[o + 2..o + 4].copy_from_slice(&v.to_be_bytes());
}
fn both32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
    b[o + 4..o + 8].copy_from_slice(&v.to_be_bytes());
}
fn sectors(bytes: usize) -> u32 {
    bytes.div_ceil(SEC) as u32
}
fn utf16be(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()
}

/// Deterministic filler bytes (used by the samples and by tests to check
/// file contents): byte `i` is `(i * mul + add) mod 256`.
pub fn pattern(len: usize, mul: u8, add: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(mul).wrapping_add(add))
        .collect()
}

/// Fills in the descriptor tag at the start of a UDF descriptor: identifier,
/// location, CRC over `crc_len` bytes after the tag, then the tag checksum.
pub(crate) fn seal_tag(buf: &mut [u8], id: u16, location: u32, crc_len: usize) {
    w16(buf, 0, id);
    w16(buf, 2, 2);
    buf[4] = 0;
    buf[5] = 0;
    w16(buf, 6, 0);
    w16(buf, 10, crc_len as u16);
    w32(buf, 12, location);
    let crc = crc16(&buf[16..16 + crc_len]);
    w16(buf, 8, crc);
    buf[4] = tag_checksum(buf);
}

fn regid(id: &str, suffix: &[u8]) -> [u8; 32] {
    let mut r = [0u8; 32];
    r[1..1 + id.len()].copy_from_slice(id.as_bytes());
    r[24..24 + suffix.len()].copy_from_slice(suffix);
    r
}

fn charspec() -> [u8; 64] {
    let mut c = [0u8; 64];
    let text = b"OSTA Compressed Unicode";
    c[1..1 + text.len()].copy_from_slice(text);
    c
}

/// OSTA compressed unicode: 8-bit when every character fits, else 16-bit.
fn cs0(text: &str) -> Vec<u8> {
    if text.is_empty() {
        return Vec::new();
    }
    if text.chars().all(|c| (c as u32) < 0x100) {
        let mut v = vec![8u8];
        v.extend(text.chars().map(|c| c as u32 as u8));
        v
    } else {
        let mut v = vec![16u8];
        v.extend(utf16be(text));
        v
    }
}

fn dstring(field: &mut [u8], text: &str) {
    let mut v = cs0(text);
    v.truncate(field.len() - 1);
    field[..v.len()].copy_from_slice(&v);
    let last = field.len() - 1;
    field[last] = v.len() as u8;
}

/// Fixed recording time: 2024-01-02 03:04:05, local time at UTC+0.
fn udf_time(b: &mut [u8], o: usize) {
    w16(b, o, 0x1000);
    w16(b, o + 2, 2024);
    b[o + 4..o + 9].copy_from_slice(&[1, 2, 3, 4, 5]);
}

const ISO_TIME: [u8; 7] = [124, 1, 2, 3, 4, 5, 0];

fn dir_record(name: &[u8], lba: u32, len: u32, flags: u8, su: &[u8]) -> Vec<u8> {
    let pad = if name.len().is_multiple_of(2) { 1 } else { 0 };
    let mut total = 33 + name.len() + pad + su.len();
    total += total & 1;
    assert!(total <= 255, "directory record too long");
    let mut r = vec![0u8; total];
    r[0] = total as u8;
    both32(&mut r, 2, lba);
    both32(&mut r, 10, len);
    r[18..25].copy_from_slice(&ISO_TIME);
    r[25] = flags;
    both16(&mut r, 28, 1);
    r[32] = name.len() as u8;
    r[33..33 + name.len()].copy_from_slice(name);
    let at = 33 + name.len() + pad;
    r[at..at + su.len()].copy_from_slice(su);
    r
}

/// Appends a record, padding to the next sector when it would cross one.
fn push_record(out: &mut Vec<u8>, rec: Vec<u8>) {
    if out.len() % SEC + rec.len() > SEC {
        out.resize(out.len().div_ceil(SEC) * SEC, 0);
    }
    out.extend(rec);
}

fn rr_px(is_dir: bool) -> Vec<u8> {
    let mut p = vec![0u8; 36];
    p[0..4].copy_from_slice(&[b'P', b'X', 36, 1]);
    both32(&mut p, 4, if is_dir { 0o040555 } else { 0o100444 });
    both32(&mut p, 12, 1);
    p
}

fn rr_nm(flags: u8, name: &[u8]) -> Vec<u8> {
    let mut v = vec![b'N', b'M', (5 + name.len()) as u8, 1, flags];
    v.extend_from_slice(name);
    v
}

/// Rock Ridge continuation areas, packed into sectors.
struct CePool {
    base: u32,
    buf: Vec<u8>,
}

impl CePool {
    fn alloc(&mut self, bytes: &[u8]) -> (u32, u32) {
        if self.buf.len() % SEC + bytes.len() > SEC {
            self.buf.resize(self.buf.len().div_ceil(SEC) * SEC, 0);
        }
        let at = self.buf.len();
        self.buf.extend_from_slice(bytes);
        (self.base + (at / SEC) as u32, (at % SEC) as u32)
    }
}

#[derive(Clone)]
struct Node {
    name: String,
    parent: usize,
    is_dir: bool,
    children: Vec<usize>,
    file: usize,
}

struct Tree {
    nodes: Vec<Node>,
}

impl Tree {
    fn new() -> Tree {
        Tree {
            nodes: vec![Node {
                name: String::new(),
                parent: 0,
                is_dir: true,
                children: Vec::new(),
                file: 0,
            }],
        }
    }

    fn add(&mut self, path: &str, file: Option<usize>) {
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        let mut at = 0usize;
        for (i, part) in parts.iter().enumerate() {
            let last = i + 1 == parts.len();
            let want_dir = !last || file.is_none();
            let found = self.nodes[at]
                .children
                .iter()
                .copied()
                .find(|c| self.nodes[*c].name == *part);
            at = match found {
                Some(c) => {
                    if last {
                        if let Some(f) = file {
                            self.nodes[c].is_dir = false;
                            self.nodes[c].file = f;
                        }
                    }
                    c
                }
                None => {
                    let id = self.nodes.len();
                    self.nodes.push(Node {
                        name: part.to_string(),
                        parent: at,
                        is_dir: want_dir,
                        children: Vec::new(),
                        file: file.unwrap_or(0),
                    });
                    self.nodes[at].children.push(id);
                    id
                }
            };
        }
    }

    fn dirs_bfs(&self) -> Vec<usize> {
        let mut out = vec![0usize];
        let mut i = 0;
        while i < out.len() {
            let d = out[i];
            i += 1;
            for &c in &self.nodes[d].children {
                if self.nodes[c].is_dir {
                    out.push(c);
                }
            }
        }
        out
    }

    fn preorder(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack = vec![0usize];
        while let Some(n) = stack.pop() {
            out.push(n);
            for &c in self.nodes[n].children.iter().rev() {
                stack.push(c);
            }
        }
        out
    }

    /// Upper-case 8.3 style names, unique within each directory.
    fn iso_names(&self) -> Vec<String> {
        let mut names = vec![String::new(); self.nodes.len()];
        for d in self.dirs_bfs() {
            let mut used: Vec<String> = Vec::new();
            for &c in &self.nodes[d].children {
                names[c] = iso_name(&self.nodes[c].name, self.nodes[c].is_dir, &mut used);
            }
        }
        names
    }
}

fn iso_name(name: &str, is_dir: bool, used: &mut Vec<String>) -> String {
    let clean = |s: &str, max: usize| -> String {
        s.chars()
            .map(|c| {
                let u = c.to_ascii_uppercase();
                if u.is_ascii_alphanumeric() || u == '_' {
                    u
                } else {
                    '_'
                }
            })
            .take(max)
            .collect()
    };
    let (stem, ext) = if is_dir {
        (name, "")
    } else {
        match name.rfind('.') {
            Some(i) if i > 0 => (&name[..i], &name[i + 1..]),
            _ => (name, ""),
        }
    };
    let mut s = clean(stem, 8);
    if s.is_empty() {
        s = "_".to_string();
    }
    let e = clean(ext, 3);
    let make = |s: &str| {
        if is_dir {
            s.to_string()
        } else {
            format!("{s}.{e}")
        }
    };
    let mut candidate = make(&s);
    let mut n = 0;
    while used.contains(&candidate) {
        n += 1;
        let suffix = n.to_string();
        let base: String = s.chars().take(8 - suffix.len()).collect();
        candidate = make(&format!("{base}{suffix}"));
    }
    used.push(candidate.clone());
    candidate
}

#[derive(Clone)]
struct FileData {
    data: Vec<u8>,
    /// Bytes after `data` that are not recorded (UDF only).
    sparse_tail: u64,
}

/// Builds a synthetic optical image in memory.
///
/// ```ignore
/// let image = IsoBuilder::new("DEMO")
///     .joliet(true)
///     .file("docs/readme.txt", b"hello".to_vec())
///     .build();
/// ```
#[derive(Clone)]
pub struct IsoBuilder {
    label: String,
    iso9660: bool,
    joliet: bool,
    rock_ridge: bool,
    udf: bool,
    windows_style: bool,
    bios_boot: Option<Vec<u8>>,
    uefi_boot: Option<Vec<u8>>,
    hybrid_mbr: bool,
    hybrid_gpt: bool,
    extent_size: Option<u64>,
    udf_long_ads: bool,
    udf_embed: bool,
    udf_extended: bool,
    udf_inline_ads: usize,
    entries: Vec<(String, Option<usize>)>,
    files: Vec<FileData>,
}

impl IsoBuilder {
    /// A plain ISO 9660 image with the given volume label and no files.
    pub fn new(label: &str) -> IsoBuilder {
        IsoBuilder {
            label: label.to_string(),
            iso9660: true,
            joliet: false,
            rock_ridge: false,
            udf: false,
            windows_style: false,
            bios_boot: None,
            uefi_boot: None,
            hybrid_mbr: false,
            hybrid_gpt: false,
            extent_size: None,
            udf_long_ads: false,
            udf_embed: false,
            udf_extended: false,
            udf_inline_ads: 64,
            entries: Vec::new(),
            files: Vec::new(),
        }
    }

    /// Write the ISO 9660 volume (default on). Off gives a UDF-only image
    /// (UDF is then switched on; El Torito needs ISO 9660 and is left out).
    pub fn iso9660(mut self, on: bool) -> IsoBuilder {
        self.iso9660 = on;
        self
    }
    /// Add a Joliet supplementary volume (UCS-2 level 3).
    pub fn joliet(mut self, on: bool) -> IsoBuilder {
        self.joliet = on;
        self
    }
    /// Add Rock Ridge `PX`/`NM` records to the ISO 9660 tree. Names that do
    /// not fit their directory record continue in a `CE` area.
    pub fn rock_ridge(mut self, on: bool) -> IsoBuilder {
        self.rock_ridge = on;
        self
    }
    /// Add a UDF 1.02 file system sharing the file data (a "bridge" image).
    pub fn udf(mut self, on: bool) -> IsoBuilder {
        self.udf = on;
        self
    }
    /// Windows setup style: UDF holds every file while the ISO 9660 tree
    /// holds only a `README.TXT`. Implies UDF and no Joliet.
    pub fn windows_style(mut self, on: bool) -> IsoBuilder {
        self.windows_style = on;
        self
    }
    /// El Torito BIOS (platform 0x00) no-emulation entry with this boot image.
    pub fn bios_boot(mut self, image: Vec<u8>) -> IsoBuilder {
        self.bios_boot = Some(image);
        self
    }
    /// El Torito UEFI (platform 0xEF) entry with this EFI system partition
    /// image (see [`fat12_image`]).
    pub fn uefi_boot(mut self, image: Vec<u8>) -> IsoBuilder {
        self.uefi_boot = Some(image);
        self
    }
    /// isohybrid-style MBR in sector 0: a bootable type 0x17 entry covering
    /// the image and, with a UEFI boot image, a type 0xEF entry for it.
    pub fn hybrid_mbr(mut self, on: bool) -> IsoBuilder {
        self.hybrid_mbr = on;
        self
    }
    /// GPT (with protective MBR and backup table) describing the image and
    /// the UEFI boot image.
    pub fn hybrid_gpt(mut self, on: bool) -> IsoBuilder {
        self.hybrid_gpt = on;
        self
    }
    /// Split file data into extents of at most `bytes` (rounded up to whole
    /// sectors) with an unrelated filler sector between them: ISO 9660 and
    /// Joliet then use multi-extent directory records, UDF several
    /// allocation descriptors.
    pub fn extent_size(mut self, bytes: u64) -> IsoBuilder {
        self.extent_size = Some(bytes.max(1).div_ceil(SEC as u64) * SEC as u64);
        self
    }
    /// UDF: use long instead of short allocation descriptors.
    pub fn udf_long_ads(mut self, on: bool) -> IsoBuilder {
        self.udf_long_ads = on;
        self
    }
    /// UDF: embed small directories and files (up to 1024 bytes) in their
    /// file entries.
    pub fn udf_embed(mut self, on: bool) -> IsoBuilder {
        self.udf_embed = on;
        self
    }
    /// UDF: write extended file entries (tag 266) instead of file entries.
    pub fn udf_extended_entries(mut self, on: bool) -> IsoBuilder {
        self.udf_extended = on;
        self
    }
    /// UDF: how many allocation descriptors fit in a file entry (and in each
    /// allocation extent) before the list continues in another block
    /// (2..=100, default 64).
    pub fn udf_inline_ads(mut self, count: usize) -> IsoBuilder {
        self.udf_inline_ads = count.clamp(2, 100);
        self
    }

    /// Adds a file; parent directories are created as needed. `path` uses
    /// forward slashes.
    pub fn file(mut self, path: &str, data: Vec<u8>) -> IsoBuilder {
        self.files.push(FileData {
            data,
            sparse_tail: 0,
        });
        self.entries
            .push((path.to_string(), Some(self.files.len() - 1)));
        self
    }
    /// Adds a file of `total_size` bytes of which only `head` is recorded;
    /// the rest is a sparse (unallocated) UDF extent reading as zeros. Such
    /// files exist only in the UDF tree.
    pub fn sparse_file(mut self, path: &str, head: Vec<u8>, total_size: u64) -> IsoBuilder {
        let mut data = head;
        let total = total_size.max(data.len() as u64);
        // Recorded extents before a sparse one must be whole blocks.
        let padded = (data.len().div_ceil(SEC) * SEC) as u64;
        data.resize(padded.min(total) as usize, 0);
        self.files.push(FileData {
            sparse_tail: total - data.len() as u64,
            data,
        });
        self.entries
            .push((path.to_string(), Some(self.files.len() - 1)));
        self
    }
    /// Adds an (empty) directory.
    pub fn dir(mut self, path: &str) -> IsoBuilder {
        self.entries.push((path.to_string(), None));
        self
    }

    fn chunk_len(&self) -> u64 {
        self.extent_size.unwrap_or(MAX_AD_LEN).min(MAX_AD_LEN)
    }

    /// Number of UDF allocation descriptors a file needs.
    fn ad_count(&self, f: &FileData) -> usize {
        (f.data.len() as u64).div_ceil(self.chunk_len()) as usize
            + f.sparse_tail.div_ceil(MAX_AD_LEN) as usize
    }

    fn udf_embedded_file(&self, f: &FileData) -> bool {
        self.udf_embed && f.sparse_tail == 0 && f.data.len() <= 1024
    }

    fn fe_base(&self) -> usize {
        if self.udf_extended {
            216
        } else {
            176
        }
    }

    /// Assembles the image.
    pub fn build(&self) -> Vec<u8> {
        let iso = self.iso9660;
        let udf = self.udf || self.windows_style || !iso;
        let joliet = iso && self.joliet && !self.windows_style;

        let mut files = self.files.clone();
        let mut full = Tree::new();
        for (path, file) in &self.entries {
            full.add(path, *file);
        }
        // The tree shown by ISO 9660 / Joliet.
        let mut iso_tree = Tree::new();
        if self.windows_style {
            files.push(FileData {
                data: b"This disc contains a \"UDF\" file system and requires an operating system\r\nthat supports the ISO-13346 \"UDF\" file system specification.\r\n".to_vec(),
                sparse_tail: 0,
            });
            iso_tree.add("README.TXT", Some(files.len() - 1));
        } else {
            for (path, file) in &self.entries {
                match file {
                    Some(f) if files[*f].sparse_tail > 0 => {}
                    _ => iso_tree.add(path, *file),
                }
            }
        }
        let iso_names = iso_tree.iso_names();

        // ---- sizes that do not depend on locations --------------------------------
        let k = self.udf_inline_ads;
        let order = full.preorder();
        let mut fe_block = vec![0u32; full.nodes.len()];
        let mut extra_block = vec![0u32; full.nodes.len()]; // directory data or first AED
        let mut udf_blocks = 0u32;
        let mut dir_sizes = vec![0usize; full.nodes.len()];
        if udf {
            let mut cursor = 2u32; // file set descriptor + terminator
            for &n in &order {
                fe_block[n] = cursor;
                cursor += 1;
                let node = &full.nodes[n];
                if node.is_dir {
                    let mut size = 40usize;
                    for &c in &node.children {
                        size += (38 + cs0(&full.nodes[c].name).len() + 3) & !3;
                    }
                    dir_sizes[n] = size;
                    let embedded = self.udf_embed && size <= SEC - self.fe_base();
                    if !embedded {
                        extra_block[n] = cursor;
                        cursor += sectors(size);
                    }
                } else {
                    let f = &files[node.file];
                    if !self.udf_embedded_file(f) {
                        let count = self.ad_count(f);
                        if count > k {
                            extra_block[n] = cursor;
                            let mut rest = count - (k - 1);
                            loop {
                                cursor += 1;
                                if rest <= k {
                                    break;
                                }
                                rest -= k - 1;
                            }
                        }
                    }
                }
            }
            udf_blocks = cursor;
        }

        // ---- layout ---------------------------------------------------------------
        let has_boot = iso && (self.bios_boot.is_some() || self.uefi_boot.is_some());
        let mut descriptors = 0u32;
        if iso {
            descriptors += 2 + has_boot as u32 + joliet as u32;
        }
        if udf {
            descriptors += 3;
        }
        let mut cursor = 16 + descriptors;
        if udf {
            cursor = 257;
        }

        let iso_dirs = iso_tree.dirs_bfs();
        let zero_loc = vec![(0u32, 0u32); iso_tree.nodes.len()];
        let no_alloc: Vec<Vec<(u32, u64)>> = vec![Vec::new(); files.len()];
        // Measuring pass: record sizes do not depend on the locations.
        let measure = |jol: bool| -> (Vec<u32>, usize) {
            let mut ce = CePool {
                base: 0,
                buf: Vec::new(),
            };
            let lens = iso_dirs
                .iter()
                .map(|&d| {
                    let bytes = self.iso_dir(
                        &iso_tree, &iso_names, &files, jol, d, &zero_loc, &no_alloc, &mut ce,
                    );
                    sectors(bytes.len()) * SEC as u32
                })
                .collect();
            (lens, ce.buf.len())
        };
        let mut pt_lba = [0u32; 4];
        let mut pt_len = [0usize; 2];
        let mut iso_loc = zero_loc.clone();
        let mut jol_loc = zero_loc.clone();
        let mut ce_base = 0u32;
        if iso {
            pt_len[0] = self
                .path_table(&iso_tree, &iso_names, false, &zero_loc, false)
                .len();
            pt_lba[0] = cursor;
            pt_lba[1] = cursor + sectors(pt_len[0]);
            cursor += 2 * sectors(pt_len[0]);
            if joliet {
                pt_len[1] = self
                    .path_table(&iso_tree, &iso_names, true, &zero_loc, false)
                    .len();
                pt_lba[2] = cursor;
                pt_lba[3] = cursor + sectors(pt_len[1]);
                cursor += 2 * sectors(pt_len[1]);
            }
            let (lens, ce_len) = measure(false);
            for (i, &d) in iso_dirs.iter().enumerate() {
                iso_loc[d] = (cursor, lens[i]);
                cursor += lens[i] / SEC as u32;
            }
            ce_base = cursor;
            cursor += sectors(ce_len);
            if joliet {
                let (lens, _) = measure(true);
                for (i, &d) in iso_dirs.iter().enumerate() {
                    jol_loc[d] = (cursor, lens[i]);
                    cursor += lens[i] / SEC as u32;
                }
            }
        }
        let mut catalog_lba = 0u32;
        let mut bios_lba = 0u32;
        let mut uefi_lba = 0u32;
        if has_boot {
            catalog_lba = cursor;
            cursor += 1;
            if let Some(b) = &self.bios_boot {
                bios_lba = cursor;
                cursor += sectors(b.len()).max(1);
            }
            if let Some(b) = &self.uefi_boot {
                uefi_lba = cursor;
                cursor += sectors(b.len()).max(1);
            }
        }
        let part_start = cursor;
        cursor += udf_blocks;

        let split = self.extent_size.is_some();
        let chunk = self.chunk_len() as usize;
        let mut alloc: Vec<Vec<(u32, u64)>> = Vec::with_capacity(files.len());
        let mut gaps: Vec<u32> = Vec::new();
        for f in &files {
            let mut list = Vec::new();
            let count = f.data.len().div_ceil(chunk);
            for (i, piece) in f.data.chunks(chunk).enumerate() {
                list.push((cursor, piece.len() as u64));
                cursor += sectors(piece.len());
                if split && i + 1 < count {
                    gaps.push(cursor);
                    cursor += 1;
                }
            }
            alloc.push(list);
        }
        let gpt = self.hybrid_gpt;
        let tail_start = cursor;
        if gpt {
            cursor += 8;
        }
        if udf || gpt {
            cursor += 1;
        }
        let total = cursor;
        let mut img = vec![0u8; total as usize * SEC];
        let put = |img: &mut Vec<u8>, lba: u32, bytes: &[u8]| {
            let at = lba as usize * SEC;
            img[at..at + bytes.len()].copy_from_slice(bytes);
        };

        // ---- file data --------------------------------------------------------------
        for (f, list) in files.iter().zip(&alloc) {
            let mut at = 0usize;
            for (lba, len) in list {
                put(&mut img, *lba, &f.data[at..at + *len as usize]);
                at += *len as usize;
            }
        }
        for g in gaps {
            put(&mut img, g, &[0xEE; SEC]);
        }

        // ---- ISO 9660 / Joliet ------------------------------------------------------
        if iso {
            let mut lba = 16u32;
            let root = |loc: &[(u32, u32)]| dir_record(&[0], loc[0].0, loc[0].1, 2, &[]);
            let mut pvd = self.volume_descriptor(false, total, pt_len[0], pt_lba[0], pt_lba[1]);
            pvd[156..190].copy_from_slice(&root(&iso_loc));
            put(&mut img, lba, &pvd);
            lba += 1;
            if has_boot {
                let mut b = vec![0u8; SEC];
                b[1..6].copy_from_slice(b"CD001");
                b[6] = 1;
                b[7..30].copy_from_slice(b"EL TORITO SPECIFICATION");
                w32(&mut b, 71, catalog_lba);
                put(&mut img, lba, &b);
                lba += 1;
            }
            if joliet {
                let mut svd = self.volume_descriptor(true, total, pt_len[1], pt_lba[2], pt_lba[3]);
                svd[156..190].copy_from_slice(&root(&jol_loc));
                put(&mut img, lba, &svd);
                lba += 1;
            }
            let mut term = vec![0u8; SEC];
            term[0] = 255;
            term[1..6].copy_from_slice(b"CD001");
            term[6] = 1;
            put(&mut img, lba, &term);

            put(
                &mut img,
                pt_lba[0],
                &self.path_table(&iso_tree, &iso_names, false, &iso_loc, false),
            );
            put(
                &mut img,
                pt_lba[1],
                &self.path_table(&iso_tree, &iso_names, false, &iso_loc, true),
            );
            let mut ce = CePool {
                base: ce_base,
                buf: Vec::new(),
            };
            for &d in &iso_dirs {
                let bytes = self.iso_dir(
                    &iso_tree, &iso_names, &files, false, d, &iso_loc, &alloc, &mut ce,
                );
                put(&mut img, iso_loc[d].0, &bytes);
            }
            put(&mut img, ce_base, &ce.buf);
            if joliet {
                put(
                    &mut img,
                    pt_lba[2],
                    &self.path_table(&iso_tree, &iso_names, true, &jol_loc, false),
                );
                put(
                    &mut img,
                    pt_lba[3],
                    &self.path_table(&iso_tree, &iso_names, true, &jol_loc, true),
                );
                for &d in &iso_dirs {
                    let bytes = self.iso_dir(
                        &iso_tree, &iso_names, &files, true, d, &jol_loc, &alloc, &mut ce,
                    );
                    put(&mut img, jol_loc[d].0, &bytes);
                }
            }
        }

        // ---- El Torito --------------------------------------------------------------
        if has_boot {
            let mut cat = vec![0u8; SEC];
            let boot_entry = |image: &[u8], lba: u32, bios: bool| -> [u8; 32] {
                let mut e = [0u8; 32];
                e[0] = 0x88;
                let count = if bios {
                    4
                } else {
                    (image.len().div_ceil(512)).min(0xFFFF) as u16
                };
                w16(&mut e, 6, count);
                w32(&mut e, 8, lba);
                e
            };
            let first_is_bios = self.bios_boot.is_some();
            cat[0] = 1;
            cat[1] = if first_is_bios { 0x00 } else { 0xEF };
            cat[4..13].copy_from_slice(b"BOOTFORGE");
            cat[30] = 0x55;
            cat[31] = 0xAA;
            let mut sum = 0u16;
            for i in 0..16 {
                sum = sum.wrapping_add(u16::from_le_bytes([cat[i * 2], cat[i * 2 + 1]]));
            }
            w16(&mut cat, 28, 0u16.wrapping_sub(sum));
            if let Some(b) = &self.bios_boot {
                cat[32..64].copy_from_slice(&boot_entry(b, bios_lba, true));
                put(&mut img, bios_lba, b);
                if let Some(u) = &self.uefi_boot {
                    cat[64] = 0x91;
                    cat[65] = 0xEF;
                    w16(&mut cat, 66, 1);
                    cat[96..128].copy_from_slice(&boot_entry(u, uefi_lba, false));
                }
            } else if let Some(u) = &self.uefi_boot {
                cat[32..64].copy_from_slice(&boot_entry(u, uefi_lba, false));
            }
            if let Some(u) = &self.uefi_boot {
                put(&mut img, uefi_lba, u);
            }
            put(&mut img, catalog_lba, &cat);
        }

        // ---- UDF --------------------------------------------------------------------
        if udf {
            let first = 16 + if iso { descriptors - 3 } else { 0 };
            for (lba, id) in (first..).zip([b"BEA01", b"NSR02", b"TEA01"]) {
                let mut s = vec![0u8; SEC];
                s[1..6].copy_from_slice(id);
                s[6] = 1;
                put(&mut img, lba, &s);
            }
            let part_len = total - part_start;
            for base in [UDF_MAIN_VDS, UDF_RESERVE_VDS] {
                for (i, d) in self
                    .udf_sequence(part_start, part_len)
                    .iter_mut()
                    .enumerate()
                {
                    let loc = base + i as u32;
                    let (id, crc_len) = (
                        u16::from_le_bytes([d[0], d[1]]),
                        u16::from_le_bytes([d[10], d[11]]),
                    );
                    seal_tag(d, id, loc, crc_len as usize);
                    put(&mut img, loc, d);
                }
            }
            // Logical volume integrity descriptor and its terminator.
            let mut lvid = vec![0u8; SEC];
            udf_time(&mut lvid, 16);
            w32(&mut lvid, 28, 1);
            w32(&mut lvid, 72, 1);
            w32(&mut lvid, 76, 46);
            w32(&mut lvid, 84, part_len);
            lvid[88..120].copy_from_slice(&regid("*BootForge", &[]));
            let dirs = full.nodes.iter().filter(|n| n.is_dir).count() as u32;
            w32(&mut lvid, 120, full.nodes.len() as u32 - dirs);
            w32(&mut lvid, 124, dirs);
            w16(&mut lvid, 128, 0x0102);
            w16(&mut lvid, 130, 0x0102);
            w16(&mut lvid, 132, 0x0102);
            seal_tag(&mut lvid, TAG_INTEGRITY, UDF_LVID, 118);
            put(&mut img, UDF_LVID, &lvid);
            let mut td = vec![0u8; SEC];
            seal_tag(&mut td, TAG_TERMINATOR, UDF_LVID + 1, 496);
            put(&mut img, UDF_LVID + 1, &td);
            // Anchors.
            for loc in [256, total - 1] {
                let mut a = vec![0u8; 512];
                w32(&mut a, 16, 16 * SEC as u32);
                w32(&mut a, 20, UDF_MAIN_VDS);
                w32(&mut a, 24, 16 * SEC as u32);
                w32(&mut a, 28, UDF_RESERVE_VDS);
                seal_tag(&mut a, TAG_ANCHOR, loc, 496);
                put(&mut img, loc, &a);
            }
            // File set descriptor.
            let mut fsd = vec![0u8; SEC];
            udf_time(&mut fsd, 16);
            w16(&mut fsd, 28, 3);
            w16(&mut fsd, 30, 3);
            w32(&mut fsd, 32, 1);
            w32(&mut fsd, 36, 1);
            fsd[48..112].copy_from_slice(&charspec());
            dstring(&mut fsd[112..240], &self.label);
            fsd[240..304].copy_from_slice(&charspec());
            dstring(&mut fsd[304..336], &self.label);
            w32(&mut fsd, 400, SEC as u32);
            w32(&mut fsd, 404, fe_block[0]);
            fsd[416..448].copy_from_slice(&regid("*OSTA UDF Compliant", &[0x02, 0x01, 0x03]));
            seal_tag(&mut fsd, TAG_FILE_SET, 0, 496);
            put(&mut img, part_start, &fsd);
            let mut td = vec![0u8; SEC];
            seal_tag(&mut td, TAG_TERMINATOR, 1, 496);
            put(&mut img, part_start + 1, &td);

            let base = self.fe_base();
            for &n in &order {
                let node = &full.nodes[n];
                let mut fe = vec![0u8; SEC];
                let mut ad_type = if self.udf_long_ads { 1u16 } else { 0 };
                let mut ads: Vec<u8> = Vec::new();
                let size: u64;
                let mut recorded_blocks = 0u64;
                if node.is_dir {
                    // Directory data: the parent link, then one identifier per child.
                    let data_block = if extra_block[n] != 0 {
                        extra_block[n]
                    } else {
                        fe_block[n]
                    };
                    let mut data: Vec<u8> = Vec::new();
                    let fid = |name: &[u8], target: u32, flags: u8, data: &mut Vec<u8>| {
                        let len = (38 + name.len() + 3) & !3;
                        let mut f = vec![0u8; len];
                        w16(&mut f, 16, 1);
                        f[18] = flags;
                        f[19] = name.len() as u8;
                        w32(&mut f, 20, SEC as u32);
                        w32(&mut f, 24, target);
                        f[38..38 + name.len()].copy_from_slice(name);
                        let loc = if extra_block[n] != 0 {
                            data_block + (data.len() / SEC) as u32
                        } else {
                            data_block
                        };
                        seal_tag(&mut f, TAG_FILE_ID, loc, len - 16);
                        data.extend(f);
                    };
                    fid(&[], fe_block[node.parent], 0x0A, &mut data);
                    for &c in &node.children {
                        let child = &full.nodes[c];
                        let flags = if child.is_dir { 0x02 } else { 0 };
                        fid(&cs0(&child.name), fe_block[c], flags, &mut data);
                    }
                    debug_assert_eq!(data.len(), dir_sizes[n]);
                    size = data.len() as u64;
                    if extra_block[n] != 0 {
                        put(&mut img, part_start + extra_block[n], &data);
                        ads = self.ad(data.len() as u32, extra_block[n]);
                        recorded_blocks = sectors(data.len()) as u64;
                    } else {
                        ad_type = 3;
                        ads = data;
                    }
                } else {
                    let f = &files[node.file];
                    size = f.data.len() as u64 + f.sparse_tail;
                    if self.udf_embedded_file(f) {
                        ad_type = 3;
                        ads = f.data.clone();
                    } else {
                        let mut list: Vec<Vec<u8>> = Vec::new();
                        for (lba, len) in &alloc[node.file] {
                            list.push(self.ad(*len as u32, lba - part_start));
                            recorded_blocks += len.div_ceil(SEC as u64);
                        }
                        let mut tail = f.sparse_tail;
                        while tail > 0 {
                            let len = tail.min(MAX_AD_LEN);
                            list.push(self.ad(len as u32 | 2 << 30, 0));
                            tail -= len;
                        }
                        // Spill into allocation extent blocks when there are too many.
                        let mut rest: &[Vec<u8>] = &list;
                        let mut next_block = extra_block[n];
                        let mut target: Option<u32> = None; // None: the file entry itself
                        loop {
                            let last = rest.len() <= k;
                            let take = if last { rest.len() } else { k - 1 };
                            let mut area: Vec<u8> = rest[..take].concat();
                            if !last {
                                area.extend(self.ad(SEC as u32 | 3 << 30, next_block));
                            }
                            match target {
                                None => ads = area,
                                Some(block) => {
                                    let mut aed = vec![0u8; SEC];
                                    w32(&mut aed, 20, area.len() as u32);
                                    aed[24..24 + area.len()].copy_from_slice(&area);
                                    seal_tag(&mut aed, TAG_ALLOC_EXTENT, block, 8 + area.len());
                                    put(&mut img, part_start + block, &aed);
                                }
                            }
                            if last {
                                break;
                            }
                            rest = &rest[take..];
                            target = Some(next_block);
                            next_block += 1;
                        }
                    }
                }
                // ICB tag.
                w16(&mut fe, 20, 4);
                w16(&mut fe, 24, 1);
                fe[27] = if node.is_dir { 4 } else { 5 };
                w16(&mut fe, 34, ad_type);
                w32(&mut fe, 36, u32::MAX);
                w32(&mut fe, 40, u32::MAX);
                w32(&mut fe, 44, 0x14A5);
                w16(&mut fe, 48, 1);
                w64(&mut fe, 56, size);
                let (times, checkpoint, impl_at, unique_at): (&[usize], usize, usize, usize) =
                    if self.udf_extended {
                        w64(&mut fe, 64, size);
                        w64(&mut fe, 72, recorded_blocks);
                        (&[80, 92, 104, 116], 128, 168, 200)
                    } else {
                        w64(&mut fe, 64, recorded_blocks);
                        (&[72, 84, 96], 108, 128, 160)
                    };
                for t in times {
                    udf_time(&mut fe, *t);
                }
                w32(&mut fe, checkpoint, 1);
                fe[impl_at..impl_at + 32].copy_from_slice(&regid("*BootForge", &[]));
                w64(&mut fe, unique_at, 16 + n as u64);
                w32(&mut fe, base - 4, ads.len() as u32);
                fe[base..base + ads.len()].copy_from_slice(&ads);
                let id = if self.udf_extended {
                    TAG_EXT_FILE_ENTRY
                } else {
                    TAG_FILE_ENTRY
                };
                seal_tag(&mut fe, id, fe_block[n], base + ads.len() - 16);
                put(&mut img, part_start + fe_block[n], &fe);
            }
        }

        // ---- partition tables -------------------------------------------------------
        let total512 = total as u64 * 4;
        let efi = self
            .uefi_boot
            .as_ref()
            .filter(|_| has_boot)
            .map(|b| (uefi_lba as u64 * 4, (b.len().div_ceil(512) as u64).max(1)));
        if self.hybrid_mbr || gpt {
            let mut mbr = vec![0u8; 512];
            mbr[0..4].copy_from_slice(&[0x33, 0xED, 0xFA, 0xF4]);
            let mut entry = |slot: usize, status: u8, kind: u8, start: u64, count: u64| {
                let e = &mut mbr[446 + slot * 16..462 + slot * 16];
                e[0] = status;
                e[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
                e[4] = kind;
                e[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
                e[8..12].copy_from_slice(&(start as u32).to_le_bytes());
                e[12..16].copy_from_slice(&(count.min(u32::MAX as u64) as u32).to_le_bytes());
            };
            if gpt {
                entry(0, 0x00, 0xEE, 1, total512 - 1);
            } else {
                entry(0, 0x80, 0x17, 0, total512);
                if let Some((start, count)) = efi {
                    entry(1, 0x00, 0xEF, start, count);
                }
            }
            mbr[510] = 0x55;
            mbr[511] = 0xAA;
            img[0..512].copy_from_slice(&mbr);
        }
        if gpt {
            let mut entries = vec![0u8; 128 * 128];
            let mut add = |slot: usize, kind: [u8; 16], first: u64, last: u64, name: &str| {
                let e = &mut entries[slot * 128..(slot + 1) * 128];
                e[0..16].copy_from_slice(&kind);
                e[16..32].copy_from_slice(&[0xB0 + slot as u8; 16]);
                e[32..40].copy_from_slice(&first.to_le_bytes());
                e[40..48].copy_from_slice(&last.to_le_bytes());
                for (i, u) in name.encode_utf16().take(36).enumerate() {
                    e[56 + i * 2..58 + i * 2].copy_from_slice(&u.to_le_bytes());
                }
            };
            const BASIC_DATA: [u8; 16] = [
                0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26,
                0x99, 0xC7,
            ];
            const EFI_SYSTEM: [u8; 16] = [
                0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E,
                0xC9, 0x3B,
            ];
            let backup_entries = tail_start as u64 * 4;
            add(0, BASIC_DATA, 64, backup_entries - 1, "ISO9660");
            if let Some((start, count)) = efi {
                add(1, EFI_SYSTEM, start, start + count - 1, "EFI boot image");
            }
            let entries_crc = crc32fast::hash(&entries);
            let header = |own: u64, other: u64, table: u64| -> Vec<u8> {
                let mut h = vec![0u8; 512];
                h[0..8].copy_from_slice(b"EFI PART");
                w32(&mut h, 8, 0x0001_0000);
                w32(&mut h, 12, 92);
                w64(&mut h, 24, own);
                w64(&mut h, 32, other);
                w64(&mut h, 40, 34);
                w64(&mut h, 48, backup_entries - 1);
                h[56..72].copy_from_slice(&[0xD1; 16]);
                w64(&mut h, 72, table);
                w32(&mut h, 80, 128);
                w32(&mut h, 84, 128);
                w32(&mut h, 88, entries_crc);
                let crc = crc32fast::hash(&h[..92]);
                w32(&mut h, 16, crc);
                h
            };
            let last = total512 - 1;
            img[512..1024].copy_from_slice(&header(1, last, 2));
            img[1024..1024 + entries.len()].copy_from_slice(&entries);
            let at = backup_entries as usize * 512;
            img[at..at + entries.len()].copy_from_slice(&entries);
            let at = last as usize * 512;
            img[at..at + 512].copy_from_slice(&header(last, 1, backup_entries));
        }
        img
    }

    /// One short or long allocation descriptor (`raw_len` carries the type bits).
    fn ad(&self, raw_len: u32, block: u32) -> Vec<u8> {
        let mut a = vec![0u8; if self.udf_long_ads { 16 } else { 8 }];
        w32(&mut a, 0, raw_len);
        w32(&mut a, 4, block);
        a
    }

    /// The volume descriptor sequence, tags not yet sealed (identifier and
    /// CRC length are pre-filled for `seal_tag`).
    fn udf_sequence(&self, part_start: u32, part_len: u32) -> Vec<Vec<u8>> {
        let blank = |id: u16, crc_len: u16, vdsn: u32| {
            let mut d = vec![0u8; SEC];
            w16(&mut d, 0, id);
            w16(&mut d, 10, crc_len);
            w32(&mut d, 16, vdsn);
            d
        };
        let implementation = regid("*BootForge", &[]);

        let mut pvd = blank(TAG_PRIMARY, 496, 0);
        dstring(&mut pvd[24..56], &self.label);
        w16(&mut pvd, 56, 1);
        w16(&mut pvd, 58, 1);
        w16(&mut pvd, 60, 2);
        w16(&mut pvd, 62, 2);
        w32(&mut pvd, 64, 1);
        w32(&mut pvd, 68, 1);
        dstring(&mut pvd[72..200], "BootForge synthetic volume set");
        pvd[200..264].copy_from_slice(&charspec());
        pvd[264..328].copy_from_slice(&charspec());
        udf_time(&mut pvd, 376);
        pvd[388..420].copy_from_slice(&implementation);

        let mut iuvd = blank(TAG_IMPL_USE, 496, 1);
        iuvd[20..52].copy_from_slice(&regid("*UDF LV Info", &[0x02, 0x01]));
        iuvd[52..116].copy_from_slice(&charspec());
        dstring(&mut iuvd[116..244], &self.label);
        iuvd[352..384].copy_from_slice(&implementation);

        let mut pd = blank(TAG_PARTITION, 496, 2);
        w16(&mut pd, 20, 1);
        w16(&mut pd, 22, 0);
        pd[24..56].copy_from_slice(&regid("+NSR02", &[]));
        w32(&mut pd, 184, 1);
        w32(&mut pd, 188, part_start);
        w32(&mut pd, 192, part_len);
        pd[196..228].copy_from_slice(&implementation);

        let mut lvd = blank(TAG_LOGICAL_VOLUME, 430, 3);
        lvd[20..84].copy_from_slice(&charspec());
        dstring(&mut lvd[84..212], &self.label);
        w32(&mut lvd, 212, SEC as u32);
        lvd[216..248].copy_from_slice(&regid("*OSTA UDF Compliant", &[0x02, 0x01, 0x03]));
        w32(&mut lvd, 248, 2 * SEC as u32);
        w32(&mut lvd, 264, 6);
        w32(&mut lvd, 268, 1);
        lvd[272..304].copy_from_slice(&implementation);
        w32(&mut lvd, 432, 2 * SEC as u32);
        w32(&mut lvd, 436, UDF_LVID);
        lvd[440..446].copy_from_slice(&[1, 6, 1, 0, 0, 0]);

        let usd = blank(TAG_UNALLOCATED, 8, 4);
        let td = blank(TAG_TERMINATOR, 496, 0);
        vec![pvd, iuvd, pd, lvd, usd, td]
    }

    fn volume_descriptor(
        &self,
        joliet: bool,
        total: u32,
        path_table_len: usize,
        l_table: u32,
        m_table: u32,
    ) -> Vec<u8> {
        let mut s = vec![0u8; SEC];
        s[0] = if joliet { 2 } else { 1 };
        s[1..6].copy_from_slice(b"CD001");
        s[6] = 1;
        if joliet {
            for pair in s[8..72].as_chunks_mut::<2>().0 {
                pair.copy_from_slice(&[0, 0x20]);
            }
            let label: String = self.label.chars().take(16).collect();
            let bytes = utf16be(&label);
            let n = bytes.len().min(32);
            s[40..40 + n].copy_from_slice(&bytes[..n]);
            s[88..91].copy_from_slice(&[0x25, 0x2F, 0x45]);
        } else {
            s[8..72].fill(b' ');
            let label: Vec<u8> = self
                .label
                .chars()
                .map(|c| {
                    let u = c.to_ascii_uppercase();
                    if u.is_ascii_alphanumeric() || u == '_' {
                        u as u8
                    } else {
                        b'_'
                    }
                })
                .take(32)
                .collect();
            s[40..40 + label.len()].copy_from_slice(&label);
        }
        both32(&mut s, 80, total);
        both16(&mut s, 120, 1);
        both16(&mut s, 124, 1);
        both16(&mut s, 128, SEC as u16);
        both32(&mut s, 132, path_table_len as u32);
        w32(&mut s, 140, l_table);
        s[148..152].copy_from_slice(&m_table.to_be_bytes());
        s[190..813].fill(b' ');
        for at in [813, 830, 847, 864] {
            s[at..at + 16].copy_from_slice(if at < 847 {
                b"2024010203040500"
            } else {
                b"0000000000000000"
            });
            s[at + 16] = 0;
        }
        s[881] = 1;
        s
    }

    /// The recorded name of a node in the ISO 9660 or Joliet directory.
    fn recorded_name(&self, tree: &Tree, iso_names: &[String], joliet: bool, n: usize) -> Vec<u8> {
        let node = &tree.nodes[n];
        if joliet {
            let mut name: String = node.name.chars().take(100).collect();
            if !node.is_dir {
                name.push_str(";1");
            }
            utf16be(&name)
        } else if node.is_dir {
            iso_names[n].clone().into_bytes()
        } else {
            format!("{};1", iso_names[n]).into_bytes()
        }
    }

    fn path_table(
        &self,
        tree: &Tree,
        iso_names: &[String],
        joliet: bool,
        loc: &[(u32, u32)],
        big_endian: bool,
    ) -> Vec<u8> {
        let dirs = tree.dirs_bfs();
        let mut out = Vec::new();
        for &d in &dirs {
            let name = if d == 0 {
                vec![0u8]
            } else {
                self.recorded_name(tree, iso_names, joliet, d)
            };
            let parent = dirs
                .iter()
                .position(|x| *x == tree.nodes[d].parent)
                .unwrap_or(0) as u16
                + 1;
            out.push(name.len() as u8);
            out.push(0);
            if big_endian {
                out.extend(loc[d].0.to_be_bytes());
                out.extend(parent.to_be_bytes());
            } else {
                out.extend(loc[d].0.to_le_bytes());
                out.extend(parent.to_le_bytes());
            }
            out.extend(&name);
            if name.len() % 2 == 1 {
                out.push(0);
            }
        }
        out
    }

    /// Rock Ridge fields for one record whose ISO name is `name_len` bytes.
    fn rock_ridge_fields(
        &self,
        name: &str,
        is_dir: bool,
        name_len: usize,
        ce: &mut CePool,
    ) -> Vec<u8> {
        let base = 33 + name_len + if name_len.is_multiple_of(2) { 1 } else { 0 };
        let room = 254 - base;
        let bytes = name.as_bytes();
        let mut su = rr_px(is_dir);
        if su.len() + 5 + bytes.len() <= room {
            su.extend(rr_nm(0, bytes));
            return su;
        }
        let first = room - su.len() - 28 - 5;
        su.extend(rr_nm(1, &bytes[..first]));
        let mut area = Vec::new();
        let rest: Vec<&[u8]> = bytes[first..].chunks(200).collect();
        for (i, piece) in rest.iter().enumerate() {
            area.extend(rr_nm(if i + 1 < rest.len() { 1 } else { 0 }, piece));
        }
        let (block, offset) = ce.alloc(&area);
        let mut field = vec![0u8; 28];
        field[0..4].copy_from_slice(&[b'C', b'E', 28, 1]);
        both32(&mut field, 4, block);
        both32(&mut field, 12, offset);
        both32(&mut field, 20, area.len() as u32);
        su.extend(field);
        su
    }

    #[allow(clippy::too_many_arguments)]
    fn iso_dir(
        &self,
        tree: &Tree,
        iso_names: &[String],
        files: &[FileData],
        joliet: bool,
        dir: usize,
        loc: &[(u32, u32)],
        alloc: &[Vec<(u32, u64)>],
        ce: &mut CePool,
    ) -> Vec<u8> {
        let rr = self.rock_ridge && !joliet;
        let mut out = Vec::new();
        let mut su = Vec::new();
        if rr {
            if dir == 0 {
                su.extend([b'S', b'P', 7, 1, 0xBE, 0xEF, 0]);
            }
            su.extend(rr_px(true));
        }
        push_record(&mut out, dir_record(&[0], loc[dir].0, loc[dir].1, 2, &su));
        let parent = tree.nodes[dir].parent;
        let su = if rr { rr_px(true) } else { Vec::new() };
        push_record(
            &mut out,
            dir_record(&[1], loc[parent].0, loc[parent].1, 2, &su),
        );

        let mut children: Vec<(Vec<u8>, usize)> = tree.nodes[dir]
            .children
            .iter()
            .map(|&c| (self.recorded_name(tree, iso_names, joliet, c), c))
            .collect();
        children.sort();
        for (name, c) in children {
            let node = &tree.nodes[c];
            let fields = |ce: &mut CePool| {
                if rr {
                    self.rock_ridge_fields(&node.name, node.is_dir, name.len(), ce)
                } else {
                    Vec::new()
                }
            };
            if node.is_dir {
                let su = fields(ce);
                push_record(&mut out, dir_record(&name, loc[c].0, loc[c].1, 2, &su));
                continue;
            }
            // Measuring happens before allocation: derive the pieces from the size.
            let pieces: Vec<(u32, u64)> = if alloc[node.file].is_empty() {
                let chunk = self.chunk_len() as usize;
                files[node.file]
                    .data
                    .chunks(chunk)
                    .map(|p| (0u32, p.len() as u64))
                    .collect()
            } else {
                alloc[node.file].clone()
            };
            if pieces.is_empty() {
                let su = fields(ce);
                push_record(&mut out, dir_record(&name, 0, 0, 0, &su));
            }
            for (i, (lba, len)) in pieces.iter().enumerate() {
                let flags = if i + 1 < pieces.len() { 0x80 } else { 0 };
                let su = fields(ce);
                push_record(&mut out, dir_record(&name, *lba, *len as u32, flags, &su));
            }
        }
        out.resize(out.len().div_ceil(SEC) * SEC, 0);
        out
    }
}

/// A small FAT12 volume (512-byte sectors, one sector per cluster) holding
/// the given files; directories are created as needed. Names must be 8.3
/// and are stored upper case. Meant for El Torito EFI boot images.
pub fn fat12_image(files: &[(&str, &[u8])]) -> Vec<u8> {
    // Directory tree: index 0 is the root.
    struct Dir {
        entries: Vec<[u8; 32]>,
        cluster: u16,
        parent: usize,
        name: String,
    }
    let mut dirs = vec![Dir {
        entries: Vec::new(),
        cluster: 0,
        parent: 0,
        name: String::new(),
    }];
    let mut next_cluster = 2u16;
    let mut data: Vec<(u16, &[u8])> = Vec::new();
    let short = |name: &str| -> [u8; 11] {
        let mut out = [b' '; 11];
        let upper = name.to_ascii_uppercase();
        let (stem, ext) = upper.rsplit_once('.').unwrap_or((upper.as_str(), ""));
        for (i, b) in stem.bytes().take(8).enumerate() {
            out[i] = b;
        }
        for (i, b) in ext.bytes().take(3).enumerate() {
            out[8 + i] = b;
        }
        out
    };
    let entry = |name: [u8; 11], attr: u8, cluster: u16, size: u32| -> [u8; 32] {
        let mut e = [0u8; 32];
        e[0..11].copy_from_slice(&name);
        e[11] = attr;
        // 2024-01-02 03:04:04
        w16(&mut e, 22, 3 << 11 | 4 << 5 | 2);
        w16(&mut e, 24, 44 << 9 | 1 << 5 | 2);
        w16(&mut e, 26, cluster);
        w32(&mut e, 28, size);
        e
    };
    for (path, bytes) in files {
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        let mut at = 0usize;
        for (i, part) in parts.iter().enumerate() {
            if i + 1 == parts.len() {
                let clusters = bytes.len().div_ceil(512) as u16;
                let first = if clusters == 0 { 0 } else { next_cluster };
                dirs[at]
                    .entries
                    .push(entry(short(part), 0x20, first, bytes.len() as u32));
                if clusters > 0 {
                    data.push((first, bytes));
                    next_cluster += clusters;
                }
            } else {
                let found = (0..dirs.len()).find(|d| {
                    *d != 0 && dirs[*d].parent == at && dirs[*d].name.eq_ignore_ascii_case(part)
                });
                at = match found {
                    Some(d) => d,
                    None => {
                        let cluster = next_cluster;
                        next_cluster += 1;
                        dirs[at].entries.push(entry(short(part), 0x10, cluster, 0));
                        let parent_cluster = dirs[at].cluster;
                        let mut dot = [b' '; 11];
                        dot[0] = b'.';
                        let mut dotdot = dot;
                        dotdot[1] = b'.';
                        dirs.push(Dir {
                            entries: vec![
                                entry(dot, 0x10, cluster, 0),
                                entry(dotdot, 0x10, parent_cluster, 0),
                            ],
                            cluster,
                            parent: at,
                            name: part.to_string(),
                        });
                        dirs.len() - 1
                    }
                };
            }
        }
    }
    let used = (next_cluster - 2) as usize;
    // Round the volume up to whole 2048-byte sectors with a little slack.
    let mut clusters = used + 8;
    let mut fat_sectors;
    let mut total;
    loop {
        fat_sectors = ((clusters + 2) * 3).div_ceil(2).div_ceil(512);
        total = 1 + 2 * fat_sectors + 1 + clusters;
        if total % 4 == 0 {
            break;
        }
        clusters += 1;
    }
    assert!(clusters < 4085, "too much data for a FAT12 image");
    let mut img = vec![0u8; total * 512];
    img[0..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
    img[3..11].copy_from_slice(b"BOOTFRGE");
    w16(&mut img, 11, 512);
    img[13] = 1;
    w16(&mut img, 14, 1);
    img[16] = 2;
    w16(&mut img, 17, 16);
    w16(&mut img, 19, total as u16);
    img[21] = 0xF8;
    w16(&mut img, 22, fat_sectors as u16);
    w16(&mut img, 24, 32);
    w16(&mut img, 26, 64);
    img[36] = 0x80;
    img[38] = 0x29;
    w32(&mut img, 39, 0xB007_F0E5);
    img[43..54].copy_from_slice(b"EFI BOOT   ");
    img[54..62].copy_from_slice(b"FAT12   ");
    img[510] = 0x55;
    img[511] = 0xAA;

    // File allocation table: every chain is contiguous.
    let mut fat = vec![0u16; clusters + 2];
    fat[0] = 0xFF8;
    fat[1] = 0xFFF;
    for d in dirs.iter().skip(1) {
        fat[d.cluster as usize] = 0xFFF;
    }
    for (first, bytes) in &data {
        let n = bytes.len().div_ceil(512);
        for i in 0..n {
            let c = *first as usize + i;
            fat[c] = if i + 1 == n { 0xFFF } else { c as u16 + 1 };
        }
    }
    let mut packed = vec![0u8; fat_sectors * 512];
    for (i, v) in fat.iter().enumerate() {
        let at = i * 3 / 2;
        if i % 2 == 0 {
            packed[at] = *v as u8;
            packed[at + 1] |= (*v >> 8) as u8 & 0x0F;
        } else {
            packed[at] |= (*v << 4) as u8;
            packed[at + 1] = (*v >> 4) as u8;
        }
    }
    img[512..512 + packed.len()].copy_from_slice(&packed);
    let second = 512 + packed.len();
    img[second..second + packed.len()].copy_from_slice(&packed);

    let root_at = 512 + 2 * packed.len();
    let data_at = root_at + 512;
    let cluster_at = |c: u16| data_at + (c as usize - 2) * 512;
    for d in &dirs {
        assert!(d.entries.len() <= 16, "too many entries for one directory");
        let at = if d.cluster == 0 {
            root_at
        } else {
            cluster_at(d.cluster)
        };
        for (i, e) in d.entries.iter().enumerate() {
            img[at + i * 32..at + (i + 1) * 32].copy_from_slice(e);
        }
    }
    for (first, bytes) in &data {
        let at = cluster_at(*first);
        img[at..at + bytes.len()].copy_from_slice(bytes);
    }
    img
}

/// A header plus XML that [`super::wim::read_wim_info`] accepts: one image
/// per `(name, arch code, (major, minor, build))`. The result is
/// `total_size` bytes long (or as long as header and XML need, if that is
/// more), filled with a deterministic pattern, with the XML at the end as in
/// real WIM files.
pub fn fake_wim(images: &[(&str, u32, (u32, u32, u32))], total_size: usize) -> Vec<u8> {
    let escape = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let mut xml = String::from("<WIM><TOTALBYTES>0</TOTALBYTES>");
    for (i, (name, arch, (major, minor, build))) in images.iter().enumerate() {
        let name = escape(name);
        xml.push_str(&format!(
            "<IMAGE INDEX=\"{}\"><DIRCOUNT>1</DIRCOUNT><FILECOUNT>1</FILECOUNT><WINDOWS><ARCH>{arch}</ARCH><PRODUCTNAME>Microsoft Windows Operating System</PRODUCTNAME><EDITIONID>Edition{}</EDITIONID><VERSION><MAJOR>{major}</MAJOR><MINOR>{minor}</MINOR><BUILD>{build}</BUILD><SPBUILD>1</SPBUILD><SPLEVEL>0</SPLEVEL></VERSION></WINDOWS><NAME>{name}</NAME><DESCRIPTION>{name}</DESCRIPTION><FLAGS>Edition{}</FLAGS><DISPLAYNAME>{name}</DISPLAYNAME><DISPLAYDESCRIPTION>{name}</DISPLAYDESCRIPTION></IMAGE>",
            i + 1,
            i + 1,
            i + 1
        ));
    }
    xml.push_str("</WIM>");
    let mut xml_bytes = vec![0xFF, 0xFE];
    xml_bytes.extend(xml.encode_utf16().flat_map(|u| u.to_le_bytes()));

    let total = total_size.max(super::wim::WIM_HEADER_LEN + xml_bytes.len());
    let mut out = pattern(total, 31, 7);
    let xml_at = total - xml_bytes.len();
    out[xml_at..].copy_from_slice(&xml_bytes);
    let h = &mut out[..super::wim::WIM_HEADER_LEN];
    h.fill(0);
    h[0..8].copy_from_slice(b"MSWIM\0\0\0");
    w32(h, 8, super::wim::WIM_HEADER_LEN as u32);
    w32(h, 12, 0x0001_0D00);
    w32(h, 16, 0x0002_0002);
    w32(h, 20, 32768);
    h[24..40].copy_from_slice(&[0xA7; 16]);
    w16(h, 40, 1);
    w16(h, 42, 1);
    w32(h, 44, images.len() as u32);
    // XML data resource entry: stored size (7 bytes) + flags, offset, original size.
    w64(h, 72, xml_bytes.len() as u64);
    w64(h, 80, xml_at as u64);
    w64(h, 88, xml_bytes.len() as u64);
    out
}

/// A minimal PE file (512 bytes) whose machine field names `arch`, as an
/// EFI application (subsystem 10).
pub fn fake_efi_binary(arch: Arch) -> Vec<u8> {
    let (machine, plus) = match arch {
        Arch::X86 => (0x014Cu16, false),
        Arch::X64 => (0x8664, true),
        Arch::Arm => (0x01C2, false),
        Arch::Arm64 => (0xAA64, true),
        Arch::Ia64 => (0x0200, true),
        Arch::RiscV64 => (0x5064, true),
    };
    let mut b = vec![0u8; 512];
    b[0..2].copy_from_slice(b"MZ");
    w32(&mut b, 0x3C, 0x40);
    b[0x40..0x44].copy_from_slice(b"PE\0\0");
    w16(&mut b, 0x44, machine);
    w16(&mut b, 0x54, if plus { 240 } else { 224 }); // size of optional header
    w16(&mut b, 0x56, 0x0022);
    w16(&mut b, 0x58, if plus { 0x020B } else { 0x010B });
    let subsystem_at = 0x58 + 68;
    w16(&mut b, subsystem_at, 10);
    b
}

/// A Linux live image in miniature: ISO 9660 + Joliet + Rock Ridge, BIOS and
/// UEFI El Torito entries, isohybrid MBR, `EFI/BOOT/BOOTX64.EFI`,
/// `boot/grub/grub.cfg`, `casper/vmlinuz` and friends (about 1 MB).
pub fn sample_linux_hybrid() -> Vec<u8> {
    let efi = fake_efi_binary(Arch::X64);
    let esp = fat12_image(&[("EFI/BOOT/BOOTX64.EFI", &efi)]);
    IsoBuilder::new("BF_LINUX_LIVE")
        .joliet(true)
        .rock_ridge(true)
        .bios_boot(pattern(2048, 3, 0xEB))
        .uefi_boot(esp)
        .hybrid_mbr(true)
        .file(".disk/info", b"BootForge Sample Linux 24.04 (synthetic)\n".to_vec())
        .file("EFI/BOOT/BOOTX64.EFI", efi)
        .file("EFI/BOOT/grubx64.efi", pattern(9000, 5, 1))
        .file(
            "boot/grub/grub.cfg",
            b"menuentry \"Try the sample\" {\n  linux /casper/vmlinuz quiet\n  initrd /casper/initrd\n}\n".to_vec(),
        )
        .file("isolinux/isolinux.bin", pattern(4096, 7, 2))
        .file("isolinux/isolinux.cfg", b"default live\n".to_vec())
        .file("casper/vmlinuz", pattern(300_000, 11, 3))
        .file("casper/initrd", pattern(200_000, 13, 4))
        .file("casper/filesystem.squashfs", pattern(400_000, 17, 5))
        .file("casper/filesystem.manifest-with-a-rather-long-name.txt", pattern(700, 19, 6))
        .file("md5sum.txt", b"synthetic\n".to_vec())
        .build()
}

/// A Windows setup image in miniature: UDF bridge in the Windows style (the
/// ISO 9660 tree holds only a readme), BIOS and UEFI El Torito entries,
/// `bootmgr`, `bootmgr.efi`, `efi/boot/bootx64.efi`, `sources/boot.wim` and
/// a `sources/install.wim` of `wim_bytes` bytes with two editions.
pub fn sample_windows_setup(wim_bytes: usize) -> Vec<u8> {
    sample_windows_setup_sparse(wim_bytes, 0)
}

/// Like [`sample_windows_setup`], with `sparse_tail` extra unrecorded bytes
/// at the end of `sources/install.wim`: a 5 GiB install image can be
/// simulated in a few hundred kilobytes (the tail reads as zeros).
pub fn sample_windows_setup_sparse(wim_bytes: usize, sparse_tail: u64) -> Vec<u8> {
    let efi = fake_efi_binary(Arch::X64);
    let esp = fat12_image(&[("EFI/BOOT/BOOTX64.EFI", &efi)]);
    let install = fake_wim(
        &[
            ("Windows 11 Home", 9, (10, 0, 26100)),
            ("Windows 11 Pro", 9, (10, 0, 26100)),
        ],
        wim_bytes,
    );
    let install_len = install.len() as u64;
    let boot = fake_wim(
        &[("Microsoft Windows Setup (amd64)", 9, (10, 0, 26100))],
        60_000,
    );
    let builder = IsoBuilder::new("CCCOMA_X64FRE_EN-US_DV9")
        .windows_style(true)
        .bios_boot(pattern(4096, 3, 0xFA))
        .uefi_boot(esp)
        .file(
            "autorun.inf",
            b"[AutoRun.Amd64]\r\nopen=setup.exe\r\n".to_vec(),
        )
        .file("bootmgr", pattern(20_000, 5, 1))
        .file("bootmgr.efi", fake_efi_binary(Arch::X64))
        .file("setup.exe", pattern(30_000, 7, 2))
        .file("boot/bcd", pattern(8192, 9, 3))
        .file("boot/boot.sdi", pattern(40_000, 11, 4))
        .file("boot/etfsboot.com", pattern(4096, 3, 0xFA))
        .file("efi/boot/bootx64.efi", efi)
        .file("efi/microsoft/boot/bcd", pattern(8192, 13, 5))
        .file("sources/boot.wim", boot);
    let builder = if sparse_tail > 0 {
        builder.sparse_file("sources/install.wim", install, install_len + sparse_tail)
    } else {
        builder.file("sources/install.wim", install)
    };
    builder
        .file("sources/setup.exe", pattern(10_000, 15, 6))
        .file("support/logging/readme.txt", b"synthetic\r\n".to_vec())
        .build()
}
