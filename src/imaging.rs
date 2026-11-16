//! Raw image writing, verification, drive backup and the bad-block check.
//!
//! Everything here is generic over [`BlockDevice`] / [`ReadAt`], so the same
//! code that writes a real drive is exercised in the tests against
//! `MemDevice` and `FileDevice`.
//!
//! Conventions shared by all operations:
//! * data moves in chunks of 1 MiB (rounded down to a whole number of sectors);
//! * `cancel` is checked before every chunk and a cancelled run returns
//!   `Error::Cancelled`;
//! * `progress` receives the bytes completed so far within that operation.

use crate::blockdev::{BlockDevice, ReadAt};
use crate::error::{Error, IoContext, Result};
use crate::hash::MultiHasher;
use crate::model::{Cancel, Checksums, ProgressFn};
use crate::util::{hex, human_size};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Write;
use std::ops::Range;
use std::path::Path;

const MIB: u64 = 1024 * 1024;

/// At most this many separate bad ranges are listed in a [`BadBlockReport`].
pub const MAX_BAD_RANGES: usize = 256;

/// After this many consecutive chunks failing with I/O errors the bad-block
/// check gives up: the drive has most likely been unplugged or has died.
const MAX_CONSECUTIVE_IO_ERRORS: u32 = 32;

/// The I/O chunk for a device: 1 MiB rounded down to whole sectors (and never
/// less than one sector).
fn chunk_len(sector: u32) -> usize {
    let s = sector.max(1) as u64;
    (MIB - MIB % s).max(s) as usize
}

fn round_up(n: u64, to: u64) -> u64 {
    n.div_ceil(to) * to
}

// --------------------------------------------------------------------------------------
// Image writing and verification
// --------------------------------------------------------------------------------------

/// What [`write_image`] wrote, kept so that [`verify_image`] can check the
/// drive without reading the image file a second time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteDigest {
    /// Number of bytes written to the device: the image size rounded up to a
    /// whole sector (the padding is zeros).
    pub len: u64,
    /// Size of the chunks the CRCs were computed over (the last chunk may be
    /// shorter).
    pub chunk_size: u32,
    /// SHA-256 of the `len` bytes written, lower-case hex.
    pub sha256: String,
    /// CRC-32 of every chunk, in order.
    pub chunk_crcs: Vec<u32>,
}

/// Writes `src` to the start of `dev`, byte for byte.
///
/// The image is written sequentially in 1 MiB chunks; a final partial sector
/// is padded with zeros. The device is flushed at the end. Returns
/// `Error::Invalid` (before anything is written) when the image does not fit.
///
/// `progress` receives the number of image bytes written so far, ending at
/// `src.size()`.
pub fn write_image(
    src: &mut dyn ReadAt,
    dev: &mut dyn BlockDevice,
    cancel: &Cancel,
    progress: ProgressFn,
) -> Result<WriteDigest> {
    let sector = dev.sector_size();
    if sector == 0 {
        return Err(Error::invalid("The drive reports a sector size of zero."));
    }
    let src_len = src.size();
    if src_len > dev.size() {
        return Err(Error::invalid(format!(
            "The image ({}) is larger than the drive ({}); it needs {} more.",
            human_size(src_len),
            human_size(dev.size()),
            human_size(src_len - dev.size())
        )));
    }
    let chunk = chunk_len(sector);
    let mut buf = vec![0u8; chunk];
    let mut sha = Sha256::new();
    let mut crcs = Vec::with_capacity(src_len.div_ceil(chunk as u64) as usize);
    let mut offset = 0u64;
    while offset < src_len {
        cancel.check()?;
        let n = (src_len - offset).min(chunk as u64) as usize;
        let padded = round_up(n as u64, sector as u64) as usize;
        src.read_at(offset, &mut buf[..n])
            .ctx("Could not read the image file")?;
        buf[n..padded].fill(0);
        dev.write_at(offset, &buf[..padded]).map_err(|e| {
            Error::io(
                format!("Could not write to the drive at offset {offset}"),
                e,
            )
        })?;
        sha.update(&buf[..padded]);
        crcs.push(crc32fast::hash(&buf[..padded]));
        offset += n as u64;
        progress(offset);
    }
    cancel.check()?;
    dev.flush().ctx("Could not flush the drive")?;
    Ok(WriteDigest {
        len: round_up(src_len, sector as u64),
        chunk_size: chunk as u32,
        sha256: hex(&sha.finalize()),
        chunk_crcs: crcs,
    })
}

