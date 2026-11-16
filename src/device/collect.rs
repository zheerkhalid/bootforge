//! The Win32 half of disk enumeration: gathers raw facts, decides nothing.
//!
//! Everything here is read-only. Disks and volumes are opened with desired
//! access 0, which permits device queries (IOCTLs declared `FILE_ANY_ACCESS`)
//! but no data access, needs no administrator rights and cannot disturb a
//! device that is in use.

use super::assemble::{
    parse_device_descriptor, parse_device_number, parse_disk_attributes, parse_extents,
    parse_geometry, parse_layout, DeviceStrings, DiskFacts, SystemFacts, VolumeFacts, LAYOUT_ENTRY,
    LAYOUT_HEADER,
};
use super::win::{
    from_wide, ioctl, ioctl_out, open_device, physical_drive_path, split_multi_sz, Handle,
};
use crate::sys::wide;
use std::io;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS};
use windows::Win32::Storage::FileSystem::{
    FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetDriveTypeW, GetVolumeInformationW,
    GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, GetVolumePathNamesForVolumeNameW,
    QueryDosDeviceW, FILE_FLAGS_AND_ATTRIBUTES,
};
use windows::Win32::System::Diagnostics::Debug::{
    SetThreadErrorMode, SEM_FAILCRITICALERRORS, THREAD_ERROR_MODE,
};
use windows::Win32::System::Ioctl::{
    IOCTL_DISK_GET_DISK_ATTRIBUTES, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
    IOCTL_DISK_GET_DRIVE_LAYOUT_EX, IOCTL_DISK_IS_WRITABLE, IOCTL_STORAGE_GET_DEVICE_NUMBER,
    IOCTL_STORAGE_QUERY_PROPERTY,
};
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_LOCAL_MACHINE, REG_ROUTINE_FLAGS, RRF_RT_REG_MULTI_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::System::SystemInformation::GetWindowsDirectoryW;

/// Highest `\\.\PhysicalDriveN` probed (exclusive).
const MAX_DISKS: u32 = 64;

/// How long label / file-system queries may take in total before the
/// remaining volumes are listed without them.
const LABEL_BUDGET: Duration = Duration::from_millis(2500);

/// `CTL_CODE(IOCTL_VOLUME_BASE ('V'), 0, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;

const ERROR_WRITE_PROTECT: i32 = 19;
const ERROR_INSUFFICIENT_BUFFER: i32 = 122;

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;
const DRIVE_CDROM: u32 = 5;
const DRIVE_RAMDISK: u32 = 6;

/// Entry placed in `SystemFacts::page_files` when the page-file list could
/// not be read. It has no drive letter, so the assembly treats it as doubt.
const UNREADABLE: &str = "<unreadable>";

/// While alive, the calling thread gets error codes instead of "There is no
/// disk in the drive" dialog boxes when it touches an empty card reader.
pub(crate) struct QuietErrors(Option<THREAD_ERROR_MODE>);

impl QuietErrors {
    pub(crate) fn new() -> QuietErrors {
        let mut old = THREAD_ERROR_MODE(0);
        // SAFETY: valid out-pointer; affects only the calling thread.
        let ok = unsafe { SetThreadErrorMode(SEM_FAILCRITICALERRORS, Some(&mut old)) }.is_ok();
        QuietErrors(ok.then_some(old))
    }
}

impl Drop for QuietErrors {
    fn drop(&mut self) {
        if let Some(old) = self.0 {
            // SAFETY: restores the mode saved in `new`.
            unsafe {
                let _ = SetThreadErrorMode(old, None);
            }
        }
    }
}

// --------------------------------------------------------------------------------------
// Disks
// --------------------------------------------------------------------------------------

