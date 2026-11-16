//! exFAT formatter.
//!
//! Creates an empty exFAT volume (used when a drive is restored to a normal,
//! empty state) and parses one back for verification. Written from the
//! published exFAT file system specification.
//!
//! Honesty note: the output of this formatter has been validated only by the
//! parser in this file (`check`), which was written from the same reading of
//! the specification. It has not been validated by mounting the result in
//! Windows or running `chkdsk` on it. Two deliberate deviations from what
//! Windows' own formatter writes are documented at `upcase_table` (the table
//! is generated from Rust's Unicode data instead of being the specification's
//! recommended table) and at `format` (no volume GUID entry, zeroed OEM
//! parameters).

use crate::blockdev::BlockDevice;
use crate::error::{Error, IoContext, Result};
use crate::model::{Cancel, ProgressFn};

const MIB: u64 = 1024 * 1024;
const CHUNK: usize = 1024 * 1024;
/// Sectors in one boot region (main: 0..12, backup: 12..24).
const BOOT_REGION_SECTORS: u64 = 12;
const FAT_EOC: u32 = 0xFFFF_FFFF;
const MAX_CLUSTER_COUNT: u64 = 0xFFFF_FFF5;
/// Largest cluster the specification allows (sector shift + cluster shift <= 25).
const MAX_CLUSTER_SIZE: u32 = 32 * 1024 * 1024;
const MAX_UPCASE_BYTES: u64 = 4 * 65536;
const MAX_PROBLEMS: usize = 100;

const ENTRY_BITMAP: u8 = 0x81;
const ENTRY_UPCASE: u8 = 0x82;
const ENTRY_LABEL: u8 = 0x83;
const ENTRY_VOLUME_GUID: u8 = 0xA0;

/// A tiny original boot stub: `int 18h`, then `hlt` in a loop. These volumes
/// are data volumes; nothing boots from this sector.
const BOOT_STUB: [u8; 5] = [0xCD, 0x18, 0xF4, 0xEB, 0xFD];

/// What to format.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExFatOptions {
    /// Volume label; passed through `sanitize_label` (11 UTF-16 units at most).
    pub label: String,
    /// Cluster size in bytes; `None` picks `default_cluster_size`.
    pub cluster_size: Option<u32>,
    /// Start of the partition on the disk in sectors (the PartitionOffset
    /// field; also used to align the FAT and the cluster heap to 1 MiB on the
    /// disk rather than within the partition).
    pub partition_offset_sectors: u64,
    /// Volume serial number.
    pub volume_serial: u32,
}

/// Where everything was put.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExFatGeometry {
    pub sector_size: u32,
    pub cluster_size: u32,
    /// VolumeLength, in sectors.
    pub volume_sectors: u64,
    /// FatOffset, in sectors from the start of the volume.
    pub fat_offset: u32,
    /// FatLength, in sectors.
    pub fat_length: u32,
    /// ClusterHeapOffset, in sectors.
    pub cluster_heap_offset: u32,
    pub cluster_count: u32,
    pub bitmap_cluster: u32,
    /// Size of the allocation bitmap in bytes.
    pub bitmap_bytes: u64,
    pub upcase_cluster: u32,
    /// Size of the up-case table in bytes.
    pub upcase_bytes: u64,
    pub upcase_checksum: u32,
    pub root_cluster: u32,
    /// Clusters taken by the bitmap, the up-case table and the root directory.
    pub used_clusters: u32,
    pub volume_serial: u32,
    /// The label as stored.
    pub label: String,
}

/// What `check` found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExFatReport {
    pub label: String,
    pub cluster_size: u32,
    pub cluster_count: u32,
    /// Bits set in the allocation bitmap.
    pub used_clusters: u32,
    pub percent_in_use: u8,
    pub volume_serial: u32,
    /// Every inconsistency found; empty for a healthy, freshly formatted volume.
    pub problems: Vec<String>,
}

// --------------------------------------------------------------------------------------
// Helpers
// --------------------------------------------------------------------------------------

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn le64(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}
fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

fn problem(list: &mut Vec<String>, text: String) {
    if list.len() < MAX_PROBLEMS {
        list.push(text);
    } else if list.len() == MAX_PROBLEMS {
        list.push("(further problems not listed)".to_string());
    }
}

fn sector_of(dev: &dyn BlockDevice) -> Result<u32> {
    let s = dev.sector_size();
    if !(512..=4096).contains(&s) || !s.is_power_of_two() {
        return Err(Error::invalid(format!(
            "Unsupported sector size ({s} bytes)."
        )));
    }
    Ok(s)
}

/// One step of the rotate-right-and-add checksum the specification uses for
/// the boot region and the up-case table.
fn checksum_step(sum: u32, byte: u8) -> u32 {
    sum.rotate_right(1).wrapping_add(byte as u32)
}

/// Boot checksum over the first eleven sectors of a boot region, skipping
/// VolumeFlags (bytes 106 and 107) and PercentInUse (byte 112).
fn boot_checksum(eleven_sectors: &[u8]) -> u32 {
    let mut sum = 0u32;
    for (i, b) in eleven_sectors.iter().enumerate() {
        if i == 106 || i == 107 || i == 112 {
            continue;
        }
        sum = checksum_step(sum, *b);
    }
    sum
}

fn table_checksum(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |sum, b| checksum_step(sum, *b))
}

/// The upper-case mapping stored in the up-case table.
fn upcase_of(c: u32) -> u16 {
    if c < 128 {
        return (c as u8).to_ascii_uppercase() as u16;
    }
    let Some(ch) = char::from_u32(c) else {
        return c as u16; // surrogates
    };
    let mut up = ch.to_uppercase();
    match (up.next(), up.next()) {
        // Only simple one-to-one mappings that stay in the BMP and do not
        // fold a non-ASCII character into ASCII.
        (Some(u), None) if (128..=0xFFFF).contains(&(u as u32)) => u as u32 as u16,
        _ => c as u16,
    }
}

/// The up-case table in the specification's compressed form: a sequence of
/// little-endian UTF-16 units where `0xFFFF, n` stands for "the next `n`
/// characters map to themselves".
///
/// The first 128 entries are the mandatory ASCII mappings. Above that the
/// table holds the simple one-to-one upper-case mappings of Rust's
/// `char::to_uppercase` (one BMP character to one non-ASCII BMP character).
/// This is not byte-identical to the "recommended up-case table" printed in
/// the specification, which Windows' formatter writes; implementations are
/// required to use whatever table is on the volume, but this difference has
/// not been exercised against Windows.
pub fn upcase_table() -> Vec<u8> {
    let mut units: Vec<u16> = Vec::new();
    let mut i = 0u32;
    while i < 0x1_0000 {
        if upcase_of(i) as u32 == i {
            let mut j = i;
            while j < 0x1_0000 && upcase_of(j) as u32 == j {
                j += 1;
            }
            let run = j - i;
            // A lone identity mapping is cheaper written out, except for
            // 0xFFFF itself, which would read as a run marker.
            if run >= 2 || i == 0xFFFF {
                // `run` is below 65536: 'a' never maps to itself.
                units.push(0xFFFF);
                units.push(run as u16);
            } else {
                units.push(i as u16);
            }
            i = j;
        } else {
            units.push(upcase_of(i));
            i += 1;
        }
    }
    units.iter().flat_map(|u| u.to_le_bytes()).collect()
}

/// Expands a compressed (or plain) up-case table into at most 65536 mappings.
/// `None` when the table describes more than 65536 characters or ends in the
/// middle of a run.
fn expand_upcase(table: &[u8]) -> Option<Vec<u16>> {
    let mut map: Vec<u16> = Vec::new();
    let mut units = table
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c));
    while let Some(u) = units.next() {
        if u == 0xFFFF {
            let run = units.next()? as usize;
            if map.len() + run > 0x1_0000 {
                return None;
            }
            let start = map.len();
            map.extend((start..start + run).map(|c| c as u16));
        } else {
            if map.len() >= 0x1_0000 {
                return None;
            }
            map.push(u);
        }
    }
    Some(map)
}

/// The label as it will be stored: control characters and the characters
/// Windows refuses in labels (`" * / : < > ? \ |`) removed, surrounding
/// spaces trimmed, cut to 11 UTF-16 units (never in the middle of a
/// surrogate pair). Case is preserved.
pub fn sanitize_label(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .filter(|c| !c.is_control() && !"\"*/:<>?\\|".contains(*c))
        .collect();
    let mut out = String::new();
    let mut units = 0;
    for c in cleaned.trim().chars() {
        if units + c.len_utf16() > 11 {
            break;
        }
        units += c.len_utf16();
        out.push(c);
    }
    out.trim_end().to_string()
}