/// Reads back what [`write_image`] wrote and compares it with `digest`.
///
/// Every chunk's CRC-32 is compared first, so that a failure can name the
/// position of the first difference (to the nearest chunk); the SHA-256 of
/// the whole range is checked at the end. A mismatch is an `Error::Msg`.
///
/// `progress` receives the bytes verified so far, ending at `digest.len`.
pub fn verify_image(
    dev: &mut dyn BlockDevice,
    digest: &WriteDigest,
    cancel: &Cancel,
    progress: ProgressFn,
) -> Result<()> {
    let sector = dev.sector_size() as u64;
    let chunk = digest.chunk_size as u64;
    if sector == 0
        || chunk == 0
        || !chunk.is_multiple_of(sector)
        || !digest.len.is_multiple_of(sector)
        || digest.len > dev.size()
        || digest.chunk_crcs.len() as u64 != digest.len.div_ceil(chunk)
    {
        return Err(Error::invalid(
            "The write record does not match this drive; it cannot be verified.",
        ));
    }
    let mut buf = vec![0u8; chunk as usize];
    let mut sha = Sha256::new();
    let mut offset = 0u64;
    for crc in &digest.chunk_crcs {
        cancel.check()?;
        let n = (digest.len - offset).min(chunk) as usize;
        dev.read_at(offset, &mut buf[..n])
            .map_err(|e| Error::io(format!("Could not read the drive at offset {offset}"), e))?;
        if crc32fast::hash(&buf[..n]) != *crc {
            return Err(Error::msg(format!(
                "Verification failed: the drive returned different data than was written, \
                 first within the {} starting at byte offset {offset} ({} into the drive). \
                 The drive may be failing or may not have its advertised capacity.",
                human_size(n as u64),
                human_size(offset)
            )));
        }
        sha.update(&buf[..n]);
        offset += n as u64;
        progress(offset);
    }
    if hex(&sha.finalize()) != digest.sha256 {
        return Err(Error::msg(
            "Verification failed: the SHA-256 of the data on the drive differs from what was written.",
        ));
    }
    Ok(())
}

// --------------------------------------------------------------------------------------
// Backup
// --------------------------------------------------------------------------------------

/// Copies the whole device into the image file `out` (created or truncated)
/// and returns the checksums of the image.
///
/// On any error, including cancellation, the partial file is deleted.
/// Restoring a backup is [`write_image`] with the file as source.
///
/// `progress` receives the bytes copied so far, ending at `dev.size()`.
pub fn backup_to_file(
    dev: &mut dyn BlockDevice,
    out: &Path,
    cancel: &Cancel,
    progress: ProgressFn,
) -> Result<Checksums> {
    if dev.sector_size() == 0 {
        return Err(Error::invalid("The drive reports a sector size of zero."));
    }
    let mut file = File::create(out).ctx("Could not create the backup file")?;
    let result = copy_device(dev, &mut file, cancel, progress);
    drop(file);
    if result.is_err() {
        // Never leave a truncated image behind that looks like a backup.
        let _ = std::fs::remove_file(out);
    }
    result
}

fn copy_device(
    dev: &mut dyn BlockDevice,
    file: &mut File,
    cancel: &Cancel,
    progress: ProgressFn,
) -> Result<Checksums> {
    let size = dev.size();
    let chunk = chunk_len(dev.sector_size());
    let mut buf = vec![0u8; chunk];
    let mut hasher = MultiHasher::new();
    let mut offset = 0u64;
    while offset < size {
        cancel.check()?;
        let n = (size - offset).min(chunk as u64) as usize;
        dev.read_at(offset, &mut buf[..n])
            .map_err(|e| Error::io(format!("Could not read the drive at offset {offset}"), e))?;
        file.write_all(&buf[..n])
            .ctx("Could not write the backup file (is the disk full?)")?;
        hasher.update(&buf[..n]);
        offset += n as u64;
        progress(offset);
    }
    cancel.check()?;
    file.sync_all().ctx("Could not finish the backup file")?;
    Ok(hasher.finish())
}

// --------------------------------------------------------------------------------------
// Zeroing
// --------------------------------------------------------------------------------------

/// Writes zeros over `len` bytes starting at `offset`.
///
/// Both must be multiples of the sector size and the range must lie inside
/// the device (`Error::Invalid` otherwise). Does not flush.
///
/// `progress` receives the bytes zeroed so far, ending at `len`.
pub fn zero_fill(
    dev: &mut dyn BlockDevice,
    offset: u64,
    len: u64,
    cancel: &Cancel,
    progress: ProgressFn,
) -> Result<()> {
    let sector = dev.sector_size() as u64;
    let end = offset.checked_add(len);
    if sector == 0
        || !offset.is_multiple_of(sector)
        || !len.is_multiple_of(sector)
        || end.is_none_or(|e| e > dev.size())
    {
        return Err(Error::invalid(format!(
            "Cannot zero {len} bytes at offset {offset}: the range is not sector-aligned or lies outside the drive."
        )));
    }
    let chunk = chunk_len(sector as u32);
    let zeros = vec![0u8; chunk.min(len as usize)];
    let mut done = 0u64;
    while done < len {
        cancel.check()?;
        let n = (len - done).min(chunk as u64) as usize;
        let at = offset + done;
        dev.write_at(at, &zeros[..n])
            .map_err(|e| Error::io(format!("Could not write to the drive at offset {at}"), e))?;
        done += n as u64;
        progress(done);
    }
    Ok(())
}

