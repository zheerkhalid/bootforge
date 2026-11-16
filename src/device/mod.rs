//! Disk enumeration, raw physical-drive access and hot-plug watching (Windows).
//!
//! * [`list_disks`] enumerates every physical disk, read-only and without
//!   administrator rights, and flags the ones Windows or this program run from.
//! * [`RawDisk`] is the `BlockDevice` for a real drive; opening it for writing
//!   requires a [`Clearance`](crate::safety::Clearance).
//! * [`Watcher`] reports when disks or volumes arrive or leave.
//!
//! The Win32 collection (`collect.rs`) is kept apart from the decisions
//! (`assemble.rs`), so the latter can be tested with invented machines.

mod assemble;
mod collect;
mod eject;
pub mod raw;
mod watch;
mod win;

pub use raw::RawDisk;
pub use watch::Watcher;

use crate::error::{Error, Result};
use crate::model::Disk;
use crate::safety::Clearance;
use std::path::Path;

/// Enumerates every physical disk of the machine (`\\.\PhysicalDrive0..63`).
///
/// Strictly read-only: disks and volumes are opened for queries only, so this
/// works without elevation and never disturbs a drive that is in use. Empty
/// card readers are listed with `size == 0`. No error dialogs are shown for
/// drives without a medium, and label queries are bounded in time.
///
/// `is_system` is set for every disk holding the Windows volume, the
/// partition Windows booted from or a page file. It fails closed: when any of
/// these cannot be mapped to a disk, every disk that is not attached by USB
/// is marked as a system disk.
///
/// The result is sorted by disk number. Fails only when no disk at all could
/// be queried.
pub fn list_disks() -> Result<Vec<Disk>> {
    let _quiet = collect::QuietErrors::new();
    let disks = collect::collect_disks();
    if disks.is_empty() {
        return Err(Error::msg("Windows did not report any disk drives."));
    }
    let volumes = collect::collect_volumes(true);
    let facts = collect::collect_system_facts();
    Ok(assemble::assemble(&disks, &volumes, &facts))
}

/// The number of the physical disk a local file or folder is stored on
/// (the first one, for volumes spanning several disks). `None` for network
/// paths and paths that do not resolve to a disk-backed volume. Read-only.
pub fn disk_of_path(path: &Path) -> Option<u32> {
    collect::disk_number_of_path(path)
}

/// Shorthand for [`RawDisk::open_for_write`].
pub fn open_for_write(clearance: &Clearance) -> Result<RawDisk> {
    RawDisk::open_for_write(clearance)
}

/// Shorthand for [`RawDisk::open_for_read`].
pub fn open_for_read(number: u32) -> Result<RawDisk> {
    RawDisk::open_for_read(number)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read-only enumeration of this machine. Asserts generic facts only and
    /// prints nothing that identifies the hardware.
    #[test]
    fn this_machine_lists_its_disks() {
        let disks = list_disks().unwrap();
        assert!(!disks.is_empty());
        assert!(disks.windows(2).all(|w| w[0].number < w[1].number));
        assert!(
            disks.iter().any(|d| d.is_system),
            "no system disk was identified"
        );
        for d in &disks {
            assert!(d.sector_size >= 512, "sector size");
            assert!(d.partitions.iter().all(|p| !p.kind.is_empty()));
            assert!(d.letters().iter().all(|l| l.is_ascii_uppercase()));
            assert_eq!(d.serial, d.serial.trim());
            assert_eq!(d.model, d.model.trim());
        }
        // The disk holding the temp folder is in the list.
        let temp = disk_of_path(&std::env::temp_dir()).expect("temp folder is on a disk");
        assert!(disks.iter().any(|d| d.number == temp));
        // The test executable lives on a local disk, which is flagged.
        if disk_of_path(&std::env::current_exe().unwrap()).is_some() {
            assert!(disks.iter().any(|d| d.holds_program));
        }
        // No drive letter is claimed by two disks.
        let mut letters: Vec<char> = disks.iter().flat_map(|d| d.letters()).collect();
        let total = letters.len();
        letters.sort_unstable();
        letters.dedup();
        assert_eq!(letters.len(), total);
        // Enumerating twice gives the same identities.
        let again = list_disks().unwrap();
        let ids = |v: &[Disk]| v.iter().map(|d| d.identity()).collect::<Vec<_>>();
        assert_eq!(ids(&disks), ids(&again));
    }

    #[test]
    fn network_and_missing_paths_have_no_disk() {
        assert_eq!(
            disk_of_path(Path::new("\\\\server\\share\\image.iso")),
            None
        );
    }
}