/// The usual default: 4 KiB up to 256 MiB, 32 KiB up to 32 GiB, 128 KiB
/// above; never smaller than the sector.
pub fn default_cluster_size(volume_size: u64, sector: u32) -> u32 {
    let c = if volume_size <= 256 * MIB {
        4096
    } else if volume_size <= 32 * 1024 * MIB {
        32 * 1024
    } else {
        128 * 1024
    };
    c.max(sector)
}

struct Layout {
    sector: u32,
    cluster: u32,
    volume_sectors: u64,
    fat_offset: u32,
    fat_length: u32,
    heap_offset: u32,
    cluster_count: u32,
}

impl Layout {
    fn cluster_offset(&self, cluster: u32) -> u64 {
        self.heap_offset as u64 * self.sector as u64 + (cluster as u64 - 2) * self.cluster as u64
    }
}

fn round_up(v: u64, to: u64) -> u64 {
    v.div_ceil(to) * to
}

fn plan(volume_bytes: u64, sector: u32, cluster: u32, partition_offset: u64) -> Result<Layout> {
    let s = sector as u64;
    let cs = cluster as u64;
    let too_small = || Error::invalid("The volume is too small for exFAT with this cluster size.");
    if !cluster.is_power_of_two() || cluster < sector || cluster > MAX_CLUSTER_SIZE {
        return Err(Error::invalid(format!(
            "Invalid exFAT cluster size ({cluster} bytes): it must be a power of two between the sector size and 32 MiB."
        )));
    }
    if volume_bytes < MIB {
        return Err(Error::invalid(
            "The volume is too small for exFAT (1 MiB at least).",
        ));
    }
    // FAT and cluster heap start on 1 MiB boundaries of the disk; tiny
    // volumes align to the cluster instead so that something is left.
    let boundary = if volume_bytes >= 64 * MIB {
        MIB
    } else {
        cs.min(MIB)
    };
    let rel = (partition_offset % (boundary / s)) * s;
    let fat_off = round_up(rel + 2 * BOOT_REGION_SECTORS * s, boundary) - rel;
    let upper = volume_bytes.checked_sub(fat_off).ok_or_else(too_small)? / cs;
    let fat_len = round_up((upper + 2) * 4, s);
    let heap_off = round_up(rel + fat_off + fat_len, boundary) - rel;
    let count = volume_bytes.checked_sub(heap_off).ok_or_else(too_small)? / cs;
    if count == 0 {
        return Err(too_small());
    }
    if count > MAX_CLUSTER_COUNT || heap_off / s > u32::MAX as u64 {
        return Err(Error::invalid(
            "The volume is too large for exFAT with this cluster size; choose a larger cluster size.",
        ));
    }
    Ok(Layout {
        sector,
        cluster,
        volume_sectors: volume_bytes / s,
        fat_offset: (fat_off / s) as u32,
        fat_length: (fat_len / s) as u32,
        heap_offset: (heap_off / s) as u32,
        cluster_count: count as u32,
    })
}

/// Sets, in `out` (which starts at byte `first_byte` of a bitmap), the bits
/// covered by the half-open bit ranges.
fn fill_bits(ranges: &[(u64, u64)], first_byte: u64, out: &mut [u8]) {
    out.fill(0);
    let lo = first_byte * 8;
    let hi = lo + out.len() as u64 * 8;
    for &(a, b) in ranges {
        for bit in a.max(lo)..b.min(hi) {
            out[(bit / 8 - first_byte) as usize] |= 1 << (bit % 8);
        }
    }
}

fn boot_region(lay: &Layout, root_cluster: u32, opts: &ExFatOptions, percent: u8) -> Vec<u8> {
    let s = lay.sector as usize;
    let mut r = vec![0u8; BOOT_REGION_SECTORS as usize * s];
    // Main boot sector.
    r[0..3].copy_from_slice(&[0xEB, 0x76, 0x90]);
    r[3..11].copy_from_slice(b"EXFAT   ");
    // 11..64 must be zero (where a FAT BPB would be).
    put64(&mut r, 64, opts.partition_offset_sectors);
    put64(&mut r, 72, lay.volume_sectors);
    put32(&mut r, 80, lay.fat_offset);
    put32(&mut r, 84, lay.fat_length);
    put32(&mut r, 88, lay.heap_offset);
    put32(&mut r, 92, lay.cluster_count);
    put32(&mut r, 96, root_cluster);
    put32(&mut r, 100, opts.volume_serial);
    r[104] = 0x00; // FileSystemRevision 1.00
    r[105] = 0x01;
    // 106: VolumeFlags = 0 (first FAT active, clean, no failures).
    r[108] = lay.sector.trailing_zeros() as u8;
    r[109] = (lay.cluster / lay.sector).trailing_zeros() as u8;
    r[110] = 1; // NumberOfFats
    r[111] = 0x80; // DriveSelect
    r[112] = percent;
    r[120..125].copy_from_slice(&BOOT_STUB);
    r[510] = 0x55;
    r[511] = 0xAA;
    // Eight extended boot sectors, each ending in 0xAA550000.
    for i in 1..=8 {
        r[(i + 1) * s - 4..(i + 1) * s].copy_from_slice(&[0x00, 0x00, 0x55, 0xAA]);
    }
    // Sector 9: OEM parameters (none), sector 10: reserved. Sector 11: checksum.
    let sum = boot_checksum(&r[..11 * s]).to_le_bytes();
    for chunk in r[11 * s..].as_chunks_mut::<4>().0 {
        *chunk = sum;
    }
    r
}

/// Writes `data` padded with zeros to a whole number of sectors.
fn write_padded(dev: &mut dyn BlockDevice, offset: u64, data: &[u8], what: &str) -> Result<()> {
    let s = dev.sector_size() as usize;
    let mut buf = data.to_vec();
    buf.resize(data.len().div_ceil(s) * s, 0);
    dev.write_at(offset, &buf).ctx(what)
}