/// Zeroes the first and the last MiB of the device (clamped to its size) and
/// flushes, which removes the MBR, both GPT copies and the boot sectors of
/// common file systems so that nothing stale is recognised afterwards.
pub fn erase_signatures(dev: &mut dyn BlockDevice) -> Result<()> {
    let sector = dev.sector_size() as u64;
    if sector == 0 {
        return Err(Error::invalid("The drive reports a sector size of zero."));
    }
    let size = dev.size();
    // One MiB in whole sectors, or the whole device when it is smaller.
    let span = round_up(MIB, sector).min(size);
    let never = Cancel::new();
    zero_fill(dev, 0, span, &never, &mut |_| {})?;
    if size > span {
        // `size` and `span` are sector multiples, so the tail start is aligned.
        let tail = (size - span).max(span);
        zero_fill(dev, tail, size - tail, &never, &mut |_| {})?;
    }
    dev.flush().ctx("Could not flush the drive")?;
    Ok(())
}

// --------------------------------------------------------------------------------------
// Bad-block check
// --------------------------------------------------------------------------------------

/// Outcome of [`bad_block_check`].
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct BadBlockReport {
    /// Size of the device that was tested.
    pub device_size: u64,
    /// Total number of bytes in blocks that could not be written, could not
    /// be read or read back wrong (counted in whole sectors, or whole chunks
    /// for I/O errors).
    pub bad_bytes: u64,
    /// The bad areas as byte ranges, ascending and merged. At most
    /// [`MAX_BAD_RANGES`] are listed; see `truncated`.
    pub bad_ranges: Vec<Range<u64>>,
    /// There were more bad ranges than are listed (`bad_bytes` still counts
    /// all of them).
    pub truncated: bool,
    /// Number of chunks whose write or read failed with an I/O error.
    pub io_errors: u64,
    /// Number of blocks that read back the test pattern of a *different*
    /// block: the drive stores several addresses in the same place.
    pub aliased_blocks: u64,
    /// The drive appears to have less real capacity than it reports (a
    /// "fake" flash drive that wraps addresses). Drives that silently drop
    /// writes past their real capacity show up as one large bad range instead.
    pub fake_capacity: bool,
}

impl BadBlockReport {
    /// No bad blocks and no sign of fake capacity.
    pub fn is_clean(&self) -> bool {
        self.bad_bytes == 0 && !self.fake_capacity && self.io_errors == 0
    }

    /// One or two sentences for the user.
    pub fn summary(&self) -> String {
        if self.is_clean() {
            return format!(
                "No bad blocks found ({} tested).",
                human_size(self.device_size)
            );
        }
        let mut s = format!(
            "{} of {} failed the test in {}{} area(s)",
            human_size(self.bad_bytes),
            human_size(self.device_size),
            if self.truncated { "more than " } else { "" },
            self.bad_ranges.len()
        );
        if let Some(first) = self.bad_ranges.first() {
            s.push_str(&format!(
                ", the first at {} into the drive",
                human_size(first.start)
            ));
        }
        s.push('.');
        if self.fake_capacity {
            s.push_str(
                " Parts of the drive return data written elsewhere: its real capacity is \
                 smaller than it reports (a counterfeit drive).",
            );
        }
        s
    }

    fn add_bad(&mut self, range: Range<u64>) {
        self.bad_bytes += range.end - range.start;
        if let Some(last) = self.bad_ranges.last_mut() {
            if range.start <= last.end {
                last.end = last.end.max(range.end);
                return;
            }
        }
        if self.bad_ranges.len() >= MAX_BAD_RANGES {
            self.truncated = true;
        } else {
            self.bad_ranges.push(range);
        }
    }
}

/// SplitMix64 step: a small, well-mixed generator for the test pattern.
fn mix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The test pattern. Every block (one sector) starts with its own block index
/// XORed with a per-run key and continues with pseudo-random data derived
/// from the index and the seed, so no two blocks of a run are alike and a
/// block that turns up in the wrong place can be recognised.
struct Pattern {
    seed: u64,
    key: u64,
    block: usize,
}

impl Pattern {
    fn new(seed: u64, block: usize) -> Pattern {
        let mut s = seed;
        Pattern {
            seed,
            key: mix(&mut s),
            block,
        }
    }

    /// Fills `buf` (whole blocks) with the pattern of the blocks starting at `first`.
    fn fill(&self, first: u64, buf: &mut [u8]) {
        for (i, block) in buf.chunks_mut(self.block).enumerate() {
            let index = first + i as u64;
            let mut state = self.seed ^ index.wrapping_mul(0xD6E8_FEB8_6659_FD93);
            let tag = (index ^ self.key).to_le_bytes();
            for (w, word) in block.chunks_mut(8).enumerate() {
                let v = if w == 0 {
                    tag
                } else {
                    mix(&mut state).to_le_bytes()
                };
                word.copy_from_slice(&v[..word.len()]);
            }
        }
    }

