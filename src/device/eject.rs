//! "Safely remove hardware" for a disk, by its disk number.
//!
//! The documented route: find the disk's device node through the SetupAPI
//! device-interface list, step up to its parent (the USB device the disk
//! function belongs to) and ask the PnP manager to eject that with
//! `CM_Request_Device_EjectW`. This is what the tray icon does; the system
//! refuses ("vetoes") the request when something still uses the device.

use super::assemble::parse_device_number;
use super::win::{from_wide, ioctl_out, open_device, Handle};
use std::time::Duration;
use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Parent, CM_Request_Device_EjectW, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
    CR_SUCCESS, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, PNP_VETO_TYPE,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
};
use windows::Win32::Foundation::HWND;
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::System::Ioctl::{GUID_DEVINTERFACE_DISK, IOCTL_STORAGE_GET_DEVICE_NUMBER};

/// The eject request is repeated a few times: right after a write Windows is
/// busy mounting the new volumes and vetoes the first attempt.
const EJECT_ATTEMPTS: u32 = 4;
const EJECT_RETRY_DELAY: Duration = Duration::from_millis(1000);

/// Closes a SetupAPI device list on drop.
struct DeviceList(HDEVINFO);

impl Drop for DeviceList {
    fn drop(&mut self) {
        // SAFETY: the list was created by SetupDiGetClassDevsW and is destroyed once.
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

/// The disk number behind a device-interface path (query-only open).
fn disk_number_of_interface(path: &str) -> Option<u32> {
    let handle: Handle = open_device(path, 0, FILE_FLAGS_AND_ATTRIBUTES(0)).ok()?;
    let out = ioctl_out(&handle, IOCTL_STORAGE_GET_DEVICE_NUMBER, None, 12).ok()?;
    parse_device_number(&out).map(|(number, _)| number)
}

/// The PnP device instance (devnode) of physical disk `number`. Read-only.
pub(crate) fn find_disk_devinst(number: u32) -> Option<u32> {
    // SAFETY: SetupAPI calls with correctly sized, initialised structures;
    // the detail buffer is u64-aligned and large enough for `needed` bytes.
    unsafe {
        let list = DeviceList(
            SetupDiGetClassDevsW(
                Some(&GUID_DEVINTERFACE_DISK),
                PCWSTR::null(),
                HWND::default(),
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )
            .ok()?,
        );
        for index in 0..1024 {
            let mut interface = SP_DEVICE_INTERFACE_DATA {
                cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInterfaces(
                list.0,
                None,
                &GUID_DEVINTERFACE_DISK,
                index,
                &mut interface,
            )
            .is_err()
            {
                break; // no more interfaces
            }
            // First call: how large is the detail (it ends in the device path)?
            let mut needed = 0u32;
            let _ = SetupDiGetDeviceInterfaceDetailW(
                list.0,
                &interface,
                None,
                0,
                Some(&mut needed),
                None,
            );
            if (needed as usize) < std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() {
                continue;
            }
            let mut storage = vec![0u64; (needed as usize).div_ceil(8) + 1];
            let detail = storage.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            let mut info = SP_DEVINFO_DATA {
                cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiGetDeviceInterfaceDetailW(
                list.0,
                &interface,
                Some(detail),
                needed,
                None,
                Some(&mut info),
            )
            .is_err()
            {
                continue;
            }
            // The path starts at the `DevicePath` field and is NUL-terminated
            // inside the `needed` bytes the call filled in.
            let path_offset = std::mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
            let chars = (needed as usize - path_offset) / 2;
            let path_ptr = (storage.as_ptr() as *const u8).add(path_offset) as *const u16;
            let path = from_wide(std::slice::from_raw_parts(path_ptr, chars));
            if disk_number_of_interface(&path) == Some(number) {
                return Some(info.DevInst);
            }
        }
        None
    }
}

/// Text for a vetoed or failed eject request.
pub(crate) fn eject_failure_text(code: u32, veto_type: i32, veto_name: &str) -> String {
    let name = veto_name.trim();
    if veto_type != 0 || !name.is_empty() {
        if name.is_empty() {
            "Windows reports the device is still in use".to_string()
        } else {
            format!("Windows reports the device is still in use by {name}")
        }
    } else {
        format!("the request failed with configuration manager code {code}")
    }
}

/// Safely removes the device that physical disk `number` belongs to. The
/// caller must have closed all of its own handles to the disk first.
///
/// Returns a short reason on failure; the data on the drive is unaffected.
pub(crate) fn eject_disk(number: u32) -> std::result::Result<(), String> {
    let disk = find_disk_devinst(number)
        .ok_or_else(|| "the device could not be found any more".to_string())?;
    let mut parent = 0u32;
    // SAFETY: valid out-pointer.
    if unsafe { CM_Get_Parent(&mut parent, disk, 0) } != CR_SUCCESS {
        return Err("its parent device could not be determined".to_string());
    }
    let mut last = String::new();
    for attempt in 0..EJECT_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(EJECT_RETRY_DELAY);
        }
        let mut veto_type = PNP_VETO_TYPE(0);
        let mut veto_name = [0u16; 260];
        // SAFETY: valid out-pointer and buffer. Passing a veto buffer also
        // keeps Windows from showing its own "device in use" dialog.
        let result = unsafe {
            CM_Request_Device_EjectW(parent, Some(&mut veto_type), Some(&mut veto_name), 0)
        };
        if result == CR_SUCCESS && veto_type.0 == 0 {
            return Ok(());
        }
        last = eject_failure_text(result.0, veto_type.0, &from_wide(&veto_name));
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_texts() {
        assert_eq!(
            eject_failure_text(23, 6, "\\Device\\HarddiskVolume9 "),
            "Windows reports the device is still in use by \\Device\\HarddiskVolume9"
        );
        assert_eq!(
            eject_failure_text(23, 5, ""),
            "Windows reports the device is still in use"
        );
        assert!(eject_failure_text(13, 0, "").contains("code 13"));
    }

    /// Read-only lookup of a device node; nothing is ejected.
    #[test]
    fn the_disk_holding_the_temp_folder_has_a_device_node() {
        let Some(number) = super::super::collect::disk_number_of_path(&std::env::temp_dir()) else {
            return;
        };
        assert!(find_disk_devinst(number).is_some());
        assert!(find_disk_devinst(9_999).is_none());
    }
}
