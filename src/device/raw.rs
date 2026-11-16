//! Raw access to a physical drive: the only place that writes to real disks.
//!
//! The order of operations in [`RawDisk::open_for_write`] is the safety
//! argument, so it is spelled out:
//!
//! 1. The caller must hold a [`Clearance`], which only `safety::clear` can
//!    create after every rule passed on a fresh enumeration.
//! 2. `\\.\PhysicalDriveN` is opened and, before anything else happens, the
//!    size, sector size, model and serial number are read back *from that
//!    very handle* and compared with the clearance. If the stick was swapped
//!    between the confirmation and this moment, the handle is closed and
//!    nothing has been touched.
//! 3. Every volume with an extent on the disk is locked and dismounted, so no
//!    file system is mounted on the sectors about to be overwritten. Volumes
//!    Windows or this program run from are refused outright, whatever the
//!    clearance says.
//! 4. The locks are RAII guards. If any step fails, the guards taken so far
//!    unlock and close on the way out; a `RawDisk` that is dropped without
//!    [`RawDisk::finish`] does the same. There is no path that leaves a
//!    volume locked.
//!
//! Windows keeps its old view of the partition table while the volume handles
//! are held, which is what allows writing the whole disk including a new
//! partition table; `finish` asks it to re-read the table at the very end.

use super::assemble::{tidy_serial, tidy_text, VolumeFacts};
use super::collect::{collect_system_facts, collect_volumes, query_device_strings, query_geometry};
use super::eject::eject_disk;
use super::win::{ioctl, open_device, physical_drive_path, win_err, Handle};
use crate::blockdev::BlockDevice;
use crate::error::{Error, Result};
use crate::model::DiskIdentity;
use crate::safety::{Clearance, Refusal};
use crate::util::human_size;
use std::alloc::{alloc_zeroed, dealloc, handle_alloc_error, Layout};
use std::io;
use std::ptr::NonNull;
use std::time::Duration;
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    FlushFileBuffers, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_NO_BUFFERING,
    FILE_FLAG_WRITE_THROUGH,
};
use windows::Win32::System::Ioctl::{
    FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, FSCTL_UNLOCK_VOLUME, IOCTL_DISK_UPDATE_PROPERTIES,
};
use windows::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};

/// Alignment of I/O buffers. Unbuffered I/O needs buffers aligned to the
/// physical sector size; a page covers every sector size in use.
const BUFFER_ALIGN: usize = 4096;

/// Largest single transfer, and the size of the bounce buffer.
const MAX_TRANSFER: usize = 1024 * 1024;

/// How often and how long to wait for a busy volume before giving up
/// (20 x 200 ms = 4 seconds).
const LOCK_ATTEMPTS: u32 = 20;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(200);

// --------------------------------------------------------------------------------------
// Pure helpers (unit-tested)
// --------------------------------------------------------------------------------------

/// A zeroed heap buffer whose address is a multiple of `align`.
pub(crate) struct AlignedBuf {
    ptr: NonNull<u8>,
    layout: Layout,
}