/// Vendor, product, serial, bus and removable flag of an open disk.
pub(crate) fn query_device_strings(handle: &Handle) -> io::Result<DeviceStrings> {
    // STORAGE_PROPERTY_QUERY { PropertyId = StorageDeviceProperty (0),
    // QueryType = PropertyStandardQuery (0), AdditionalParameters[1] }.
    let query = [0u8; 12];
    let out = ioctl_out(handle, IOCTL_STORAGE_QUERY_PROPERTY, Some(&query), 4096)?;
    parse_device_descriptor(&out)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "short device descriptor"))
}

/// (size in bytes, bytes per sector) of an open disk. Fails when there is no
/// medium in the drive.
pub(crate) fn query_geometry(handle: &Handle) -> io::Result<(u64, u32)> {
    let out = ioctl_out(handle, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, None, 256)?;
    parse_geometry(&out)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "short disk geometry"))
}

fn query_layout(handle: &Handle) -> io::Result<Vec<u8>> {
    // Room for the 128 entries of a standard GPT; grown once if a disk has more.
    let mut capacity = LAYOUT_HEADER + 128 * LAYOUT_ENTRY;
    loop {
        match ioctl_out(handle, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, None, capacity) {
            Err(e)
                if e.raw_os_error() == Some(ERROR_INSUFFICIENT_BUFFER)
                    && capacity < LAYOUT_HEADER + 4096 * LAYOUT_ENTRY =>
            {
                capacity = LAYOUT_HEADER + 4096 * LAYOUT_ENTRY;
            }
            other => return other,
        }
    }
}

/// Collects everything about one disk from a query-only handle. Individual
/// queries that fail leave their fields at the "unknown" defaults; the disk
/// is still listed.
pub(crate) fn disk_facts(number: u32, handle: &Handle) -> DiskFacts {
    let mut facts = DiskFacts {
        number,
        ..DiskFacts::default()
    };
    if let Ok(device) = query_device_strings(handle) {
        facts.device = device;
    }
    // No medium (empty card reader): size stays 0.
    if let Ok((size, sector)) = query_geometry(handle) {
        facts.size = size;
        facts.sector_size = sector;
    }
    if facts.size > 0 {
        if let Some((scheme, partitions)) = query_layout(handle).ok().and_then(|b| parse_layout(&b))
        {
            facts.scheme = scheme;
            facts.partitions = partitions;
        }
    }
    if let Some((offline, read_only)) = ioctl_out(handle, IOCTL_DISK_GET_DISK_ATTRIBUTES, None, 16)
        .ok()
        .and_then(|b| parse_disk_attributes(&b))
    {
        facts.offline = offline;
        facts.read_only = read_only;
    }
    // The write-protect switch of SD cards and some sticks. Any other failure
    // (no medium, not supported) says nothing and leaves the flag alone.
    if let Err(e) = ioctl(handle, IOCTL_DISK_IS_WRITABLE, None, None) {
        if e.raw_os_error() == Some(ERROR_WRITE_PROTECT) {
            facts.read_only = true;
        }
    }
    facts
}

/// Every physical disk that can be opened for queries.
pub(crate) fn collect_disks() -> Vec<DiskFacts> {
    (0..MAX_DISKS)
        .filter_map(|n| {
            let handle =
                open_device(&physical_drive_path(n), 0, FILE_FLAGS_AND_ATTRIBUTES(0)).ok()?;
            Some(disk_facts(n, &handle))
        })
        .collect()
}

// --------------------------------------------------------------------------------------
// Volumes
// --------------------------------------------------------------------------------------

fn volume_guid_paths() -> Vec<String> {
    let mut out = Vec::new();
    let mut name = [0u16; 260];
    // SAFETY: the buffer is valid for the calls; the find handle is closed below.
    unsafe {
        let Ok(find) = FindFirstVolumeW(&mut name) else {
            return out;
        };
        loop {
            out.push(from_wide(&name));
            name.fill(0);
            if FindNextVolumeW(find, &mut name).is_err() {
                break;
            }
        }
        let _ = FindVolumeClose(find);
    }
    out
}

