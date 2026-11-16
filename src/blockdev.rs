//! The block-device abstraction everything destructive goes through.
//!
//! The engine never talks to a disk directly: it is handed a `BlockDevice`.
//! On a real machine that is a locked physical drive (`device::raw`); in tests,
//! the self-test and all development work it is an image file (`FileDevice`)
//! or a buffer in memory (`MemDevice`).

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// A device made of fixed-size sectors.
///
/// `offset` and the buffer length must be multiples of `sector_size()` and the
/// range must lie inside `size()`; implementations return `InvalidInput`
/// otherwise instead of silently extending the device.
pub trait BlockDevice: Send {
    /// Logical sector size in bytes (512 or 4096).
    fn sector_size(&self) -> u32;
    /// Total size in bytes.
    fn size(&self) -> u64;
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
}

/// Random-access, read-only source of bytes (an ISO or image file).
/// No alignment requirements.
pub trait ReadAt: Send {
    fn size(&self) -> u64;
    /// Fills `buf` completely or fails (`UnexpectedEof` past the end).
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
}

fn check_range(sector: u32, size: u64, offset: u64, len: usize) -> io::Result<()> {
    let s = sector as u64;
    let len = len as u64;
    if !offset.is_multiple_of(s) || !len.is_multiple_of(s) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unaligned device access (offset {offset}, length {len})"),
        ));
    }
    match offset.checked_add(len) {
        Some(end) if end <= size => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("device access past the end (offset {offset}, length {len}, size {size})"),
        )),
    }
}

/// An image file used as a device.
pub struct FileDevice {
    file: File,
    size: u64,
    sector: u32,
}

impl FileDevice {
    /// Creates (or truncates) an image file of `size` bytes, all zero.
    pub fn create(path: &Path, size: u64, sector: u32) -> io::Result<FileDevice> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(size - size % sector as u64)?;
        Ok(FileDevice {
            file,
            size: size - size % sector as u64,
            sector,
        })
    }

    /// Opens an existing image file read-write. A trailing partial sector is ignored.
    pub fn open(path: &Path, sector: u32) -> io::Result<FileDevice> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let len = file.metadata()?.len();
        Ok(FileDevice {
            file,
            size: len - len % sector as u64,
            sector,
        })
    }
}

impl BlockDevice for FileDevice {
    fn sector_size(&self) -> u32 {
        self.sector
    }
    fn size(&self) -> u64 {
        self.size
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        check_range(self.sector, self.size, offset, buf.len())?;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        check_range(self.sector, self.size, offset, buf.len())?;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()?;
        self.file.sync_data()
    }
}

/// A device held in memory. `fail_writes_at` lets tests simulate a bad region.
pub struct MemDevice {
    pub data: Vec<u8>,
    sector: u32,
    /// Byte ranges whose writes are silently corrupted (for bad-block tests).
    pub bad_ranges: Vec<std::ops::Range<u64>>,
    /// Number of write calls so far (tests use it to prove nothing was written).
    pub writes: u64,
}

impl MemDevice {
    pub fn new(size: u64, sector: u32) -> MemDevice {
        MemDevice {
            data: vec![0; (size - size % sector as u64) as usize],
            sector,
            bad_ranges: Vec::new(),
            writes: 0,
        }
    }
}

impl BlockDevice for MemDevice {
    fn sector_size(&self) -> u32 {
        self.sector
    }
    fn size(&self) -> u64 {
        self.data.len() as u64
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        check_range(self.sector, self.data.len() as u64, offset, buf.len())?;
        buf.copy_from_slice(&self.data[offset as usize..offset as usize + buf.len()]);
        Ok(())
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        check_range(self.sector, self.data.len() as u64, offset, buf.len())?;
        self.writes += 1;
        let start = offset as usize;
        self.data[start..start + buf.len()].copy_from_slice(buf);
        for r in &self.bad_ranges {
            let (a, b) = (r.start.max(offset), r.end.min(offset + buf.len() as u64));
            for i in a..b {
                self.data[i as usize] ^= 0xff;
            }
        }
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A window (one partition) of a larger device.
pub struct SubDevice<'a> {
    inner: &'a mut dyn BlockDevice,
    start: u64,
    len: u64,
}

impl<'a> SubDevice<'a> {
    /// `start` and `len` are in bytes and must be sector-aligned and inside the device.
    pub fn new(inner: &'a mut dyn BlockDevice, start: u64, len: u64) -> io::Result<SubDevice<'a>> {
        check_range(inner.sector_size(), inner.size(), start, 0)?;
        let s = inner.sector_size() as u64;
        if !len.is_multiple_of(s)
            || start
                .checked_add(len)
                .map(|e| e > inner.size())
                .unwrap_or(true)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "partition window outside the device",
            ));
        }
        Ok(SubDevice { inner, start, len })
    }
    pub fn start(&self) -> u64 {
        self.start
    }
}