// SAFETY: the buffer is uniquely owned heap memory with no thread affinity.
unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    /// `len` must be non-zero and `align` a power of two.
    pub(crate) fn new(len: usize, align: usize) -> AlignedBuf {
        let layout = Layout::from_size_align(len.max(1), align).expect("valid buffer layout");
        // SAFETY: the layout has a non-zero size.
        let raw = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).unwrap_or_else(|| handle_alloc_error(layout));
        AlignedBuf { ptr, layout }
    }

    pub(crate) fn len(&self) -> usize {
        self.layout.size()
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` points to `len` initialised (zeroed) bytes owned by self.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with exactly this layout.
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

fn is_aligned(ptr: *const u8) -> bool {
    (ptr as usize).is_multiple_of(BUFFER_ALIGN)
}

/// The transfer size for a sector size: at most `MAX_TRANSFER`, a whole
/// number of sectors, at least one sector.
pub(crate) fn transfer_len(sector: u32) -> usize {
    let s = sector.max(1) as usize;
    (MAX_TRANSFER - MAX_TRANSFER % s).max(s)
}

/// The `BlockDevice` contract: sector-aligned and inside the device.
pub(crate) fn check_io_range(sector: u32, size: u64, offset: u64, len: usize) -> io::Result<()> {
    let s = sector.max(1) as u64;
    let len = len as u64;
    if !offset.is_multiple_of(s) || !len.is_multiple_of(s) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unaligned disk access (offset {offset}, length {len})"),
        ));
    }
    match offset.checked_add(len) {
        Some(end) if end <= size => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("disk access past the end (offset {offset}, length {len}, size {size})"),
        )),
    }
}

/// Compares what the opened device reports with what the user confirmed.
/// `None` when it is the same device, otherwise what differs. The strings
/// are raw (as returned by the device) and are tidied here exactly as the
/// enumeration tidies them.
pub(crate) fn identity_difference(
    expected: &DiskIdentity,
    expected_sector: u32,
    raw_model: &str,
    raw_serial: &str,
    size: u64,
    sector: u32,
) -> Option<String> {
    if size != expected.size {
        return Some(format!(
            "size was {}, is now {}",
            human_size(expected.size),
            human_size(size)
        ));
    }
    if sector != expected_sector {
        return Some("different sector size".into());
    }
    if tidy_serial(raw_serial) != expected.serial {
        return Some("different serial number".into());
    }
    if tidy_text(raw_model) != expected.model {
        return Some("different model".into());
    }
    None
}

/// The volumes that have at least one extent on disk `number`.
pub(crate) fn volumes_on_disk(volumes: &[VolumeFacts], number: u32) -> Vec<&VolumeFacts> {
    volumes
        .iter()
        .filter(|v| v.disks().contains(&number))
        .collect()
}

/// The first of `targets` that is one of the `protected` volumes (compared by
/// volume GUID path).
pub(crate) fn protected_volume<'a>(
    targets: &[&'a VolumeFacts],
    protected: &[Option<String>],
) -> Option<&'a VolumeFacts> {
    targets.iter().copied().find(|v| {
        protected
            .iter()
            .flatten()
            .any(|p| p.eq_ignore_ascii_case(&v.guid_path))
    })
}

/// "drive E:" or, for a volume without a letter, "a volume without a drive letter".
pub(crate) fn volume_name(letter: Option<char>) -> String {
    match letter {
        Some(l) => format!("drive {}:", l.to_ascii_uppercase()),
        None => "a volume without a drive letter".to_string(),
    }
}

pub(crate) fn lock_failure_message(letter: Option<char>, error: &io::Error) -> String {
    format!(
        "Could not get exclusive access to {} on the target drive: another program is using it \
         ({error}). Close any Explorer window, document or tool that has it open and try again.",
        volume_name(letter)
    )
}

pub(crate) fn open_failure(number: u32, write: bool, e: io::Error) -> Error {
    if e.kind() == io::ErrorKind::PermissionDenied {
        Error::msg(format!(
            "Windows denied {} access to disk {number}. BootForge must run as administrator to {} a drive.",
            if write { "write" } else { "read" },
            if write { "write to" } else { "read" },
        ))
    } else {
        Error::io(format!("Could not open disk {number}"), e)
    }
}

// --------------------------------------------------------------------------------------
// Volume locks
// --------------------------------------------------------------------------------------

/// A volume that is locked for our exclusive use. Dropping it unlocks the
/// volume and closes the handle, in that order.
struct LockedVolume {
    handle: Handle,
}

impl Drop for LockedVolume {
    fn drop(&mut self) {
        // Closing the handle releases the lock as well; unlocking first makes
        // the intent explicit. Nothing useful can be done if it fails.
        let _ = ioctl(&self.handle, FSCTL_UNLOCK_VOLUME, None, None);
    }
}

/// Opens, locks and dismounts one volume.
fn lock_and_dismount(volume: &VolumeFacts) -> Result<LockedVolume> {
    let name = volume_name(volume.letter);
    // The volume device itself: the GUID path without its trailing slash.
    let handle = open_device(
        volume.guid_path.trim_end_matches('\\'),
        GENERIC_READ.0 | GENERIC_WRITE.0,
        FILE_FLAGS_AND_ATTRIBUTES(0),
    )
    .map_err(|e| Error::io(format!("Could not open {name} on the target drive"), e))?;

    // FSCTL_LOCK_VOLUME fails while any other handle to the volume is open.
    // Such handles are often short-lived (indexer, antivirus, Explorer
    // refreshing), so retry for a few seconds before blaming the user.
    let mut attempt = 1;
    loop {
        match ioctl(&handle, FSCTL_LOCK_VOLUME, None, None) {
            Ok(_) => break,
            Err(e) if attempt >= LOCK_ATTEMPTS => {
                // `handle` is dropped here: nothing was locked.
                return Err(Error::msg(lock_failure_message(volume.letter, &e)));
            }
            Err(_) => {
                attempt += 1;
                std::thread::sleep(LOCK_RETRY_DELAY);
            }
        }
    }
    // From here on the guard owns the lock: every exit path unlocks.
    let locked = LockedVolume { handle };
    ioctl(&locked.handle, FSCTL_DISMOUNT_VOLUME, None, None)
        .map_err(|e| Error::io(format!("Could not dismount {name} on the target drive"), e))?;
    Ok(locked)
}

// --------------------------------------------------------------------------------------
// RawDisk
// --------------------------------------------------------------------------------------

/// An open physical drive as a [`BlockDevice`].
///
/// Opened for writing ([`RawDisk::open_for_write`]) it holds every volume of
/// the drive locked and dismounted until it is dropped or finished. Opened
/// for reading ([`RawDisk::open_for_read`]) it locks nothing and refuses
/// writes.
///
/// I/O is unbuffered and write-through: offsets and lengths must be multiples
/// of the sector size (the `BlockDevice` contract); buffers of any alignment
/// are accepted and bounced through an aligned buffer when necessary.
pub struct RawDisk {
    // Field order is drop order: volumes are unlocked before the disk closes.
    volumes: Vec<LockedVolume>,
    disk: Handle,
    number: u32,
    size: u64,
    sector: u32,
    writable: bool,
    bounce: AlignedBuf,
}

impl RawDisk {
    /// Opens the cleared disk for writing, verifies that it still is the
    /// confirmed device and locks and dismounts all of its volumes.
    ///
    /// Errors: `Error::Refused` when the device behind the disk number is not
    /// the one in the clearance (or holds a volume Windows or BootForge runs
    /// from); a readable `Error::Msg` naming the drive letter when a volume
    /// stays in use; `Error::Io` otherwise. On any error nothing is left
    /// open or locked and nothing has been written.
    ///
    /// Requires an elevated process.
    pub fn open_for_write(clearance: &Clearance) -> Result<RawDisk> {
        let number = clearance.disk_number();

        // 1. Open the disk. Unbuffered + write-through: data goes straight to
        //    the device, so a successful write has really left the PC.
        let disk = open_device(
            &physical_drive_path(number),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH,
        )
        .map_err(|e| open_failure(number, true, e))?;

        // 2. Before anything else: is the device behind this handle still the
        //    one that was confirmed? Disk numbers are reused when sticks are
        //    swapped, so the number alone proves nothing.
        let changed = |what: &str| Error::from(Refusal::Changed(what.to_string()));
        let strings = query_device_strings(&disk)
            .map_err(|_| changed("its identity could not be read back"))?;
        let (size, sector) =
            query_geometry(&disk).map_err(|_| changed("its size could not be read back"))?;
        if let Some(what) = identity_difference(
            clearance.identity(),
            clearance.sector_size(),
            &strings.product,
            &strings.serial,
            size,
            sector,
        ) {
            return Err(Refusal::Changed(what).into());
        }
        if sector == 0 || size == 0 || size % sector as u64 != 0 {
            return Err(changed("it reports an unusable size"));
        }

        // 3. Find every volume on the disk, by volume GUID path (this also
        //    covers partitions without a drive letter).
        let all = collect_volumes(false);
        let targets = volumes_on_disk(&all, number);

        // Belt and braces: independent of the clearance, never lock or
        // dismount the volume Windows or this program runs from.
        let facts = collect_system_facts();
        let protected = [facts.windows_volume, facts.program_volume];
        if let Some(v) = protected_volume(&targets, &protected) {
            return Err(Error::Refused(format!(
                "{} on this disk is in use by Windows or by BootForge itself",
                volume_name(v.letter)
            )));
        }

        // 4. Lock and dismount them all. If one fails, `volumes` is dropped
        //    by the `?` and every lock taken so far is released.
        let mut volumes = Vec::with_capacity(targets.len());
        for v in targets {
            volumes.push(lock_and_dismount(v)?);
        }

        Ok(RawDisk {
            volumes,
            disk,
            number,
            size,
            sector,
            writable: true,
            bounce: AlignedBuf::new(transfer_len(sector), BUFFER_ALIGN),
        })
    }

    /// Opens a disk read-only (for drive backups). Nothing is locked or
    /// dismounted and `write_at` fails with `PermissionDenied`.
    ///
    /// Requires an elevated process.
    pub fn open_for_read(number: u32) -> Result<RawDisk> {
        let disk = open_device(
            &physical_drive_path(number),
            GENERIC_READ.0,
            FILE_FLAG_NO_BUFFERING,
        )
        .map_err(|e| open_failure(number, false, e))?;
        let (size, sector) = query_geometry(&disk).map_err(|e| {
            Error::io(
                format!("Could not read the size of disk {number} (no medium?)"),
                e,
            )
        })?;
        if sector == 0 {
            return Err(Error::invalid(format!(
                "Disk {number} reports a sector size of zero."
            )));
        }
        Ok(RawDisk {
            volumes: Vec::new(),
            disk,
            number,
            size: size - size % sector as u64,
            sector,
            writable: false,
            bounce: AlignedBuf::new(transfer_len(sector), BUFFER_ALIGN),
        })
    }

    /// N in `\\.\PhysicalDriveN`.
    pub fn number(&self) -> u32 {
        self.number
    }

    /// Finishes a write: flushes the disk, unlocks and closes the volumes,
    /// asks Windows to re-read the partition table and, when `eject` is set,
    /// safely removes the device.
    ///
    /// `Err` means the data may not have reached the drive (the flush
    /// failed). `Ok(Some(message))` means the write is complete but a
    /// follow-up step failed (Windows did not refresh the partition table, or
    /// the drive could not be ejected); the message is meant for the user.
    /// `Ok(None)` means everything succeeded.
    pub fn finish(mut self, eject: bool) -> Result<Option<String>> {
        // 1. Everything written must be on the device before the locks go.
        //    (On error `self` is dropped, which still unlocks and closes.)
        self.flush()
            .map_err(|e| Error::io("Could not flush the drive", e))?;

        // 2. Unlock and close the volume handles.
        self.volumes.clear();

        // 3. Only now tell Windows that the partition table changed, so that
        //    it drops its stale view and mounts the new volumes.
        let mut notes: Vec<String> = Vec::new();
        if self.writable {
            if let Err(e) = ioctl(&self.disk, IOCTL_DISK_UPDATE_PROPERTIES, None, None) {
                notes.push(format!(
                    "Windows could not re-read the partition table ({e}); unplug the drive and plug it in again."
                ));
            }
        }

        // 4. Close the disk itself; ejecting needs every handle gone.
        let number = self.number;
        drop(self);

        if eject {
            if let Err(why) = eject_disk(number) {
                notes.push(format!(
                    "The drive is ready but could not be ejected automatically ({why}). \
                     Use \"Safely Remove Hardware\" before unplugging it."
                ));
            }
        }
        Ok((!notes.is_empty()).then(|| notes.join(" ")))
    }
}

impl Drop for RawDisk {
    fn drop(&mut self) {
        // Explicit for the reader: unlock and close every volume first; the
        // disk handle and the buffer are released by their own destructors.
        self.volumes.clear();
    }
}

fn overlapped_at(offset: u64) -> OVERLAPPED {
    // For a synchronous handle the OVERLAPPED structure simply carries the
    // file offset of the transfer.
    OVERLAPPED {
        Anonymous: OVERLAPPED_0 {
            Anonymous: OVERLAPPED_0_0 {
                Offset: offset as u32,
                OffsetHigh: (offset >> 32) as u32,
            },
        },
        ..OVERLAPPED::default()
    }
}

fn read_exact_at(handle: HANDLE, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    let mut overlapped = overlapped_at(offset);
    let mut got = 0u32;
    let len = buf.len();
    // SAFETY: `buf` is valid for `len` bytes; the handle is synchronous, so
    // the call completes before `overlapped` and `got` go out of scope.
    unsafe { ReadFile(handle, Some(buf), Some(&mut got), Some(&mut overlapped)) }
        .map_err(win_err)?;
    if got as usize != len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("the drive returned {got} of {len} bytes at offset {offset}"),
        ));
    }
    Ok(())
}

fn write_all_at(handle: HANDLE, offset: u64, buf: &[u8]) -> io::Result<()> {
    let mut overlapped = overlapped_at(offset);
    let mut put = 0u32;
    // SAFETY: as in `read_exact_at`.
    unsafe { WriteFile(handle, Some(buf), Some(&mut put), Some(&mut overlapped)) }
        .map_err(win_err)?;
    if put as usize != buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!(
                "the drive accepted {put} of {} bytes at offset {offset}",
                buf.len()
            ),
        ));
    }
    Ok(())
}

impl BlockDevice for RawDisk {
    fn sector_size(&self) -> u32 {
        self.sector
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        check_io_range(self.sector, self.size, offset, buf.len())?;
        let step = self.bounce.len();
        let mut at = offset;
        // `step` is a whole number of sectors, so every piece stays aligned.
        for piece in buf.chunks_mut(step) {
            if is_aligned(piece.as_ptr()) {
                read_exact_at(self.disk.raw(), at, piece)?;
            } else {
                let bounce = &mut self.bounce.as_mut_slice()[..piece.len()];
                read_exact_at(self.disk.raw(), at, bounce)?;
                piece.copy_from_slice(bounce);
            }
            at += piece.len() as u64;
        }
        Ok(())
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the disk was opened read-only",
            ));
        }
        check_io_range(self.sector, self.size, offset, buf.len())?;
        let step = self.bounce.len();
        let mut at = offset;
        for piece in buf.chunks(step) {
            if is_aligned(piece.as_ptr()) {
                write_all_at(self.disk.raw(), at, piece)?;
            } else {
                let bounce = &mut self.bounce.as_mut_slice()[..piece.len()];
                bounce.copy_from_slice(piece);
                write_all_at(self.disk.raw(), at, bounce)?;
            }
            at += piece.len() as u64;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.writable {
            return Ok(());
        }
        // SAFETY: the handle is open for the lifetime of `self`.
        match unsafe { FlushFileBuffers(self.disk.raw()) }.map_err(win_err) {
            Ok(()) => Ok(()),
            // Some USB bridges do not implement the cache-flush command. The
            // handle is unbuffered and write-through, so every completed
            // write has already been handed to the device: not an error.
            Err(e) if matches!(e.raw_os_error(), Some(1) | Some(50)) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::assemble::Extent;
    use super::*;

    #[test]
    fn aligned_buffer_is_aligned_zeroed_and_sized() {
        for len in [512usize, 4096, 520 * 3, MAX_TRANSFER] {
            let mut b = AlignedBuf::new(len, BUFFER_ALIGN);
            assert_eq!(b.len(), len);
            assert!(is_aligned(b.as_mut_slice().as_ptr()));
            assert!(b.as_mut_slice().iter().all(|x| *x == 0));
            b.as_mut_slice()[len - 1] = 7;
            assert_eq!(b.as_mut_slice()[len - 1], 7);
        }
        // Several live at once do not overlap.
        let mut a = AlignedBuf::new(4096, BUFFER_ALIGN);
        let mut b = AlignedBuf::new(4096, BUFFER_ALIGN);
        a.as_mut_slice().fill(1);
        b.as_mut_slice().fill(2);
        assert!(a.as_mut_slice().iter().all(|x| *x == 1));
        assert!(!is_aligned(a.as_mut_slice()[1..].as_ptr()));
    }

    #[test]
    fn transfer_sizes() {
        assert_eq!(transfer_len(512), MAX_TRANSFER);
        assert_eq!(transfer_len(4096), MAX_TRANSFER);
        assert_eq!(transfer_len(520) % 520, 0);
        assert!(transfer_len(520) <= MAX_TRANSFER);
        assert_eq!(transfer_len(2 * MAX_TRANSFER as u32), 2 * MAX_TRANSFER);
    }

    #[test]
    fn io_ranges_follow_the_block_device_contract() {
        assert!(check_io_range(512, 4096, 0, 4096).is_ok());
        assert!(check_io_range(512, 4096, 3584, 512).is_ok());
        assert!(check_io_range(512, 4096, 0, 0).is_ok());
        for (offset, len) in [(1u64, 512usize), (0, 100), (4096, 512), (3584, 1024)] {
            let e = check_io_range(512, 4096, offset, len).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        }
        assert!(check_io_range(512, u64::MAX, u64::MAX - 511, 1024).is_err());
        assert!(check_io_range(4096, 1 << 20, 512, 4096).is_err());
    }

    fn identity() -> DiskIdentity {
        DiskIdentity {
            number: 2,
            serial: "KESTRL01".into(),
            model: "Flash 3.1".into(),
            size: 32_000_000_000,
        }
    }

    #[test]
    fn identity_must_match_exactly() {
        let id = identity();
        // Raw strings as the device reports them: padded, hex-encoded serial.
        assert_eq!(
            identity_difference(&id, 512, "Flash  3.1 ", "4B455354524C3031", id.size, 512),
            None
        );
        assert_eq!(
            identity_difference(&id, 512, "Flash 3.1", "KESTRL01", id.size, 512),
            None
        );
        let size = identity_difference(&id, 512, "Flash 3.1", "KESTRL01", id.size + 512, 512);
        assert!(size.unwrap().starts_with("size was"));
        assert_eq!(
            identity_difference(&id, 512, "Flash 3.1", "KESTRL02", id.size, 512).as_deref(),
            Some("different serial number")
        );
        assert_eq!(
            identity_difference(&id, 512, "Flash 3.1", "", id.size, 512).as_deref(),
            Some("different serial number")
        );
        assert_eq!(
            identity_difference(&id, 512, "Flash 3.2", "KESTRL01", id.size, 512).as_deref(),
            Some("different model")
        );
        assert_eq!(
            identity_difference(&id, 512, "Flash 3.1", "KESTRL01", id.size, 4096).as_deref(),
            Some("different sector size")
        );
        // The refusal reads well.
        let e: Error = Refusal::Changed("different serial number".into()).into();
        assert!(matches!(e, Error::Refused(_)));
        assert!(e.to_string().contains("nothing was written"));
    }

    fn vol(n: u32, letter: Option<char>, disks: &[u32]) -> VolumeFacts {
        VolumeFacts {
            guid_path: format!("\\\\?\\Volume{{0000000{n}-0000-0000-0000-000000000000}}\\"),
            letter,
            extents: disks
                .iter()
                .map(|&disk| Extent {
                    disk,
                    offset: 1 << 20,
                    length: 1 << 30,
                })
                .collect(),
            ..VolumeFacts::default()
        }
    }

    #[test]
    fn only_volumes_of_the_target_disk_are_selected() {
        let mut by_number = vol(4, None, &[]);
        by_number.device_number = Some((2, 1));
        let volumes = vec![
            vol(1, Some('C'), &[0]),
            vol(2, Some('E'), &[2]),
            vol(3, None, &[2]),
            by_number,
            vol(5, Some('S'), &[1, 2]),
            vol(6, Some('D'), &[1]),
        ];
        let picked = volumes_on_disk(&volumes, 2);
        let letters: Vec<Option<char>> = picked.iter().map(|v| v.letter).collect();
        assert_eq!(letters, vec![Some('E'), None, None, Some('S')]);
        assert!(volumes_on_disk(&volumes, 9).is_empty());

        // Protected volumes are spotted case-insensitively; `None` entries are ignored.
        let windows = Some(volumes[4].guid_path.to_uppercase());
        let hit = protected_volume(&picked, &[None, windows]).unwrap();
        assert_eq!(hit.letter, Some('S'));
        let elsewhere = Some(volumes[0].guid_path.clone());
        assert!(protected_volume(&picked, &[elsewhere, None]).is_none());
        assert!(protected_volume(&picked, &[]).is_none());
    }

    #[test]
    fn messages_name_the_drive() {
        let busy = io::Error::from_raw_os_error(5);
        let m = lock_failure_message(Some('e'), &busy);
        assert!(m.contains("drive E:"), "{m}");
        assert!(m.contains("another program is using it"));
        let m = lock_failure_message(None, &busy);
        assert!(m.contains("a volume without a drive letter"));

        let denied = open_failure(3, true, io::Error::from_raw_os_error(5));
        assert!(matches!(denied, Error::Msg(_)));
        assert!(denied.to_string().contains("administrator"));
        assert!(denied.to_string().contains("disk 3"));
        let gone = open_failure(3, false, io::Error::from_raw_os_error(2));
        assert!(matches!(gone, Error::Io(..)));
    }

    /// Read-only: on an unelevated machine opening is denied with a clear
    /// message; when elevated, one sector is read and writes are refused
    /// before reaching the system.
    #[test]
    fn read_only_open_never_writes() {
        let Some(number) = super::super::collect::disk_number_of_path(&std::env::temp_dir()) else {
            return;
        };
        match RawDisk::open_for_read(number) {
            Err(e) => assert!(e.to_string().contains("administrator"), "{e}"),
            Ok(mut disk) => {
                assert_eq!(disk.number(), number);
                assert!(disk.size() > 0 && disk.sector_size() >= 512);
                // An unaligned buffer exercises the bounce path.
                let mut buf = vec![0u8; disk.sector_size() as usize + 1];
                let sector = disk.sector_size() as usize;
                disk.read_at(0, &mut buf[1..=sector]).unwrap();
                let e = disk.write_at(0, &buf[..sector]).unwrap_err();
                assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
                assert!(disk.flush().is_ok());
                assert!(disk.read_at(1, &mut buf[..sector]).is_err());
            }
        }
    }

    #[test]
    fn a_missing_disk_cannot_be_opened() {
        // Disk numbers this high do not exist.
        assert!(RawDisk::open_for_read(9_999).is_err());
    }
}