    /// The block index recorded in the first bytes of `block`.
    fn tagged_index(&self, block: &[u8]) -> Option<u64> {
        let tag: [u8; 8] = block.get(..8)?.try_into().ok()?;
        Some(u64::from_le_bytes(tag) ^ self.key)
    }
}

fn run_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5EED);
    let mut s = nanos ^ ((std::process::id() as u64) << 32);
    mix(&mut s)
}

/// Destructive surface test: **erases the device**.
///
/// Phase 1 writes a pattern derived from each block's own position and a
/// per-run seed over the whole device and flushes; phase 2 reads everything
/// back and compares. Because every block's content depends on its address,
/// counterfeit drives that wrap addresses onto a smaller real capacity are
/// detected (`fake_capacity`), not just blocks that fail outright.
///
/// Chunks that fail with an I/O error are recorded as bad and the test
/// continues; only when many consecutive chunks fail (drive unplugged) is an
/// error returned. Afterwards, and also when the run is cancelled or fails,
/// the first and last MiB are zeroed so that no pattern is mistaken for a
/// partition table.
///
/// `progress` covers both phases: it receives bytes done so far out of a
/// total of `2 * dev.size()`.
pub fn bad_block_check(
    dev: &mut dyn BlockDevice,
    cancel: &Cancel,
    progress: ProgressFn,
) -> Result<BadBlockReport> {
    bad_block_check_seeded(dev, run_seed(), cancel, progress)
}

fn bad_block_check_seeded(
    dev: &mut dyn BlockDevice,
    seed: u64,
    cancel: &Cancel,
    progress: ProgressFn,
) -> Result<BadBlockReport> {
    if dev.sector_size() < 8 {
        return Err(Error::invalid("The drive reports an invalid sector size."));
    }
    let result = surface_test(dev, seed, cancel, progress);
    // Whatever happened, do not leave pattern data where partition tables live.
    let erased = erase_signatures(dev);
    let report = result?;
    erased?;
    Ok(report)
}