impl BlockDevice for SubDevice<'_> {
    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }
    fn size(&self) -> u64 {
        self.len
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        check_range(self.inner.sector_size(), self.len, offset, buf.len())?;
        self.inner.read_at(self.start + offset, buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        check_range(self.inner.sector_size(), self.len, offset, buf.len())?;
        self.inner.write_at(self.start + offset, buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// A file as a `ReadAt` source.
pub struct FileSource {
    file: File,
    size: u64,
}

impl FileSource {
    pub fn open(path: &Path) -> io::Result<FileSource> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        Ok(FileSource { file, size })
    }
}

impl ReadAt for FileSource {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(buf)
    }
}

/// Bytes in memory as a `ReadAt` source (tests, synthetic images).
pub struct MemSource(pub Vec<u8>);

impl ReadAt for MemSource {
    fn size(&self) -> u64 {
        self.0.len() as u64
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .filter(|e| *e <= self.0.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "read past the end"))?;
        buf.copy_from_slice(&self.0[offset as usize..end as usize]);
        Ok(())
    }
}

/// Reads a block device as a `ReadAt` source, hiding the alignment rules
/// (used to parse file systems back from a device or an image).
pub struct DeviceSource<'a> {
    dev: &'a mut dyn BlockDevice,
}

impl<'a> DeviceSource<'a> {
    pub fn new(dev: &'a mut dyn BlockDevice) -> DeviceSource<'a> {
        DeviceSource { dev }
    }
}

impl ReadAt for DeviceSource<'_> {
    fn size(&self) -> u64 {
        self.dev.size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let s = self.dev.sector_size() as u64;
        let first = offset - offset % s;
        let end = offset + buf.len() as u64;
        let last = end.div_ceil(s) * s;
        if last > self.dev.size() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read past the end",
            ));
        }
        let mut tmp = vec![0u8; (last - first) as usize];
        self.dev.read_at(first, &mut tmp)?;
        let skip = (offset - first) as usize;
        buf.copy_from_slice(&tmp[skip..skip + buf.len()]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_device_rejects_unaligned_and_out_of_range() {
        let mut d = MemDevice::new(4096, 512);
        let mut b = [0u8; 512];
        assert!(d.read_at(1, &mut b).is_err());
        assert!(d.read_at(4096, &mut b).is_err());
        assert!(d.write_at(3584, &[1u8; 1024]).is_err());
        assert!(d.write_at(3584, &[1u8; 512]).is_ok());
        assert_eq!(d.writes, 1);
        let mut small = [0u8; 100];
        assert!(d.read_at(0, &mut small).is_err());
    }

    #[test]
    fn sub_device_is_a_window() {
        let mut d = MemDevice::new(8192, 512);
        {
            let mut s = SubDevice::new(&mut d, 1024, 2048).unwrap();
            assert_eq!(s.size(), 2048);
            s.write_at(0, &[7u8; 512]).unwrap();
            assert!(s.write_at(2048, &[7u8; 512]).is_err());
        }
        assert_eq!(d.data[1024], 7);
        assert_eq!(d.data[1023], 0);
        assert!(SubDevice::new(&mut d, 4096, 8192).is_err());
    }

    #[test]
    fn device_source_reads_unaligned() {
        let mut d = MemDevice::new(2048, 512);
        for (i, b) in d.data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let mut s = DeviceSource::new(&mut d);
        let mut buf = [0u8; 700];
        s.read_at(300, &mut buf).unwrap();
        assert_eq!(buf[0], (300 % 251) as u8);
        assert_eq!(buf[699], (999 % 251) as u8);
        assert!(s.read_at(2000, &mut buf).is_err());
    }

    #[test]
    fn file_device_round_trip() {
        let path =
            std::env::temp_dir().join(format!("bootforge-blockdev-{}.img", std::process::id()));
        {
            let mut d = FileDevice::create(&path, 1024 * 1024 + 100, 512).unwrap();
            assert_eq!(d.size(), 1024 * 1024);
            d.write_at(512, &[9u8; 512]).unwrap();
            d.flush().unwrap();
        }
        let mut d = FileDevice::open(&path, 512).unwrap();
        let mut b = [0u8; 1024];
        d.read_at(0, &mut b).unwrap();
        assert_eq!((b[0], b[512], b[1023]), (0, 9, 9));
        drop(d);
        std::fs::remove_file(&path).unwrap();
    }
}