/// `\Device\HarddiskVolumeN` for `\\?\Volume{guid}\`.
fn nt_device_name(guid_path: &str) -> String {
    // QueryDosDevice wants the bare name: no `\\?\` prefix, no trailing slash.
    let bare = guid_path
        .trim_start_matches("\\\\?\\")
        .trim_end_matches('\\');
    let bare_w = wide(bare);
    let mut target = [0u16; 512];
    // SAFETY: NUL-terminated input, valid output buffer.
    let n = unsafe { QueryDosDeviceW(PCWSTR(bare_w.as_ptr()), Some(&mut target)) };
    if n == 0 {
        String::new()
    } else {
        from_wide(&target)
    }
}

/// The drive letter among a volume's mount points, upper case.
fn volume_letter(guid_path: &str) -> Option<char> {
    let path_w = wide(guid_path);
    let mut names = vec![0u16; 1024];
    let mut needed = 0u32;
    // SAFETY: NUL-terminated input, valid output buffer and length pointer.
    unsafe {
        GetVolumePathNamesForVolumeNameW(PCWSTR(path_w.as_ptr()), Some(&mut names), &mut needed)
    }
    .ok()?;
    split_multi_sz(&names).iter().find_map(|p| {
        let b = p.as_bytes();
        (b.len() == 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\')
            .then(|| b[0].to_ascii_uppercase() as char)
    })
}

/// Label and file-system name of a volume (`guid_path` with trailing slash).
/// Empty strings when the volume has no medium or no recognised file system.
fn label_and_fs(guid_path: &str) -> (String, String) {
    let _quiet = QuietErrors::new();
    let path_w = wide(guid_path);
    let mut label = [0u16; 261];
    let mut fs = [0u16; 261];
    // SAFETY: NUL-terminated input, valid output buffers.
    let ok = unsafe {
        GetVolumeInformationW(
            PCWSTR(path_w.as_ptr()),
            Some(&mut label),
            None,
            None,
            None,
            Some(&mut fs),
        )
    }
    .is_ok();
    if ok {
        (from_wide(&label), from_wide(&fs))
    } else {
        (String::new(), String::new())
    }
}

/// Runs the label queries on helper threads so that one sleeping or failing
/// drive cannot stall the enumeration: whatever has not answered within
/// `LABEL_BUDGET` is listed without label and file system.
fn labels_with_deadline(paths: &[String]) -> Vec<(String, String)> {
    let mut out = vec![(String::new(), String::new()); paths.len()];
    let (tx, rx) = mpsc::channel();
    for (i, path) in paths.iter().enumerate() {
        let (tx, path) = (tx.clone(), path.clone());
        std::thread::spawn(move || {
            let _ = tx.send((i, label_and_fs(&path)));
        });
    }
    drop(tx);
    let deadline = Instant::now() + LABEL_BUDGET;
    for _ in 0..paths.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((i, answer)) => out[i] = answer,
            Err(_) => break,
        }
    }
    out
}

/// A volume's disk extents and its (disk, partition) device number.
type VolumeLocation = (Vec<super::assemble::Extent>, Option<(u32, u32)>);

/// Disk extents and device number of a volume, from a query-only handle.
fn volume_location(guid_path: &str) -> io::Result<VolumeLocation> {
    // CreateFile wants the volume device itself: no trailing slash.
    let handle = open_device(
        guid_path.trim_end_matches('\\'),
        0,
        FILE_FLAGS_AND_ATTRIBUTES(0),
    )?;
    // Fails for volumes without a medium; the device number still works then.
    let extents = ioctl_out(
        &handle,
        IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
        None,
        8 + 64 * 24,
    )
    .map(|b| parse_extents(&b))
    .unwrap_or_default();
    let number = ioctl_out(&handle, IOCTL_STORAGE_GET_DEVICE_NUMBER, None, 12)
        .ok()
        .and_then(|b| parse_device_number(&b));
    Ok((extents, number))
}

