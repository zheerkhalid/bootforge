//! Thin, shared wrappers over the handful of Win32 calls the device layer needs.

use crate::sys::wide;
use std::io;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;

/// Converts a `windows` crate error into an `io::Error` carrying the plain
/// Win32 error code (so `kind()` and the system message are meaningful).
pub(crate) fn win_err(e: windows::core::Error) -> io::Error {
    let hr = e.code().0 as u32;
    // FACILITY_WIN32 HRESULTs are 0x8007xxxx with the Win32 code in the low word.
    if hr & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((hr & 0xFFFF) as i32)
    } else {
        io::Error::other(e.message())
    }
}

/// An owned kernel handle, closed on drop.
pub(crate) struct Handle(HANDLE);

// SAFETY: a kernel handle is a process-wide token, not tied to the thread
// that opened it; the wrapper gives out no interior references.
unsafe impl Send for Handle {}

impl Handle {
    pub(crate) fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this wrapper and closed exactly once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Opens a device or volume path (`\\.\PhysicalDrive1`, `\\?\Volume{...}`).
///
/// `access` 0 opens the device for queries only: no data can be read or
/// written through such a handle and no administrator rights are needed.
/// Sharing is always read+write, because the system keeps its own handles.
pub(crate) fn open_device(
    path: &str,
    access: u32,
    flags: FILE_FLAGS_AND_ATTRIBUTES,
) -> io::Result<Handle> {
    let path_w = wide(path);
    // SAFETY: NUL-terminated path that outlives the call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path_w.as_ptr()),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            flags,
            HANDLE::default(),
        )
    }
    .map_err(win_err)?;
    Ok(Handle(handle))
}

/// `DeviceIoControl` with optional input and output buffers. Returns the
/// number of bytes the driver put into `output`.
pub(crate) fn ioctl(
    handle: &Handle,
    code: u32,
    input: Option<&[u8]>,
    output: Option<&mut [u8]>,
) -> io::Result<usize> {
    let mut returned = 0u32;
    let (in_ptr, in_len) = match input {
        Some(b) => (Some(b.as_ptr() as *const core::ffi::c_void), b.len() as u32),
        None => (None, 0),
    };
    let (out_ptr, out_len) = match output {
        Some(b) => (
            Some(b.as_mut_ptr() as *mut core::ffi::c_void),
            b.len() as u32,
        ),
        None => (None, 0),
    };
    // SAFETY: the pointers and lengths describe live slices; the call is
    // synchronous (no OVERLAPPED), so they are not used after it returns.
    unsafe {
        DeviceIoControl(
            handle.raw(),
            code,
            in_ptr,
            in_len,
            out_ptr,
            out_len,
            Some(&mut returned),
            None,
        )
    }
    .map_err(win_err)?;
    Ok(returned as usize)
}

/// `ioctl` into a fresh buffer of `capacity` bytes, truncated to what the
/// driver returned.
pub(crate) fn ioctl_out(
    handle: &Handle,
    code: u32,
    input: Option<&[u8]>,
    capacity: usize,
) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; capacity];
    let n = ioctl(handle, code, input, Some(&mut buf))?;
    buf.truncate(n.min(capacity));
    Ok(buf)
}

/// UTF-16 buffer up to the first NUL as a `String`.
pub(crate) fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Splits a double-NUL-terminated list of UTF-16 strings (`REG_MULTI_SZ`,
/// volume path names).
pub(crate) fn split_multi_sz(buf: &[u16]) -> Vec<String> {
    buf.split(|c| *c == 0)
        .take_while(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

/// `\\.\PhysicalDriveN`.
pub(crate) fn physical_drive_path(number: u32) -> String {
    format!("\\\\.\\PhysicalDrive{number}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_string_helpers() {
        let w: Vec<u16> = "NTFS\0junk".encode_utf16().collect();
        assert_eq!(from_wide(&w), "NTFS");
        assert_eq!(from_wide(&[]), "");
        let m: Vec<u16> = "E:\\\0C:\\mount\\\0\0ignored\0".encode_utf16().collect();
        assert_eq!(split_multi_sz(&m), vec!["E:\\", "C:\\mount\\"]);
        assert!(split_multi_sz(&[0, 0]).is_empty());
        assert_eq!(physical_drive_path(7), "\\\\.\\PhysicalDrive7");
    }

    #[test]
    fn win32_errors_keep_their_code() {
        let e = win_err(windows::core::Error::from_hresult(windows::core::HRESULT(
            0x8007_0005u32 as i32,
        )));
        assert_eq!(e.raw_os_error(), Some(5));
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    }
}