/// Formats the device (a partition window) as an empty exFAT volume.
///
/// Layout, per the exFAT specification: main boot region in sectors 0..12
/// (boot sector, eight extended boot sectors, OEM parameters, a reserved
/// sector, the boot checksum sector) and its backup in sectors 12..24; one
/// FAT; the cluster heap holding the allocation bitmap (cluster 2), the
/// up-case table and a one-cluster root directory with the volume label,
/// bitmap and up-case entries. FAT and cluster heap start on 1 MiB boundaries
/// of the disk (on cluster boundaries for volumes under 64 MiB).
/// VolumeFlags is 0 and PercentInUse is the real value. Works with 512- and
/// 4096-byte sectors.
///
/// Not written: a volume GUID entry (optional) and OEM parameters (the sector
/// is zero, i.e. no parameters). Everything before the first free cluster is
/// overwritten; free clusters are not touched.
///
/// `progress` receives the bytes written so far, ending at the size of the
/// metadata area. The boot regions are written last, so a cancelled or failed
/// format does not leave something that looks like a valid volume.
///
/// This formatter is validated only by `check` in this module, not by Windows.
pub fn format(
    dev: &mut dyn BlockDevice,
    opts: &ExFatOptions,
    cancel: &Cancel,
    progress: ProgressFn,
) -> Result<ExFatGeometry> {
    let sector = sector_of(dev)?;
    let s = sector as u64;
    let size = dev.size();
    let cluster = opts
        .cluster_size
        .unwrap_or_else(|| default_cluster_size(size, sector));
    let lay = plan(size, sector, cluster, opts.partition_offset_sectors)?;
    let cs = cluster as u64;
    let label = sanitize_label(&opts.label);
    let upcase = upcase_table();
    let upcase_sum = table_checksum(&upcase);

    let bitmap_bytes = (lay.cluster_count as u64).div_ceil(8);
    let bitmap_clusters = bitmap_bytes.div_ceil(cs);
    let upcase_clusters = (upcase.len() as u64).div_ceil(cs);
    let used = bitmap_clusters + upcase_clusters + 1;
    if used > lay.cluster_count as u64 {
        return Err(Error::invalid(
            "The volume is too small for exFAT with this cluster size.",
        ));
    }
    let bitmap_cluster = 2u32;
    let upcase_cluster = bitmap_cluster + bitmap_clusters as u32;
    let root_cluster = upcase_cluster + upcase_clusters as u32;
    let percent = (used * 100 / lay.cluster_count as u64) as u8;

    // 1. Zero everything up to the first free cluster.
    let meta_end = lay.cluster_offset(2) + used * cs;
    let zeros = vec![0u8; CHUNK];
    let mut done = 0u64;
    while done < meta_end {
        cancel.check()?;
        let n = (meta_end - done).min(CHUNK as u64) as usize;
        dev.write_at(done, &zeros[..n])
            .ctx("clearing the exFAT metadata area")?;
        done += n as u64;
        progress(done);
    }
    cancel.check()?;

    // 2. FAT: media descriptor, reserved entry, then the three chains.
    let mut fat: Vec<u8> = Vec::with_capacity((used as usize + 2) * 4);
    fat.extend_from_slice(&0xFFFF_FFF8u32.to_le_bytes());
    fat.extend_from_slice(&FAT_EOC.to_le_bytes());
    for (first, len) in [
        (bitmap_cluster, bitmap_clusters),
        (upcase_cluster, upcase_clusters),
        (root_cluster, 1),
    ] {
        for i in 0..len {
            let next = if i + 1 < len {
                first + i as u32 + 1
            } else {
                FAT_EOC
            };
            fat.extend_from_slice(&next.to_le_bytes());
        }
    }
    write_padded(
        dev,
        lay.fat_offset as u64 * s,
        &fat,
        "writing the exFAT allocation table",
    )?;

    // 3. Allocation bitmap: the first `used` clusters are taken.
    let mut bitmap = vec![0u8; used.div_ceil(8) as usize];
    fill_bits(&[(0, used)], 0, &mut bitmap);
    write_padded(
        dev,
        lay.cluster_offset(bitmap_cluster),
        &bitmap,
        "writing the exFAT allocation bitmap",
    )?;

    // 4. Up-case table.
    write_padded(
        dev,
        lay.cluster_offset(upcase_cluster),
        &upcase,
        "writing the exFAT up-case table",
    )?;

    // 5. Root directory: label, bitmap, up-case table.
    let mut root = vec![0u8; 96];
    let units: Vec<u16> = label.encode_utf16().collect();
    if units.is_empty() {
        root[0] = ENTRY_LABEL & 0x7F; // a label entry that is not in use
    } else {
        root[0] = ENTRY_LABEL;
        root[1] = units.len() as u8;
        for (i, u) in units.iter().enumerate() {
            root[2 + i * 2..4 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
    }
    root[32] = ENTRY_BITMAP;
    put32(&mut root, 32 + 20, bitmap_cluster);
    put64(&mut root, 32 + 24, bitmap_bytes);
    root[64] = ENTRY_UPCASE;
    put32(&mut root, 64 + 4, upcase_sum);
    put32(&mut root, 64 + 20, upcase_cluster);
    put64(&mut root, 64 + 24, upcase.len() as u64);
    write_padded(
        dev,
        lay.cluster_offset(root_cluster),
        &root,
        "writing the exFAT root directory",
    )?;

    // 6. Boot regions, backup first.
    cancel.check()?;
    let region = boot_region(&lay, root_cluster, opts, percent);
    dev.write_at(BOOT_REGION_SECTORS * s, &region)
        .ctx("writing the exFAT backup boot region")?;
    dev.write_at(0, &region)
        .ctx("writing the exFAT boot region")?;
    dev.flush().ctx("flushing the exFAT volume")?;
    progress(meta_end);

    Ok(ExFatGeometry {
        sector_size: sector,
        cluster_size: cluster,
        volume_sectors: lay.volume_sectors,
        fat_offset: lay.fat_offset,
        fat_length: lay.fat_length,
        cluster_heap_offset: lay.heap_offset,
        cluster_count: lay.cluster_count,
        bitmap_cluster,
        bitmap_bytes,
        upcase_cluster,
        upcase_bytes: upcase.len() as u64,
        upcase_checksum: upcase_sum,
        root_cluster,
        used_clusters: used as u32,
        volume_serial: opts.volume_serial,
        label,
    })
}

// --------------------------------------------------------------------------------------
// Checker
// --------------------------------------------------------------------------------------

/// Reads FAT entries one sector at a time.
struct FatReader {
    cached: u64,
    data: Vec<u8>,
}

impl FatReader {
    fn entry(&mut self, dev: &mut dyn BlockDevice, lay: &Layout, index: u32) -> Result<u32> {
        let s = lay.sector as u64;
        let byte = index as u64 * 4;
        let sector = lay.fat_offset as u64 + byte / s;
        if self.cached != sector {
            self.data.resize(s as usize, 0);
            dev.read_at(sector * s, &mut self.data)
                .ctx("reading the exFAT allocation table")?;
            self.cached = sector;
        }
        Ok(le32(&self.data, (byte % s) as usize))
    }
}

/// A cluster chain as extents `(first cluster, length)`.
struct Chain {
    extents: Vec<(u32, u32)>,
    clusters: u64,
}

/// Follows a chain for at most `limit` clusters. A chain that leaves the
/// volume, or does not end within `limit` clusters (which also catches
/// loops), is reported in `problems` and returned as far as it was valid.
fn follow(
    dev: &mut dyn BlockDevice,
    fat: &mut FatReader,
    lay: &Layout,
    first: u32,
    limit: u64,
    what: &str,
    problems: &mut Vec<String>,
) -> Result<Chain> {
    let mut chain = Chain {
        extents: Vec::new(),
        clusters: 0,
    };
    let mut cur = first;
    loop {
        if cur < 2 || cur as u64 >= lay.cluster_count as u64 + 2 {
            problem(
                problems,
                format!("{what}: the cluster chain points outside the volume (cluster {cur})."),
            );
            break;
        }
        if chain.clusters >= limit {
            problem(
                problems,
                format!("{what}: the cluster chain is longer than {limit} clusters or loops."),
            );
            break;
        }
        match chain.extents.last_mut() {
            Some((start, len)) if *start as u64 + *len as u64 == cur as u64 => *len += 1,
            _ => chain.extents.push((cur, 1)),
        }
        chain.clusters += 1;
        let next = fat.entry(dev, lay, cur)?;
        if next == FAT_EOC {
            break;
        }
        cur = next;
    }
    Ok(chain)
}

/// Calls `visit(position, bytes)` for the first `len` bytes stored in a
/// chain, in pieces of at most 1 MiB.
fn read_chain(
    dev: &mut dyn BlockDevice,
    lay: &Layout,
    chain: &Chain,
    len: u64,
    visit: &mut dyn FnMut(u64, &[u8]),
) -> Result<()> {
    let s = lay.sector as u64;
    let mut buf = vec![0u8; CHUNK.min(round_up(len.max(1), s) as usize)];
    let mut pos = 0u64;
    for &(first, count) in &chain.extents {
        let mut offset = lay.cluster_offset(first);
        let mut left = count as u64 * lay.cluster as u64;
        while left > 0 && pos < len {
            let want = left.min(len - pos).min(buf.len() as u64);
            let n = round_up(want, s) as usize;
            dev.read_at(offset, &mut buf[..n])
                .ctx("reading the exFAT cluster heap")?;
            visit(pos, &buf[..want as usize]);
            pos += want;
            offset += n as u64;
            left -= left.min(n as u64);
        }
    }
    Ok(())
}

/// Parses an exFAT volume back and reports every inconsistency.
///
/// Recomputes both boot checksums and compares the backup boot region with
/// the main one, checks the boot-sector fields against each other and the
/// device, the extended boot signatures, the first FAT entries, the FAT
/// chains of the allocation bitmap, the up-case table and the root directory,
/// the up-case table checksum and its mandatory ASCII part, and that the
/// allocation bitmap marks exactly the clusters of those three chains and
/// nothing else. It is meant for freshly formatted volumes: files or
/// directories in the root are reported as problems.
///
/// `Err` is returned for I/O errors and when the boot sector is not exFAT or
/// too damaged to locate anything; everything else lands in `problems`.
/// Never panics on garbage.
pub fn check(dev: &mut dyn BlockDevice) -> Result<ExFatReport> {
    let sector = sector_of(dev)?;
    let s = sector as usize;
    let dev_sectors = dev.size() / sector as u64;
    if dev_sectors < 2 * BOOT_REGION_SECTORS {
        return Err(Error::invalid(
            "The device is too small to hold an exFAT volume.",
        ));
    }
    let mut boot = vec![0u8; 2 * BOOT_REGION_SECTORS as usize * s];
    dev.read_at(0, &mut boot)
        .ctx("reading the exFAT boot region")?;
    let (main, backup) = boot.split_at(BOOT_REGION_SECTORS as usize * s);
    if &main[3..11] != b"EXFAT   " {
        return Err(Error::invalid("This is not an exFAT volume."));
    }
    let bad = |what: &str| Error::invalid(format!("The exFAT boot sector is damaged: {what}."));

    let sector_shift = main[108];
    let cluster_shift = main[109];
    if !(9..=12).contains(&sector_shift) || 1u32 << sector_shift != sector {
        return Err(bad("the sector size does not match the device"));
    }
    if cluster_shift > 25 - sector_shift {
        return Err(bad("invalid cluster size"));
    }
    let lay = Layout {
        sector,
        cluster: sector << cluster_shift,
        volume_sectors: le64(main, 72),
        fat_offset: le32(main, 80),
        fat_length: le32(main, 84),
        heap_offset: le32(main, 88),
        cluster_count: le32(main, 92),
    };
    let root_cluster = le32(main, 96);
    let count = lay.cluster_count as u64;
    if lay.volume_sectors > dev_sectors {
        return Err(bad("the volume is larger than the device (truncated?)"));
    }
    if (lay.fat_offset as u64) < 2 * BOOT_REGION_SECTORS
        || lay.fat_length == 0
        || lay.fat_offset as u64 + lay.fat_length as u64 > lay.heap_offset as u64
        || lay.heap_offset as u64 >= lay.volume_sectors
    {
        return Err(bad("the FAT or the cluster heap is misplaced"));
    }
    if count == 0
        || count > MAX_CLUSTER_COUNT
        || lay.heap_offset as u64 + (count << cluster_shift) > lay.volume_sectors
    {
        return Err(bad("the cluster count does not fit the volume"));
    }
    if (lay.fat_length as u64 * sector as u64) / 4 < count + 2 {
        return Err(bad("the FAT is too short for the cluster count"));
    }
    if root_cluster < 2 || root_cluster as u64 >= count + 2 {
        return Err(bad("the root directory cluster is out of range"));
    }

    let mut problems: Vec<String> = Vec::new();
    let p = &mut problems;
    if main[0..3] != [0xEB, 0x76, 0x90] {
        problem(p, "Boot sector: unexpected jump instruction.".to_string());
    }
    if main[11..64].iter().any(|b| *b != 0) {
        problem(
            p,
            "Boot sector: the must-be-zero area is not zero.".to_string(),
        );
    }
    if main[510] != 0x55 || main[511] != 0xAA {
        problem(p, "Boot sector: boot signature 0x55AA missing.".to_string());
    }
    if le16(main, 104) != 0x0100 {
        problem(
            p,
            format!(
                "Boot sector: unexpected file system revision 0x{:04X}.",
                le16(main, 104)
            ),
        );
    }
    if main[110] != 1 {
        problem(p, format!("Boot sector: {} FATs (expected 1).", main[110]));
    }
    let flags = le16(main, 106);
    if flags & 0x0006 != 0 {
        problem(
            p,
            format!("Boot sector: the volume is marked dirty or failed (flags 0x{flags:04X})."),
        );
    }
    if lay.volume_sectors < MIB / sector as u64 {
        problem(
            p,
            "Boot sector: the volume is shorter than 1 MiB.".to_string(),
        );
    }
    for i in 1..=8usize {
        if main[(i + 1) * s - 4..(i + 1) * s] != [0x00, 0x00, 0x55, 0xAA] {
            problem(p, format!("Extended boot sector {i}: signature missing."));
        }
    }
    for (name, region) in [("Main", main), ("Backup", backup)] {
        let sum = boot_checksum(&region[..11 * s]).to_le_bytes();
        if region[11 * s..]
            .as_chunks::<4>()
            .0
            .iter()
            .any(|c| *c != sum)
        {
            problem(p, format!("{name} boot region: checksum mismatch."));
        }
    }
    let same = main[..11 * s]
        .iter()
        .zip(&backup[..11 * s])
        .enumerate()
        .all(|(i, (a, b))| a == b || i == 106 || i == 107 || i == 112);
    if !same {
        problem(
            p,
            "The backup boot region differs from the main one.".to_string(),
        );
    }

    // FAT.
    let mut fat = FatReader {
        cached: u64::MAX,
        data: Vec::new(),
    };
    let (fat0, fat1) = (fat.entry(dev, &lay, 0)?, fat.entry(dev, &lay, 1)?);
    if fat0 != 0xFFFF_FFF8 || fat1 != FAT_EOC {
        problem(
            p,
            format!("FAT: unexpected first entries (0x{fat0:08X}, 0x{fat1:08X})."),
        );
    }

    // Root directory. Only its beginning is parsed: a fresh volume has three entries.
    let cs = lay.cluster as u64;
    let root_limit = (8 * MIB / cs).max(1).min(count);
    let root = follow(
        dev,
        &mut fat,
        &lay,
        root_cluster,
        root_limit,
        "Root directory",
        p,
    )?;
    let mut root_data: Vec<u8> = Vec::new();
    read_chain(dev, &lay, &root, cs.min(MIB), &mut |_, bytes| {
        root_data.extend_from_slice(bytes)
    })?;
    let mut label = String::new();
    let mut bitmap_entry: Option<(u32, u64)> = None;
    let mut upcase_entry: Option<(u32, u64, u32)> = None;
    let mut foreign = false;
    for e in root_data.as_chunks::<32>().0 {
        match e[0] {
            0x00 => break,
            ENTRY_LABEL => {
                let n = e[1] as usize;
                if n > 11 {
                    problem(p, format!("Root directory: label length {n} is invalid."));
                } else {
                    let units: Vec<u16> = (0..n).map(|i| le16(e, 2 + i * 2)).collect();
                    label = String::from_utf16_lossy(&units);
                }
            }
            ENTRY_BITMAP => {
                if e[1] & 1 != 0 {
                    problem(
                        p,
                        "Root directory: a second allocation bitmap is present.".to_string(),
                    );
                } else if bitmap_entry.is_some() {
                    problem(
                        p,
                        "Root directory: more than one allocation bitmap entry.".to_string(),
                    );
                } else {
                    bitmap_entry = Some((le32(e, 20), le64(e, 24)));
                }
            }
            ENTRY_UPCASE => {
                if upcase_entry.is_some() {
                    problem(
                        p,
                        "Root directory: more than one up-case table entry.".to_string(),
                    );
                } else {
                    upcase_entry = Some((le32(e, 20), le64(e, 24), le32(e, 4)));
                }
            }
            ENTRY_VOLUME_GUID => {}
            t if t & 0x80 == 0 => {} // not in use
            t => {
                if !foreign {
                    problem(
                        p,
                        format!("Root directory: unexpected entry of type 0x{t:02X} (the volume is not empty)."),
                    );
                }
                foreign = true;
            }
        }
    }

    // The clusters that must be marked in the bitmap, as bit ranges.
    let mut expected: Vec<(u64, u64)> = Vec::new();
    let mut add = |chain: &Chain| {
        for &(first, len) in &chain.extents {
            expected.push((first as u64 - 2, first as u64 - 2 + len as u64));
        }
    };
    add(&root);

    // Up-case table.
    match upcase_entry {
        None => problem(p, "Root directory: no up-case table entry.".to_string()),
        Some((first, len, stored_sum)) => {
            if len == 0 || len > MAX_UPCASE_BYTES || !len.is_multiple_of(2) {
                problem(p, format!("Up-case table: invalid length {len}."));
            } else {
                let need = len.div_ceil(cs);
                let chain = follow(dev, &mut fat, &lay, first, need, "Up-case table", p)?;
                add(&chain);
                if chain.clusters != need {
                    problem(
                        p,
                        format!(
                            "Up-case table: the chain has {} clusters, {need} expected.",
                            chain.clusters
                        ),
                    );
                } else {
                    let mut table: Vec<u8> = Vec::new();
                    read_chain(dev, &lay, &chain, len, &mut |_, bytes| {
                        table.extend_from_slice(bytes)
                    })?;
                    let sum = table_checksum(&table);
                    if sum != stored_sum {
                        problem(
                            p,
                            format!(
                                "Up-case table: checksum mismatch (stored 0x{stored_sum:08X}, computed 0x{sum:08X})."
                            ),
                        );
                    }
                    match expand_upcase(&table) {
                        None => problem(p, "Up-case table: malformed contents.".to_string()),
                        Some(map) => {
                            let ascii_ok = map.len() >= 128
                                && (0..128u8)
                                    .all(|c| map[c as usize] == c.to_ascii_uppercase() as u16);
                            if !ascii_ok {
                                problem(
                                    p,
                                    "Up-case table: the mandatory ASCII mappings are wrong."
                                        .to_string(),
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    // Allocation bitmap.
    let mut used_clusters = 0u64;
    match bitmap_entry {
        None => problem(p, "Root directory: no allocation bitmap entry.".to_string()),
        Some((first, len)) => {
            let want_len = count.div_ceil(8);
            if len != want_len {
                problem(
                    p,
                    format!("Allocation bitmap: length {len}, expected {want_len}."),
                );
            }
            let need = want_len.div_ceil(cs);
            let chain = follow(dev, &mut fat, &lay, first, need, "Allocation bitmap", p)?;
            add(&chain);
            if chain.clusters != need {
                problem(
                    p,
                    format!(
                        "Allocation bitmap: the chain has {} clusters, {need} expected.",
                        chain.clusters
                    ),
                );
            } else {
                expected.sort_unstable();
                if expected.windows(2).any(|w| w[0].1 > w[1].0) {
                    problem(
                        p,
                        "The bitmap, up-case table and root directory share clusters.".to_string(),
                    );
                }
                let mut want = vec![0u8; CHUNK];
                let mut mismatches = 0u32;
                let mut first_bad: Option<u64> = None;
                read_chain(dev, &lay, &chain, want_len, &mut |pos, bytes| {
                    let want = &mut want[..bytes.len()];
                    fill_bits(&expected, pos, want);
                    for (i, (have, want)) in bytes.iter().zip(want.iter()).enumerate() {
                        used_clusters += have.count_ones() as u64;
                        if have != want {
                            mismatches += 1;
                            if first_bad.is_none() {
                                let bit = (have ^ want).trailing_zeros() as u64;
                                first_bad = Some((pos + i as u64) * 8 + bit + 2);
                            }
                        }
                    }
                })?;
                if let Some(cluster) = first_bad {
                    problem(
                        p,
                        format!(
                            "Allocation bitmap: {mismatches} byte(s) do not match the clusters in use by the system files (first difference at cluster {cluster})."
                        ),
                    );
                }
            }
        }
    }

    let percent = main[112];
    let real = (used_clusters * 100 / count) as u8;
    if percent != 0xFF && percent != real {
        problem(
            p,
            format!("Boot sector: PercentInUse is {percent}, the bitmap says {real}."),
        );
    }

    Ok(ExFatReport {
        label,
        cluster_size: lay.cluster,
        cluster_count: lay.cluster_count,
        used_clusters: used_clusters.min(u32::MAX as u64) as u32,
        percent_in_use: percent,
        volume_serial: le32(main, 100),
        problems,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockdev::{FileDevice, MemDevice, SubDevice};
    use crate::part::testdev::{SparseDevice, XorShift};

    const GIB: u64 = 1024 * MIB;
    const NONE: Vec<String> = Vec::new();

    fn opts(label: &str, cluster: Option<u32>, offset: u64) -> ExFatOptions {
        ExFatOptions {
            label: label.to_string(),
            cluster_size: cluster,
            partition_offset_sectors: offset,
            volume_serial: 0x1234_ABCD,
        }
    }

    fn fmt(dev: &mut dyn BlockDevice, o: &ExFatOptions) -> Result<ExFatGeometry> {
        format(dev, o, &Cancel::new(), &mut |_| {})
    }

    /// The boot checksum written out exactly as the specification's pseudo
    /// code does, independent of `boot_checksum`.
    fn spec_boot_checksum(sectors: &[u8], bytes_per_sector: usize) -> u32 {
        let mut checksum: u32 = 0;
        for (index, byte) in sectors[..bytes_per_sector * 11].iter().enumerate() {
            if index == 106 || index == 107 || index == 112 {
                continue;
            }
            let carry = if checksum & 1 != 0 { 0x8000_0000u32 } else { 0 };
            checksum = carry.wrapping_add(checksum >> 1).wrapping_add(*byte as u32);
        }
        checksum
    }

    #[test]
    fn checksum_known_answers() {
        // By hand: 1 -> 0x00000001; then (1 rotated right = 0x80000000) + 2;
        // then (0x80000002 rotated right = 0x40000001) + 3.
        assert_eq!(table_checksum(&[1]), 1);
        assert_eq!(table_checksum(&[1, 2]), 0x8000_0002);
        assert_eq!(table_checksum(&[1, 2, 3]), 0x4000_0004);
        // Wrapping: 0xFFFFFFFF + 1 after a rotation that keeps all bits set.
        let mut sum = 0xFFFF_FFFFu32;
        sum = checksum_step(sum, 1);
        assert_eq!(sum, 0);
        // The boot checksum ignores bytes 106, 107 and 112.
        let mut a = vec![0u8; 11 * 512];
        a[0] = 0xEB;
        a[300] = 7;
        let base = boot_checksum(&a);
        assert_eq!(base, spec_boot_checksum(&a, 512));
        let mut b = a.clone();
        b[106] = 0xFF;
        b[107] = 0x01;
        b[112] = 55;
        assert_eq!(boot_checksum(&b), base);
        b[113] = 1;
        assert_ne!(boot_checksum(&b), base);
    }

    #[test]
    fn upcase_table_contents() {
        let table = upcase_table();
        assert!(table.len().is_multiple_of(2));
        assert!(table.len() < 16 * 1024, "compressed: {} bytes", table.len());
        let map = expand_upcase(&table).unwrap();
        assert_eq!(map.len(), 65536);
        for c in 0..128u32 {
            let want = (c as u8).to_ascii_uppercase() as u16;
            assert_eq!(map[c as usize], want, "ASCII {c}");
        }
        // The table starts with the run "0x61 identity mappings" and then 'A'..'Z'.
        assert_eq!(&table[..4], &[0xFF, 0xFF, 0x61, 0x00]);
        assert_eq!(&table[4..8], &[0x41, 0x00, 0x42, 0x00]);
        assert_eq!(map[0xE9], 0xC9, "e acute");
        assert_eq!(map[0xFF], 0x178, "y diaeresis");
        assert_eq!(map[0x3B1], 0x391, "alpha");
        assert_eq!(map[0x44F], 0x42F, "Cyrillic ya");
        assert_eq!(map[0x4E00], 0x4E00, "CJK");
        assert_eq!(map[0xDF], 0xDF, "sharp s has no single upper case");
        assert_eq!(map[0x131], 0x131, "dotless i must not become ASCII I");
        assert_eq!(map[0x17F], 0x17F, "long s must not become ASCII S");
        assert_eq!(map[0xD800], 0xD800);
        assert_eq!(map[0xFFFF], 0xFFFF);
        // Upper-casing twice changes nothing more.
        for c in 0..65536usize {
            let u = map[c] as usize;
            assert_eq!(map[u] as usize, u, "U+{c:04X}");
        }
        // A plain, uncompressed table expands to itself.
        let plain: Vec<u8> = (0..300u16).flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(expand_upcase(&plain).unwrap().len(), 300);
        // Malformed: run past the end of the BMP, run without a count.
        assert!(expand_upcase(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02, 0x00]).is_none());
        assert!(expand_upcase(&[0x41, 0x00, 0xFF, 0xFF]).is_none());
    }

    #[test]
    fn labels_and_defaults() {
        assert_eq!(sanitize_label("  My Drive  "), "My Drive");
        assert_eq!(sanitize_label("a/b\\c:d*e?f\"g<h>i|j"), "abcdefghij");
        assert_eq!(sanitize_label("0123456789ABCDEF"), "0123456789A");
        assert_eq!(sanitize_label("tab\there\u{7}"), "tabhere");
        // Ten units, then a character needing two: it is dropped whole.
        assert_eq!(sanitize_label("0123456789\u{1F600}"), "0123456789");
        assert_eq!(
            sanitize_label("\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}")
                .encode_utf16()
                .count(),
            10
        );
        assert_eq!(sanitize_label("0123456789 X"), "0123456789");
        assert_eq!(sanitize_label(""), "");
        assert_eq!(default_cluster_size(256 * MIB, 512), 4096);
        assert_eq!(default_cluster_size(256 * MIB + 512, 512), 32768);
        assert_eq!(default_cluster_size(32 * GIB, 512), 32768);
        assert_eq!(default_cluster_size(32 * GIB + 512, 4096), 131072);
        assert_eq!(default_cluster_size(8 * MIB, 4096), 4096);
    }

    #[test]
    fn format_64_mib_byte_level() {
        // A 64 MiB partition starting at sector 2048 of a 66 MiB disk.
        let mut disk = MemDevice::new(66 * MIB, 512);
        let geo = {
            let mut part = SubDevice::new(&mut disk, MIB, 64 * MIB).unwrap();
            let g = fmt(&mut part, &opts("BootForge \u{00e9}", None, 2048)).unwrap();
            let report = check(&mut part).unwrap();
            assert_eq!(report.problems, NONE);
            assert_eq!(report.label, "BootForge \u{00e9}");
            assert_eq!(report.volume_serial, 0x1234_ABCD);
            assert_eq!(report.cluster_size, 4096);
            g
        };
        assert_eq!(
            disk.data[MIB as usize - 1],
            0,
            "nothing before the partition"
        );
        let v = &disk.data[MIB as usize..65 * MIB as usize];

        // Computed by hand for 131072 sectors of 512 bytes, 4 KiB clusters:
        // FAT at 1 MiB (sector 2048); (64 MiB - 1 MiB) / 4 KiB = 16128 clusters
        // at most, so (16128 + 2) * 4 = 64520 bytes -> 127 sectors; heap at the
        // next MiB (sector 4096); (64 MiB - 2 MiB) / 4 KiB = 15872 clusters.
        assert_eq!(geo.fat_offset, 2048);
        assert_eq!(geo.fat_length, 127);
        assert_eq!(geo.cluster_heap_offset, 4096);
        assert_eq!(geo.cluster_count, 15872);
        assert_eq!(geo.bitmap_cluster, 2);
        assert_eq!(geo.bitmap_bytes, 1984);
        assert_eq!(geo.upcase_cluster, 3);
        let upcase = upcase_table();
        let upcase_clusters = upcase.len().div_ceil(4096) as u32;
        assert_eq!(geo.root_cluster, 3 + upcase_clusters);
        assert_eq!(geo.used_clusters, 2 + upcase_clusters);

        // Main boot sector.
        assert_eq!(&v[0..3], &[0xEB, 0x76, 0x90]);
        assert_eq!(&v[3..11], b"EXFAT   ");
        assert!(v[11..64].iter().all(|b| *b == 0));
        assert_eq!(&v[64..72], &2048u64.to_le_bytes());
        assert_eq!(&v[72..80], &131072u64.to_le_bytes());
        assert_eq!(&v[80..84], &2048u32.to_le_bytes());
        assert_eq!(&v[84..88], &127u32.to_le_bytes());
        assert_eq!(&v[88..92], &4096u32.to_le_bytes());
        assert_eq!(&v[92..96], &15872u32.to_le_bytes());
        assert_eq!(&v[96..100], &geo.root_cluster.to_le_bytes());
        assert_eq!(&v[100..104], &[0xCD, 0xAB, 0x34, 0x12]);
        assert_eq!(&v[104..106], &[0x00, 0x01]);
        assert_eq!(&v[106..108], &[0, 0]);
        assert_eq!(v[108], 9);
        assert_eq!(v[109], 3);
        assert_eq!(v[110], 1);
        assert_eq!(v[111], 0x80);
        assert_eq!(v[112], 0, "4 of 15872 clusters is under one percent");
        assert!(v[113..120].iter().all(|b| *b == 0));
        assert_eq!(&v[120..125], &[0xCD, 0x18, 0xF4, 0xEB, 0xFD]);
        assert!(v[125..510].iter().all(|b| *b == 0));
        assert_eq!(&v[510..512], &[0x55, 0xAA]);

        // Extended boot sectors, OEM parameters, reserved, checksum.
        for i in 1..=8usize {
            let sec = &v[i * 512..(i + 1) * 512];
            assert!(sec[..508].iter().all(|b| *b == 0));
            assert_eq!(&sec[508..], &[0x00, 0x00, 0x55, 0xAA]);
        }
        assert!(v[9 * 512..11 * 512].iter().all(|b| *b == 0));
        let sum = spec_boot_checksum(v, 512).to_le_bytes();
        for chunk in v[11 * 512..12 * 512].chunks(4) {
            assert_eq!(chunk, sum);
        }
        // Backup boot region is an exact copy.
        assert_eq!(&v[..12 * 512], &v[12 * 512..24 * 512]);
        // Up to the FAT: zero.
        assert!(v[24 * 512..MIB as usize].iter().all(|b| *b == 0));

        // FAT.
        let fat = &v[MIB as usize..MIB as usize + 127 * 512];
        assert_eq!(&fat[0..4], &[0xF8, 0xFF, 0xFF, 0xFF]);
        assert_eq!(&fat[4..8], &[0xFF; 4]);
        assert_eq!(&fat[8..12], &[0xFF; 4], "bitmap: one cluster");
        let mut at = 12;
        for i in 0..upcase_clusters {
            let want = if i + 1 < upcase_clusters {
                3 + i + 1
            } else {
                0xFFFF_FFFF
            };
            assert_eq!(&fat[at..at + 4], &want.to_le_bytes());
            at += 4;
        }
        assert_eq!(&fat[at..at + 4], &[0xFF; 4], "root directory");
        assert!(fat[at + 4..].iter().all(|b| *b == 0));
        assert!(v[MIB as usize + 127 * 512..2 * MIB as usize]
            .iter()
            .all(|b| *b == 0));

        // Bitmap in cluster 2.
        let heap = 2 * MIB as usize;
        let used = geo.used_clusters;
        assert_eq!(v[heap], ((1u16 << used) - 1) as u8);
        assert!(v[heap + 1..heap + 4096].iter().all(|b| *b == 0));
        // Up-case table in cluster 3.
        assert_eq!(&v[heap + 4096..heap + 4096 + upcase.len()], &upcase[..]);
        // Root directory.
        let root = heap + (geo.root_cluster as usize - 2) * 4096;
        let r = &v[root..root + 4096];
        assert_eq!(r[0], 0x83);
        assert_eq!(r[1], 11);
        let label: Vec<u8> = "BootForge \u{00e9}"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(&r[2..24], &label[..]);
        assert!(r[24..32].iter().all(|b| *b == 0));
        assert_eq!(r[32], 0x81);
        assert!(r[33..52].iter().all(|b| *b == 0));
        assert_eq!(&r[52..56], &2u32.to_le_bytes());
        assert_eq!(&r[56..64], &1984u64.to_le_bytes());
        assert_eq!(r[64], 0x82);
        assert_eq!(&r[65..68], &[0, 0, 0]);
        // Table checksum recomputed here with the specification's formula.
        let mut sum: u32 = 0;
        for b in &upcase {
            sum = (if sum & 1 != 0 { 0x8000_0000u32 } else { 0 })
                .wrapping_add(sum >> 1)
                .wrapping_add(*b as u32);
        }
        assert_eq!(&r[68..72], &sum.to_le_bytes());
        assert_eq!(geo.upcase_checksum, sum);
        assert!(r[72..84].iter().all(|b| *b == 0));
        assert_eq!(&r[84..88], &3u32.to_le_bytes());
        assert_eq!(&r[88..96], &(upcase.len() as u64).to_le_bytes());
        assert!(r[96..].iter().all(|b| *b == 0));
    }

    #[test]
    fn format_many_sizes_and_sectors() {
        let cases: [(u64, u32, Option<u32>, u64); 10] = [
            (MIB, 512, None, 0),
            (8 * MIB, 512, None, 2048),
            (8 * MIB + 512 * 3, 512, Some(512), 63),
            (300 * MIB, 512, None, 2048),
            (300 * MIB + 512 * 77, 512, Some(65536), 63),
            (GIB + 4096 * 5, 4096, None, 256),
            (64 * MIB, 4096, Some(4096), 255),
            (40 * GIB, 512, None, 2048),
            (GIB, 512, Some(512), 2048),
            (2 * GIB, 512, Some(32 * 1024 * 1024), 2048),
        ];
        for (size, sector, cluster, offset) in cases {
            let mut d = SparseDevice::new(size, sector);
            let mut last = 0u64;
            let geo = format(
                &mut d,
                &opts("Volume", cluster, offset),
                &Cancel::new(),
                &mut |n| {
                    assert!(n >= last, "progress goes forward");
                    last = n;
                },
            )
            .unwrap();
            let s = sector as u64;
            let what = format!("size {size} sector {sector} cluster {cluster:?}");
            assert_eq!(geo.sector_size, sector);
            assert_eq!(
                geo.cluster_size,
                cluster.unwrap_or_else(|| default_cluster_size(size, sector))
            );
            assert_eq!(geo.volume_sectors, size / s);
            assert!(geo.fat_offset >= 24, "{what}");
            assert!(
                geo.fat_offset + geo.fat_length <= geo.cluster_heap_offset,
                "{what}"
            );
            assert!(
                geo.fat_length as u64 * s / 4 >= geo.cluster_count as u64 + 2,
                "{what}"
            );
            let heap_end = geo.cluster_heap_offset as u64 * s
                + geo.cluster_count as u64 * geo.cluster_size as u64;
            assert!(heap_end <= size, "{what}");
            assert!(
                size - heap_end < geo.cluster_size as u64,
                "{what}: no cluster wasted at the end"
            );
            if size >= 64 * MIB {
                // Aligned to 1 MiB on the disk, wherever the partition starts.
                assert_eq!((offset + geo.fat_offset as u64) * s % MIB, 0, "{what}");
                assert_eq!(
                    (offset + geo.cluster_heap_offset as u64) * s % MIB,
                    0,
                    "{what}"
                );
            }
            assert_eq!(
                last,
                geo.cluster_heap_offset as u64 * s
                    + geo.used_clusters as u64 * geo.cluster_size as u64
            );
            let report = check(&mut d).unwrap();
            assert_eq!(report.problems, NONE, "{what}");
            assert_eq!(report.label, "Volume");
            assert_eq!(report.cluster_count, geo.cluster_count);
            assert_eq!(report.used_clusters, geo.used_clusters);
            assert_eq!(report.cluster_size, geo.cluster_size);
            assert_eq!(
                report.percent_in_use as u64,
                geo.used_clusters as u64 * 100 / geo.cluster_count as u64
            );
            // Sector 0 of a 4096-byte-sector volume still has 0x55AA at 510.
            let b = d.bytes(0, 512);
            assert_eq!(
                (b[108], &b[510..512]),
                (sector.trailing_zeros() as u8, &[0x55u8, 0xAA][..])
            );
        }
    }

    #[test]
    fn format_4096_byte_sectors_byte_level() {
        let mut d = MemDevice::new(64 * MIB, 4096);
        let geo = fmt(&mut d, &opts("", Some(32768), 256)).unwrap();
        let v = &d.data;
        assert_eq!(v[108], 12);
        assert_eq!(v[109], 3);
        assert_eq!(&v[72..80], &16384u64.to_le_bytes());
        // FAT at 1 MiB = sector 256; heap at 2 MiB = sector 512.
        assert_eq!((geo.fat_offset, geo.cluster_heap_offset), (256, 512));
        assert_eq!(geo.cluster_count, 62 * 32);
        // Sized for the 63 MiB / 32 KiB = 2016 clusters possible before the heap
        // is aligned: 2018 entries * 4 = 8072 bytes -> 2 sectors.
        assert_eq!(geo.fat_length, 2);
        for i in 1..=8usize {
            assert_eq!(
                &v[(i + 1) * 4096 - 4..(i + 1) * 4096],
                &[0x00, 0x00, 0x55, 0xAA]
            );
        }
        let sum = spec_boot_checksum(v, 4096).to_le_bytes();
        assert_eq!(
            v[11 * 4096..12 * 4096]
                .chunks(4)
                .filter(|c| *c == sum)
                .count(),
            1024
        );
        assert_eq!(&v[..12 * 4096], &v[12 * 4096..24 * 4096]);
        // No label: the first root entry is an unused label entry.
        let root = 2 * MIB as usize + (geo.root_cluster as usize - 2) * 32768;
        assert_eq!(&v[root..root + 2], &[0x03, 0x00]);
        assert_eq!(v[root + 32], 0x81);
        assert_eq!(v[root + 64], 0x82);
        let report = check(&mut d).unwrap();
        assert_eq!(report.problems, NONE);
        assert_eq!(report.label, "");
    }

    #[test]
    fn format_overwrites_old_metadata_and_keeps_free_space() {
        let mut d = MemDevice::new(16 * MIB, 512);
        d.data.fill(0xA5);
        let geo = fmt(&mut d, &opts("X", None, 0)).unwrap();
        assert_eq!(check(&mut d).unwrap().problems, NONE);
        let first_free = geo.cluster_heap_offset as usize * 512 + geo.used_clusters as usize * 4096;
        assert!(d.data[first_free..].iter().all(|b| *b == 0xA5));
        assert_eq!(d.data[first_free - 1], 0);
    }

    #[test]
    fn format_on_an_image_file() {
        let path = std::env::temp_dir().join(format!("bootforge-exfat-{}.img", std::process::id()));
        let result = (|| -> Result<ExFatReport> {
            let mut d = FileDevice::create(&path, 20 * MIB, 512)?;
            fmt(&mut d, &opts("On disk", None, 0))?;
            drop(d);
            let mut d = FileDevice::open(&path, 512)?;
            check(&mut d)
        })();
        std::fs::remove_file(&path).unwrap();
        let report = result.unwrap();
        assert_eq!(report.problems, NONE);
        assert_eq!(report.label, "On disk");
    }

    #[test]
    fn format_refusals_and_cancel() {
        let mut d = MemDevice::new(16 * MIB, 512);
        for bad in [3000u32, 256, 0, 64 * 1024 * 1024] {
            let e = fmt(&mut d, &opts("", Some(bad), 0));
            assert!(matches!(e, Err(Error::Invalid(_))), "cluster {bad}");
        }
        // A 32 MiB cluster cannot hold bitmap, table and root in a 16 MiB volume.
        assert!(fmt(&mut d, &opts("", Some(32 * 1024 * 1024), 0)).is_err());
        assert_eq!(d.writes, 0);
        // Cluster smaller than the sector.
        let mut d4 = MemDevice::new(16 * MIB, 4096);
        assert!(fmt(&mut d4, &opts("", Some(512), 0)).is_err());
        // Too small.
        let mut tiny = MemDevice::new(MIB - 512, 512);
        assert!(matches!(
            fmt(&mut tiny, &opts("", None, 0)),
            Err(Error::Invalid(_))
        ));
        assert_eq!(tiny.writes + d4.writes, 0);
        // Cancelled before anything is written.
        let cancel = Cancel::new();
        cancel.cancel();
        let e = format(&mut d, &opts("", None, 0), &cancel, &mut |_| {});
        assert!(matches!(e, Err(Error::Cancelled)));
        assert_eq!(d.writes, 0);
        // Cancelled half way: no boot sector yet, so it does not look like exFAT.
        let cancel = Cancel::new();
        let c2 = cancel.clone();
        let e = format(&mut d, &opts("", None, 0), &cancel, &mut |n| {
            if n > 0 {
                c2.cancel();
            }
        });
        assert!(matches!(e, Err(Error::Cancelled)));
        assert!(check(&mut d).is_err());
    }

    fn sample() -> MemDevice {
        let mut d = MemDevice::new(16 * MIB, 512);
        fmt(&mut d, &opts("Sample", None, 0)).unwrap();
        assert_eq!(check(&mut d).unwrap().problems, NONE);
        d
    }

    fn problems_after(edit: impl FnOnce(&mut MemDevice)) -> Vec<String> {
        let mut d = sample();
        edit(&mut d);
        check(&mut d).unwrap().problems
    }

    fn has(problems: &[String], needle: &str) -> bool {
        problems.iter().any(|p| p.contains(needle))
    }

    #[test]
    fn checker_reports_corruption() {
        // 16 MiB, 4 KiB clusters, cluster-aligned: FAT at sector 24.
        let d = sample();
        let fat = le32(&d.data, 80) as usize * 512;
        let heap = le32(&d.data, 88) as usize * 512;
        let root = heap + (le32(&d.data, 96) as usize - 2) * 4096;
        assert_eq!(fat, 24 * 512);

        // Boot sector byte (the serial): main checksum and backup comparison.
        let p = problems_after(|d| d.data[100] ^= 1);
        assert!(has(&p, "Main boot region: checksum mismatch"), "{p:?}");
        assert!(has(&p, "backup boot region differs"), "{p:?}");
        // Bytes excluded from the checksum do not trip it.
        let p = problems_after(|d| d.data[112] = 0xFF);
        assert_eq!(p, NONE);
        let p = problems_after(|d| d.data[106] = 0x02);
        assert!(has(&p, "marked dirty"), "{p:?}");
        assert!(!has(&p, "checksum"), "{p:?}");
        let p = problems_after(|d| d.data[112] = 50);
        assert!(has(&p, "PercentInUse is 50"), "{p:?}");
        // Checksum sector, backup region, extended signature, 0x55AA.
        let p = problems_after(|d| d.data[11 * 512 + 200] ^= 0x40);
        assert_eq!(p.len(), 1);
        assert!(has(&p, "Main boot region: checksum mismatch"), "{p:?}");
        let p = problems_after(|d| d.data[12 * 512 + 3] ^= 0x20);
        assert!(has(&p, "Backup boot region: checksum mismatch"), "{p:?}");
        assert!(has(&p, "backup boot region differs"), "{p:?}");
        let p = problems_after(|d| d.data[4 * 512 - 1] = 0);
        assert!(
            has(&p, "Extended boot sector 3: signature missing"),
            "{p:?}"
        );
        let p = problems_after(|d| d.data[510] = 0);
        assert!(has(&p, "boot signature 0x55AA missing"), "{p:?}");
        let p = problems_after(|d| d.data[20] = 1);
        assert!(has(&p, "must-be-zero"), "{p:?}");

        // FAT.
        let p = problems_after(|d| d.data[fat] = 0xF0);
        assert!(has(&p, "FAT: unexpected first entries"), "{p:?}");
        // Bitmap chain (cluster 2) made to loop on itself.
        let p = problems_after(|d| d.data[fat + 8..fat + 12].copy_from_slice(&2u32.to_le_bytes()));
        assert!(
            has(&p, "Allocation bitmap: the cluster chain is longer"),
            "{p:?}"
        );
        // Up-case chain (cluster 3, one cluster) running on into the root / pointing outside.
        let p = problems_after(|d| {
            let root_cluster = le32(&d.data, 96);
            d.data[fat + 12..fat + 16].copy_from_slice(&root_cluster.to_le_bytes())
        });
        assert!(
            has(&p, "Up-case table: the cluster chain is longer"),
            "{p:?}"
        );
        let p = problems_after(|d| {
            d.data[fat + 12..fat + 16].copy_from_slice(&0x00FF_FFFFu32.to_le_bytes())
        });
        assert!(
            has(&p, "Up-case table: the cluster chain points outside"),
            "{p:?}"
        );
        // Root chain entry zeroed (free cluster in a chain).
        let root_cluster = le32(&d.data, 96) as usize;
        let p =
            problems_after(|d| d.data[fat + root_cluster * 4..fat + root_cluster * 4 + 4].fill(0));
        assert!(
            has(&p, "Root directory: the cluster chain points outside"),
            "{p:?}"
        );

        // Bitmap: one extra cluster marked, one system cluster not marked.
        let p = problems_after(|d| d.data[heap + 100] = 0x10);
        assert!(has(&p, "Allocation bitmap: 1 byte(s)"), "{p:?}");
        assert!(
            has(
                &p,
                &format!("first difference at cluster {}", 100 * 8 + 4 + 2)
            ),
            "{p:?}"
        );
        let p = problems_after(|d| d.data[heap] &= !0x02);
        assert!(has(&p, "first difference at cluster 3"), "{p:?}");

        // Up-case table contents.
        let p = problems_after(|d| d.data[heap + 4096 + 300] ^= 0x01);
        assert!(has(&p, "Up-case table: checksum mismatch"), "{p:?}");
        // 'A' mapped to 'B' with the checksum fixed up: only the mapping check fires.
        let p = problems_after(|d| {
            d.data[heap + 4096 + 4] = 0x42;
            let len = le64(&d.data, root + 64 + 24) as usize;
            let sum = table_checksum(&d.data[heap + 4096..heap + 4096 + len]);
            d.data[root + 68..root + 72].copy_from_slice(&sum.to_le_bytes());
        });
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(has(&p, "mandatory ASCII mappings"), "{p:?}");

        // Root directory entries.
        let p = problems_after(|d| d.data[root + 32] = 0x01);
        assert!(has(&p, "no allocation bitmap entry"), "{p:?}");
        let p = problems_after(|d| d.data[root + 64] = 0x02);
        assert!(has(&p, "no up-case table entry"), "{p:?}");
        let p = problems_after(|d| d.data[root + 56] ^= 1);
        assert!(has(&p, "Allocation bitmap: length"), "{p:?}");
        let p = problems_after(|d| d.data[root + 1] = 12);
        assert!(has(&p, "label length 12"), "{p:?}");
        let p = problems_after(|d| d.data[root + 96] = 0x85);
        assert!(has(&p, "unexpected entry of type 0x85"), "{p:?}");
        let p = problems_after(|d| {
            d.data[root + 88..root + 96].copy_from_slice(&u64::MAX.to_le_bytes())
        });
        assert!(has(&p, "Up-case table: invalid length"), "{p:?}");
        // Up-case table pointed at the bitmap's cluster.
        let p = problems_after(|d| d.data[root + 84] = 2);
        assert!(
            has(&p, "share clusters") || has(&p, "Up-case table"),
            "{p:?}"
        );
    }

    #[test]
    fn checker_rejects_what_it_cannot_parse() {
        let edits: [(usize, u8); 9] = [
            (3, b'N'),  // file system name
            (108, 10),  // sector shift
            (109, 30),  // cluster shift
            (79, 0x7F), // volume length far beyond the device
            (80, 1),    // FAT inside the boot regions (offset 1 + 24 -> not < 24, see below)
            (87, 0x10), // FAT length reaching into the heap
            (95, 0x7F), // cluster count
            (99, 0x7F), // root cluster
            (96, 0),    // root cluster 0 (low byte)
        ];
        for (at, value) in edits {
            let mut d = sample();
            d.data[at] = value;
            if at == 80 {
                d.data[80..84].copy_from_slice(&5u32.to_le_bytes());
            }
            if at == 96 {
                d.data[96..100].fill(0);
            }
            assert!(
                matches!(check(&mut d), Err(Error::Invalid(_))),
                "edit at {at}"
            );
        }
        // Truncated copies.
        let full = sample();
        for sectors in [0usize, 1, 23, 24, 100, 4096, 32767] {
            let mut d = MemDevice::new(sectors as u64 * 512, 512);
            d.data.copy_from_slice(&full.data[..sectors * 512]);
            assert!(check(&mut d).is_err(), "{sectors} sectors");
        }
        // Blank and garbage devices.
        assert!(check(&mut MemDevice::new(16 * MIB, 512)).is_err());
        let mut rng = XorShift(99);
        let mut d = MemDevice::new(MIB, 512);
        rng.fill(&mut d.data);
        assert!(check(&mut d).is_err());
        // The same volume read with the wrong sector size.
        let mut d = MemDevice::new(16 * MIB, 4096);
        d.data.copy_from_slice(&full.data);
        assert!(check(&mut d).is_err());
    }

    #[test]
    fn checker_survives_random_damage() {
        let base = sample();
        let heap = le32(&base.data, 88) as usize * 512;
        let meta = heap + 8 * 4096;
        let mut rng = XorShift(0xC0FF_EE00_1234_5678);
        for round in 0..400u32 {
            let mut d = MemDevice::new(16 * MIB, 512);
            d.data.copy_from_slice(&base.data);
            for _ in 0..1 + round % 6 {
                // Mostly the boot sector and root directory, sometimes anywhere in the metadata.
                let at = match rng.next() % 4 {
                    0 => 64 + (rng.next() % 56) as usize,
                    1 => heap + (rng.next() % (8 * 4096)) as usize,
                    2 => 24 * 512 + (rng.next() % 64) as usize,
                    _ => (rng.next() % meta as u64) as usize,
                };
                d.data[at] = rng.next() as u8;
            }
            // Either a clean error or a report; never a panic or a hang.
            let _ = check(&mut d);
        }
    }
}