/// Every disk-backed volume of the system (CD-ROM, network and RAM drives are
/// left out). `with_labels` also queries label and file system, which touches
/// the medium; the locking code does not need them.
pub(crate) fn collect_volumes(with_labels: bool) -> Vec<VolumeFacts> {
    let _quiet = QuietErrors::new();
    let mut volumes = Vec::new();
    for guid_path in volume_guid_paths() {
        let path_w = wide(&guid_path);
        // SAFETY: NUL-terminated input.
        let drive_type = unsafe { GetDriveTypeW(PCWSTR(path_w.as_ptr())) };
        if matches!(drive_type, DRIVE_REMOTE | DRIVE_CDROM | DRIVE_RAMDISK) {
            continue;
        }
        let (extents, device_number) = volume_location(&guid_path).unwrap_or_default();
        // Fixed and removable volumes are disks by definition; anything else
        // only counts when it demonstrably sits on a disk.
        let disk_backed = matches!(drive_type, DRIVE_REMOVABLE | DRIVE_FIXED);
        if !disk_backed && extents.is_empty() {
            continue;
        }
        volumes.push(VolumeFacts {
            device: nt_device_name(&guid_path),
            letter: volume_letter(&guid_path),
            guid_path,
            label: String::new(),
            fs: String::new(),
            extents,
            device_number,
        });
    }
    if with_labels {
        let paths: Vec<String> = volumes.iter().map(|v| v.guid_path.clone()).collect();
        for (v, (label, fs)) in volumes.iter_mut().zip(labels_with_deadline(&paths)) {
            v.label = label;
            v.fs = fs;
        }
    }
    volumes
}

// --------------------------------------------------------------------------------------
// System facts
// --------------------------------------------------------------------------------------

/// Turns a path into the form the volume functions accept: the `\\?\` prefix
/// of a drive path is removed; network paths give `None`.
pub(crate) fn local_path(path: &str) -> Option<String> {
    if let Some(rest) = path.strip_prefix("\\\\?\\") {
        if rest.len() >= 4 && rest[..4].eq_ignore_ascii_case("UNC\\") {
            return None;
        }
        return Some(rest.to_string());
    }
    if path.starts_with("\\\\") || path.starts_with("//") {
        return None;
    }
    Some(path.to_string())
}

/// The volume GUID path (`\\?\Volume{guid}\`) of the volume a local file or
/// folder lives on, following mounted folders.
pub(crate) fn volume_of_path(path: &Path) -> Option<String> {
    let local = local_path(&path.to_string_lossy())?;
    let local_w = wide(&local);
    let mut mount = vec![0u16; local_w.len() + 8];
    let mut name = [0u16; 64];
    // SAFETY: NUL-terminated inputs, valid output buffers.
    unsafe {
        GetVolumePathNameW(PCWSTR(local_w.as_ptr()), &mut mount).ok()?;
        GetVolumeNameForVolumeMountPointW(PCWSTR(mount.as_ptr()), &mut name).ok()?;
    }
    Some(from_wide(&name))
}

/// The number of the first disk backing the volume `path` lives on.
pub(crate) fn disk_number_of_path(path: &Path) -> Option<u32> {
    let guid_path = volume_of_path(path)?;
    let (extents, number) = volume_location(&guid_path).ok()?;
    extents
        .first()
        .map(|e| e.disk)
        .or(number.map(|(disk, _)| disk))
}

enum RegValue {
    Found(Vec<u16>),
    Missing,
    Unreadable,
}

fn read_hklm(subkey: &str, value: &str, kind: REG_ROUTINE_FLAGS) -> RegValue {
    let subkey_w = wide(subkey);
    let value_w = wide(value);
    let mut buf = vec![0u16; 512];
    for _ in 0..3 {
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: NUL-terminated names; the buffer is valid for `bytes` bytes.
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(subkey_w.as_ptr()),
                PCWSTR(value_w.as_ptr()),
                kind,
                None,
                Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
                Some(&mut bytes),
            )
        };
        if status == ERROR_SUCCESS {
            buf.truncate((bytes as usize).div_ceil(2));
            return RegValue::Found(buf);
        } else if status == ERROR_MORE_DATA {
            buf = vec![0u16; (bytes as usize).div_ceil(2) + 2];
        } else if status == ERROR_FILE_NOT_FOUND {
            return RegValue::Missing;
        } else {
            return RegValue::Unreadable;
        }
    }
    RegValue::Unreadable
}