fn surface_test(
    dev: &mut dyn BlockDevice,
    seed: u64,
    cancel: &Cancel,
    progress: ProgressFn,
) -> Result<BadBlockReport> {
    let size = dev.size();
    let block = dev.sector_size() as usize;
    let blocks = size / block as u64;
    let chunk = chunk_len(block as u32);
    let pattern = Pattern::new(seed, block);
    let mut report = BadBlockReport {
        device_size: size,
        ..BadBlockReport::default()
    };
    let mut expected = vec![0u8; chunk];
    let mut actual = vec![0u8; chunk];
    // Chunks whose write failed; they are not read back.
    let mut failed_writes: Vec<Range<u64>> = Vec::new();
    let mut consecutive = 0u32;

    // Phase 1: write the pattern everywhere.
    let mut offset = 0u64;
    while offset < size {
        cancel.check()?;
        let n = (size - offset).min(chunk as u64) as usize;
        pattern.fill(offset / block as u64, &mut expected[..n]);
        match dev.write_at(offset, &expected[..n]) {
            Ok(()) => consecutive = 0,
            Err(e) => {
                consecutive += 1;
                if consecutive >= MAX_CONSECUTIVE_IO_ERRORS {
                    return Err(Error::io(
                        format!("The drive stopped accepting writes at offset {offset}"),
                        e,
                    ));
                }
                report.io_errors += 1;
                failed_writes.push(offset..offset + n as u64);
            }
        }
        offset += n as u64;
        progress(offset);
    }
    cancel.check()?;
    dev.flush().ctx("Could not flush the drive")?;

    // Phase 2: read back and compare.
    let mut scratch = vec![0u8; block];
    let mut failed = failed_writes.iter().peekable();
    consecutive = 0;
    offset = 0;
    while offset < size {
        cancel.check()?;
        let n = (size - offset).min(chunk as u64) as usize;
        let range = offset..offset + n as u64;
        offset += n as u64;
        if failed.peek().is_some_and(|f| f.start == range.start) {
            failed.next();
            report.add_bad(range);
            progress(size + offset);
            continue;
        }
        if let Err(e) = dev.read_at(range.start, &mut actual[..n]) {
            consecutive += 1;
            if consecutive >= MAX_CONSECUTIVE_IO_ERRORS {
                return Err(Error::io(
                    format!("The drive stopped responding at offset {}", range.start),
                    e,
                ));
            }
            report.io_errors += 1;
            report.add_bad(range);
            progress(size + offset);
            continue;
        }
        consecutive = 0;
        let first = range.start / block as u64;
        pattern.fill(first, &mut expected[..n]);
        if actual[..n] != expected[..n] {
            let pairs = actual[..n].chunks(block).zip(expected[..n].chunks(block));
            for (i, (got, want)) in pairs.enumerate() {
                if got == want {
                    continue;
                }
                let index = first + i as u64;
                let start = index * block as u64;
                report.add_bad(start..start + block as u64);
                // Is this, bit for bit, the pattern of another block?
                if let Some(other) = pattern.tagged_index(got) {
                    if other != index && other < blocks {
                        pattern.fill(other, &mut scratch);
                        if scratch == got {
                            report.aliased_blocks += 1;
                        }
                    }
                }
            }
        }
        progress(size + offset);
    }
    report.fake_capacity = report.aliased_blocks > 0;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockdev::{FileDevice, FileSource, MemDevice, MemSource};
    use std::io;
    use std::path::PathBuf;

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "bootforge-imaging-{name}-{}.img",
            std::process::id()
        ))
    }

    fn sample(len: usize) -> Vec<u8> {
        let mut s = 0x1234_5678u64;
        (0..len).map(|_| mix(&mut s) as u8).collect()
    }

    /// A counterfeit drive: reports `claimed` bytes but stores only `real`,
    /// wrapping addresses.
    struct WrappingDevice {
        real: Vec<u8>,
        claimed: u64,
    }

    impl BlockDevice for WrappingDevice {
        fn sector_size(&self) -> u32 {
            512
        }
        fn size(&self) -> u64 {
            self.claimed
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            let len = self.real.len() as u64;
            for (i, b) in buf.iter_mut().enumerate() {
                *b = self.real[((offset + i as u64) % len) as usize];
            }
            Ok(())
        }
        fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
            let len = self.real.len() as u64;
            for (i, b) in buf.iter().enumerate() {
                self.real[((offset + i as u64) % len) as usize] = *b;
            }
            Ok(())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A device whose I/O fails inside a byte range (or everywhere).
    struct FailingDevice {
        inner: MemDevice,
        fail_reads: Range<u64>,
        fail_writes: Range<u64>,
    }

    impl FailingDevice {
        fn hits(range: &Range<u64>, offset: u64, len: usize) -> bool {
            offset < range.end && offset + len as u64 > range.start
        }
    }

    impl BlockDevice for FailingDevice {
        fn sector_size(&self) -> u32 {
            self.inner.sector_size()
        }
        fn size(&self) -> u64 {
            self.inner.size()
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            if Self::hits(&self.fail_reads, offset, buf.len()) {
                return Err(io::Error::other("simulated read error"));
            }
            self.inner.read_at(offset, buf)
        }
        fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
            if Self::hits(&self.fail_writes, offset, buf.len()) {
                return Err(io::Error::other("simulated write error"));
            }
            self.inner.write_at(offset, buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn chunk_is_a_sector_multiple() {
        assert_eq!(chunk_len(512), 1 << 20);
        assert_eq!(chunk_len(4096), 1 << 20);
        assert_eq!(chunk_len(520), (1 << 20) - (1 << 20) % 520);
        assert_eq!(chunk_len(2 << 20), 2 << 20);
    }

    #[test]
    fn write_pads_the_last_sector_and_verifies() {
        // 2 MiB + 700 bytes: two full chunks and a partial sector.
        let image = sample(2 * MIB as usize + 700);
        let mut dev = MemDevice::new(4 * MIB, 512);
        dev.data.fill(0xEE);
        let mut seen = Vec::new();
        let digest = write_image(
            &mut MemSource(image.clone()),
            &mut dev,
            &Cancel::new(),
            &mut |n| seen.push(n),
        )
        .unwrap();
        assert_eq!(digest.len, 2 * MIB + 1024);
        assert_eq!(digest.chunk_size as u64, MIB);
        assert_eq!(digest.chunk_crcs.len(), 3);
        assert_eq!(seen, vec![MIB, 2 * MIB, image.len() as u64]);
        assert_eq!(&dev.data[..image.len()], &image[..]);
        // Padding is zero, the rest of the device is untouched.
        assert!(dev.data[image.len()..digest.len as usize]
            .iter()
            .all(|b| *b == 0));
        assert!(dev.data[digest.len as usize..].iter().all(|b| *b == 0xEE));
        assert_eq!(dev.writes, 3);
        let mut padded = image.clone();
        padded.resize(digest.len as usize, 0);
        assert_eq!(digest.sha256, hex(&Sha256::digest(&padded)));

        let mut seen = Vec::new();
        verify_image(&mut dev, &digest, &Cancel::new(), &mut |n| seen.push(n)).unwrap();
        assert_eq!(seen, vec![MIB, 2 * MIB, digest.len]);
    }

    #[test]
    fn write_fills_a_device_exactly_and_handles_4k_sectors() {
        let image = sample(MIB as usize + 4096 + 1);
        let mut dev = MemDevice::new(MIB + 8192, 4096);
        let digest = write_image(
            &mut MemSource(image.clone()),
            &mut dev,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(digest.len, MIB + 8192);
        assert_eq!(&dev.data[..image.len()], &image[..]);
        verify_image(&mut dev, &digest, &Cancel::new(), &mut |_| {}).unwrap();
        // An empty image writes nothing and verifies trivially.
        let mut dev = MemDevice::new(MIB, 512);
        let digest = write_image(
            &mut MemSource(Vec::new()),
            &mut dev,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!((digest.len, dev.writes), (0, 0));
        verify_image(&mut dev, &digest, &Cancel::new(), &mut |_| {}).unwrap();
    }

    #[test]
    fn image_larger_than_the_device_is_refused_before_writing() {
        let mut dev = MemDevice::new(MIB, 512);
        let err = write_image(
            &mut MemSource(vec![1u8; MIB as usize + 1]),
            &mut dev,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{err:?}");
        assert!(err.to_string().contains("larger than the drive"), "{err}");
        assert_eq!(dev.writes, 0);
    }

    #[test]
    fn write_is_cancelled_promptly() {
        let cancel = Cancel::new();
        let mut dev = MemDevice::new(8 * MIB, 512);
        let c = cancel.clone();
        let err = write_image(
            &mut MemSource(sample(8 * MIB as usize)),
            &mut dev,
            &cancel,
            &mut |n| {
                if n >= 2 * MIB {
                    c.cancel();
                }
            },
        )
        .unwrap_err();
        assert!(err.is_cancelled());
        assert_eq!(dev.writes, 2);
        // Already cancelled: nothing is written at all.
        let mut dev = MemDevice::new(MIB, 512);
        assert!(
            write_image(&mut MemSource(vec![1; 512]), &mut dev, &cancel, &mut |_| {})
                .unwrap_err()
                .is_cancelled()
        );
        assert_eq!(dev.writes, 0);
    }

    #[test]
    fn verify_reports_the_first_differing_chunk() {
        let image = sample(3 * MIB as usize);
        let mut dev = MemDevice::new(4 * MIB, 512);
        let digest =
            write_image(&mut MemSource(image), &mut dev, &Cancel::new(), &mut |_| {}).unwrap();
        dev.data[MIB as usize + 5000] ^= 1;
        dev.data[2 * MIB as usize + 9] ^= 1;
        let err = verify_image(&mut dev, &digest, &Cancel::new(), &mut |_| {}).unwrap_err();
        let text = err.to_string();
        assert!(matches!(err, Error::Msg(_)));
        assert!(text.contains(&format!("offset {MIB} ")), "{text}");

        // A corrupting device is caught too.
        let mut bad = MemDevice::new(4 * MIB, 512);
        bad.bad_ranges.push(4096..4097);
        let digest = write_image(
            &mut MemSource(sample(MIB as usize)),
            &mut bad,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        let err = verify_image(&mut bad, &digest, &Cancel::new(), &mut |_| {}).unwrap_err();
        assert!(err.to_string().contains("offset 0 "), "{err}");

        // A digest that cannot belong to the device is rejected.
        let mut small = MemDevice::new(MIB, 512);
        let big = WriteDigest {
            len: 2 * MIB,
            chunk_size: MIB as u32,
            sha256: String::new(),
            chunk_crcs: vec![0, 0],
        };
        assert!(matches!(
            verify_image(&mut small, &big, &Cancel::new(), &mut |_| {}),
            Err(Error::Invalid(_))
        ));
        // Cancelling.
        let cancel = Cancel::new();
        cancel.cancel();
        assert!(verify_image(&mut dev, &digest, &cancel, &mut |_| {})
            .unwrap_err()
            .is_cancelled());
    }

    #[test]
    fn verify_checks_the_overall_hash() {
        let mut dev = MemDevice::new(MIB, 512);
        let mut digest = write_image(
            &mut MemSource(sample(4096)),
            &mut dev,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        digest.sha256 = "00".repeat(32);
        let err = verify_image(&mut dev, &digest, &Cancel::new(), &mut |_| {}).unwrap_err();
        assert!(err.to_string().contains("SHA-256"), "{err}");
    }

    #[test]
    fn file_device_write_backup_restore_round_trip() {
        let (dev_path, backup_path, restored_path) =
            (temp("dev"), temp("backup"), temp("restored"));
        let image = sample(MIB as usize + 1234);
        let mut dev = FileDevice::create(&dev_path, 3 * MIB, 512).unwrap();
        let digest = write_image(
            &mut MemSource(image.clone()),
            &mut dev,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        verify_image(&mut dev, &digest, &Cancel::new(), &mut |_| {}).unwrap();

        let mut last = 0;
        let sums =
            backup_to_file(&mut dev, &backup_path, &Cancel::new(), &mut |n| last = n).unwrap();
        assert_eq!(last, 3 * MIB);
        let saved = std::fs::read(&backup_path).unwrap();
        assert_eq!(saved.len() as u64, 3 * MIB);
        assert_eq!(&saved[..image.len()], &image[..]);
        let mut h = MultiHasher::new();
        h.update(&saved);
        assert_eq!(sums, h.finish());

        // Restore = write_image from the backup file.
        let mut restored = FileDevice::create(&restored_path, 3 * MIB, 512).unwrap();
        let mut src = FileSource::open(&backup_path).unwrap();
        let digest = write_image(&mut src, &mut restored, &Cancel::new(), &mut |_| {}).unwrap();
        verify_image(&mut restored, &digest, &Cancel::new(), &mut |_| {}).unwrap();
        drop((dev, restored, src));
        assert_eq!(std::fs::read(&restored_path).unwrap(), saved);
        for p in [dev_path, backup_path, restored_path] {
            std::fs::remove_file(p).unwrap();
        }
    }

    #[test]
    fn failed_or_cancelled_backup_leaves_no_file() {
        let path = temp("partial");
        let cancel = Cancel::new();
        let c = cancel.clone();
        let mut dev = MemDevice::new(4 * MIB, 512);
        let err = backup_to_file(&mut dev, &path, &cancel, &mut |_| c.cancel()).unwrap_err();
        assert!(err.is_cancelled());
        assert!(!path.exists());

        let mut dev = FailingDevice {
            inner: MemDevice::new(4 * MIB, 512),
            fail_reads: 2 * MIB..2 * MIB + 1,
            fail_writes: 0..0,
        };
        let err = backup_to_file(&mut dev, &path, &Cancel::new(), &mut |_| {}).unwrap_err();
        assert!(matches!(err, Error::Io(..)), "{err:?}");
        assert!(!path.exists());
    }

    #[test]
    fn zero_fill_and_erase_signatures() {
        let mut dev = MemDevice::new(4 * MIB, 512);
        dev.data.fill(0xAA);
        let mut last = 0;
        zero_fill(&mut dev, 1024, 2 * MIB, &Cancel::new(), &mut |n| last = n).unwrap();
        assert_eq!(last, 2 * MIB);
        assert!(dev.data[..1024].iter().all(|b| *b == 0xAA));
        assert!(dev.data[1024..1024 + 2 * MIB as usize]
            .iter()
            .all(|b| *b == 0));
        assert!(dev.data[1024 + 2 * MIB as usize..]
            .iter()
            .all(|b| *b == 0xAA));
        // Misaligned or out of range requests are refused without writing.
        let before = dev.writes;
        for (off, len) in [(1, 512), (0, 100), (4 * MIB, 512), (u64::MAX - 511, 1024)] {
            assert!(matches!(
                zero_fill(&mut dev, off, len, &Cancel::new(), &mut |_| {}),
                Err(Error::Invalid(_))
            ));
        }
        zero_fill(&mut dev, 0, 0, &Cancel::new(), &mut |_| {}).unwrap();
        assert_eq!(dev.writes, before);
        let cancel = Cancel::new();
        cancel.cancel();
        assert!(zero_fill(&mut dev, 0, 512, &cancel, &mut |_| {})
            .unwrap_err()
            .is_cancelled());

        dev.data.fill(0xAA);
        erase_signatures(&mut dev).unwrap();
        let m = MIB as usize;
        assert!(dev.data[..m].iter().all(|b| *b == 0));
        assert!(dev.data[m..3 * m].iter().all(|b| *b == 0xAA));
        assert!(dev.data[3 * m..].iter().all(|b| *b == 0));

        // Devices smaller than two MiB are wiped completely, without overlap errors.
        for size in [512u64, 4096, MIB, MIB + 512, 2 * MIB - 512] {
            let mut small = MemDevice::new(size, 512);
            small.data.fill(0xAA);
            erase_signatures(&mut small).unwrap();
            assert!(small.data.iter().all(|b| *b == 0), "size {size}");
        }
    }

    #[test]
    fn pattern_blocks_are_distinct_and_seed_dependent() {
        let p = Pattern::new(7, 512);
        let mut a = vec![0u8; 1024];
        p.fill(10, &mut a);
        assert_ne!(a[..512], a[512..]);
        assert_eq!(p.tagged_index(&a[..512]), Some(10));
        assert_eq!(p.tagged_index(&a[512..]), Some(11));
        let mut b = vec![0u8; 512];
        p.fill(11, &mut b);
        assert_eq!(a[512..], b[..]);
        Pattern::new(8, 512).fill(11, &mut b);
        assert_ne!(a[512..], b[..]);
        assert_ne!(run_seed(), 0);
    }

    #[test]
    fn healthy_device_passes_and_ends_zeroed() {
        let mut dev = MemDevice::new(3 * MIB + 4096, 512);
        dev.data.fill(0x55);
        let mut seen = Vec::new();
        let report = bad_block_check(&mut dev, &Cancel::new(), &mut |n| seen.push(n)).unwrap();
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(report.device_size, dev.size());
        assert!(report.summary().starts_with("No bad blocks"));
        assert_eq!(seen.last().copied(), Some(2 * dev.size()));
        assert!(seen.windows(2).all(|w| w[0] < w[1]));
        let m = MIB as usize;
        assert!(dev.data[..m].iter().all(|b| *b == 0));
        assert!(dev.data[dev.data.len() - m..].iter().all(|b| *b == 0));
        // The middle still holds the pattern: it really was written.
        assert!(dev.data[m..2 * m].iter().any(|b| *b != 0x55));
    }

    #[test]
    fn bad_ranges_are_found_and_merged() {
        let mut dev = MemDevice::new(4 * MIB, 512);
        // One byte; a run crossing a sector and a chunk boundary; two adjacent sectors.
        dev.bad_ranges.push(1500..1501);
        dev.bad_ranges.push(MIB - 10..MIB + 10);
        dev.bad_ranges.push(2 * MIB..2 * MIB + 512);
        dev.bad_ranges.push(2 * MIB + 512..2 * MIB + 1024);
        let report = bad_block_check_seeded(&mut dev, 99, &Cancel::new(), &mut |_| {}).unwrap();
        assert!(!report.is_clean());
        assert!(!report.fake_capacity);
        assert_eq!(
            report.bad_ranges,
            vec![1024..1536, MIB - 512..MIB + 512, 2 * MIB..2 * MIB + 1024]
        );
        assert_eq!(report.bad_bytes, 512 + 1024 + 1024);
        assert_eq!((report.truncated, report.io_errors), (false, 0));
        assert!(
            report.summary().contains("3 area(s)"),
            "{}",
            report.summary()
        );
    }

    #[test]
    fn bad_range_list_is_capped() {
        let mut dev = MemDevice::new(2 * MIB, 512);
        // Every other sector is bad: far more ranges than the cap.
        for i in 0..1000u64 {
            dev.bad_ranges.push(i * 1024..i * 1024 + 1);
        }
        let report = bad_block_check_seeded(&mut dev, 5, &Cancel::new(), &mut |_| {}).unwrap();
        assert_eq!(report.bad_ranges.len(), MAX_BAD_RANGES);
        assert!(report.truncated);
        assert_eq!(report.bad_bytes, 1000 * 512);
        assert!(report.summary().contains("more than 256"));
    }

    #[test]
    fn wrapping_fake_capacity_is_detected() {
        let mut dev = WrappingDevice {
            real: vec![0; 2 * MIB as usize],
            claimed: 8 * MIB,
        };
        let report = bad_block_check_seeded(&mut dev, 1234, &Cancel::new(), &mut |_| {}).unwrap();
        assert!(report.fake_capacity);
        // Only the last copy written survives: the first 6 MiB read back wrong.
        assert_eq!(report.bad_ranges, vec![0..6 * MIB]);
        assert_eq!(report.bad_bytes, 6 * MIB);
        assert_eq!(report.aliased_blocks, 6 * MIB / 512);
        assert!(report.summary().contains("counterfeit"));
    }

    #[test]
    fn io_errors_mark_chunks_bad_but_a_dead_device_aborts() {
        let mut dev = FailingDevice {
            inner: MemDevice::new(6 * MIB, 512),
            fail_reads: 4 * MIB + 100..4 * MIB + 200,
            fail_writes: MIB..MIB + 1,
        };
        let report = bad_block_check_seeded(&mut dev, 3, &Cancel::new(), &mut |_| {}).unwrap();
        assert_eq!(report.io_errors, 2);
        assert_eq!(report.bad_ranges, vec![MIB..2 * MIB, 4 * MIB..5 * MIB]);
        assert!(!report.is_clean());

        let mut dead = FailingDevice {
            inner: MemDevice::new(64 * MIB, 512),
            fail_reads: 0..0,
            fail_writes: 0..u64::MAX,
        };
        let err = bad_block_check_seeded(&mut dead, 3, &Cancel::new(), &mut |_| {}).unwrap_err();
        assert!(matches!(err, Error::Io(..)), "{err:?}");
        assert_eq!(dead.inner.writes, 0);
    }

    #[test]
    fn cancelled_check_stops_and_clears_the_signature_areas() {
        let cancel = Cancel::new();
        let c = cancel.clone();
        let mut dev = MemDevice::new(8 * MIB, 512);
        let mut calls = 0;
        let err = bad_block_check(&mut dev, &cancel, &mut |_| {
            calls += 1;
            if calls == 3 {
                c.cancel();
            }
        })
        .unwrap_err();
        assert!(err.is_cancelled());
        assert_eq!(calls, 3);
        let m = MIB as usize;
        assert!(dev.data[..m].iter().all(|b| *b == 0));
        assert!(dev.data[m..3 * m].iter().any(|b| *b != 0));
        // Nothing beyond the third chunk was touched.
        assert!(dev.data[3 * m..].iter().all(|b| *b == 0));
    }
}