fn windows_directory() -> Option<String> {
    let mut buf = [0u16; 300];
    // SAFETY: valid output buffer.
    let n = unsafe { GetWindowsDirectoryW(Some(&mut buf)) } as usize;
    (n > 0 && n < buf.len()).then(|| from_wide(&buf))
}

/// Where Windows and the running program live.
pub(crate) fn collect_system_facts() -> SystemFacts {
    let windows_volume = windows_directory().and_then(|d| volume_of_path(Path::new(&d)));

    let system_partition = match read_hklm("SYSTEM\\Setup", "SystemPartition", RRF_RT_REG_SZ) {
        RegValue::Found(v) => Some(from_wide(&v)).filter(|s| !s.trim().is_empty()),
        RegValue::Missing | RegValue::Unreadable => None,
    };

    let page_files = match read_hklm(
        "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Memory Management",
        "ExistingPageFiles",
        RRF_RT_REG_MULTI_SZ,
    ) {
        RegValue::Found(v) => split_multi_sz(&v),
        // No page file configured.
        RegValue::Missing => Vec::new(),
        RegValue::Unreadable => vec![UNREADABLE.to_string()],
    };

    // Resolve the executable through junctions, SUBST drives and mounted
    // folders to the volume it really lives on.
    let exe = std::env::current_exe().ok();
    let exe = exe.map(|p| std::fs::canonicalize(&p).unwrap_or(p));
    SystemFacts {
        windows_volume,
        system_partition,
        page_files,
        program_volume: exe.as_deref().and_then(volume_of_path),
        program_letter: exe.as_deref().and_then(crate::safety::path_letter),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_paths() {
        assert_eq!(local_path("C:\\Windows").as_deref(), Some("C:\\Windows"));
        assert_eq!(
            local_path("\\\\?\\D:\\x\\y.exe").as_deref(),
            Some("D:\\x\\y.exe")
        );
        assert_eq!(local_path("\\\\?\\UNC\\server\\share\\y.exe"), None);
        assert_eq!(local_path("\\\\?\\unc\\server\\share"), None);
        assert_eq!(local_path("\\\\server\\share\\y.exe"), None);
        assert_eq!(
            local_path("relative\\file").as_deref(),
            Some("relative\\file")
        );
    }

    #[test]
    fn the_unreadable_marker_has_no_letter() {
        assert_eq!(super::super::assemble::page_file_letter(UNREADABLE), None);
    }

    /// Read-only look at this machine; asserts structure only and prints nothing.
    #[test]
    fn this_machine_resolves_its_system_volumes() {
        let volumes = collect_volumes(false);
        assert!(!volumes.is_empty());
        assert!(volumes
            .iter()
            .all(|v| v.guid_path.starts_with("\\\\?\\Volume{")));
        assert!(volumes.iter().all(|v| v.guid_path.ends_with('\\')));
        let facts = collect_system_facts();
        let (system, doubt) = super::super::assemble::system_disks(&volumes, &facts);
        // On a normally installed Windows everything maps cleanly.
        assert!(facts.windows_volume.is_some());
        assert!(facts.system_partition.is_some());
        assert!(!system.is_empty());
        assert!(!doubt, "a system volume could not be mapped to a disk");
        // The temp directory is on a local disk that the enumeration knows.
        let temp_disk = disk_number_of_path(&std::env::temp_dir());
        assert!(temp_disk.is_some());
        let disks = collect_disks();
        assert!(disks.iter().any(|d| Some(d.number) == temp_disk));
        assert!(disks.iter().all(|d| d.number < MAX_DISKS));
        assert!(system.iter().all(|s| disks.iter().any(|d| d.number == *s)));
    }
}
