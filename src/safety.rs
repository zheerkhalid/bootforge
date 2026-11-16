//! The rules that decide which disks may ever be written.
//!
//! BootForge erases drives, so the rules fail closed: a disk is writable only
//! when everything known about it says "external, removable, not in use by
//! Windows or by this program, and not suspiciously large". The same check
//! runs when the list is built for the UI and again inside the engine,
//! immediately before the device is opened for writing; the raw-disk layer
//! only accepts a [`Clearance`], which nothing but [`clear`] can create.

use crate::model::{Bus, Disk, DiskIdentity};
use crate::util::human_size;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

pub const GIB: u64 = 1024 * 1024 * 1024;

/// No setting can raise the size cap above this (2 TiB: the limit of MBR and
/// of 512-byte-sector FAT32, and far beyond any USB stick).
pub const HARD_MAX_SIZE: u64 = 2048 * GIB;

/// Default size cap for target drives.
pub const DEFAULT_MAX_SIZE: u64 = 256 * GIB;

/// User-adjustable part of the rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Also offer USB disks that report fixed media (USB hard drives, SSD
    /// enclosures and some large flash drives). Off by default.
    pub show_usb_hdd: bool,
    /// Disks larger than this are never offered.
    pub max_size: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            show_usb_hdd: false,
            max_size: DEFAULT_MAX_SIZE,
        }
    }
}

impl Policy {
    /// The cap actually applied (clamped to the hard maximum).
    pub fn effective_max(&self) -> u64 {
        self.max_size.clamp(GIB, HARD_MAX_SIZE)
    }
}

/// Why a disk may not be written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Holds Windows, the boot partition, a page file or a hibernation file.
    SystemDisk,
    /// Holds the running program.
    HoldsProgram,
    /// Not attached by USB or a card reader (or the bus could not be determined).
    NotExternal(String),
    /// A USB disk with fixed media while "show USB hard drives" is off.
    FixedMedia,
    TooLarge {
        size: u64,
        cap: u64,
    },
    /// No medium in the reader, or the size could not be read.
    NoMedia,
    ReadOnly,
    Offline,
    /// A file that is needed for the operation (the image being written, or
    /// the backup being created) is stored on this disk.
    HoldsFile(PathBuf),
    /// The disk is not the one that was confirmed.
    Changed(String),
}

impl Refusal {
    /// Disks refused for these reasons are not shown at all; the others are
    /// listed greyed out with the reason, because the user can fix them.
    pub fn hides_disk(&self) -> bool {
        matches!(
            self,
            Refusal::SystemDisk
                | Refusal::HoldsProgram
                | Refusal::NotExternal(_)
                | Refusal::FixedMedia
                | Refusal::TooLarge { .. }
        )
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::SystemDisk => write!(f, "this disk holds Windows or its boot files"),
            Refusal::HoldsProgram => write!(f, "BootForge itself is running from this disk"),
            Refusal::NotExternal(bus) => write!(
                f,
                "this disk is not a USB drive or memory card (connection: {bus})"
            ),
            Refusal::FixedMedia => write!(
                f,
                "this is a USB hard drive or SSD; turn on \"Show USB hard drives\" in Settings to use it"
            ),
            Refusal::TooLarge { size, cap } => write!(
                f,
                "the disk is {} and the size limit in Settings is {}",
                human_size(*size),
                human_size(*cap)
            ),
            Refusal::NoMedia => write!(f, "there is no card or medium in this drive"),
            Refusal::ReadOnly => write!(f, "the drive is write-protected"),
            Refusal::Offline => write!(f, "the disk is offline"),
            Refusal::HoldsFile(p) => write!(
                f,
                "\"{}\" is stored on this drive",
                p.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.display().to_string())
            ),
            Refusal::Changed(what) => write!(
                f,
                "the drive changed since it was confirmed ({what}); nothing was written"
            ),
        }
    }
}

impl From<Refusal> for crate::error::Error {
    fn from(r: Refusal) -> Self {
        crate::error::Error::Refused(r.to_string())
    }
}

/// The drive letter a path lives on (`C:\x` and `\\?\C:\x` give `C`).
/// `None` for UNC and relative paths.
pub fn path_letter(path: &Path) -> Option<char> {
    let s = path.to_string_lossy();
    let s = s.strip_prefix("\\\\?\\").unwrap_or(&s);
    let b = s.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        Some(b[0].to_ascii_uppercase() as char)
    } else {
        None
    }
}

/// The complete rule set. `needed_files` are files that must survive the
/// operation (the source image, a backup target).
pub fn check(disk: &Disk, policy: &Policy, needed_files: &[PathBuf]) -> Result<(), Refusal> {
    // Order matters only for the message: the most fundamental reason first.
    if disk.is_system {
        return Err(Refusal::SystemDisk);
    }
    if disk.holds_program {
        return Err(Refusal::HoldsProgram);
    }
    match disk.bus {
        Bus::Usb | Bus::Sd => {}
        _ => return Err(Refusal::NotExternal(disk.bus.label().to_string())),
    }
    if !disk.removable && !policy.show_usb_hdd {
        return Err(Refusal::FixedMedia);
    }
    let cap = policy.effective_max();
    if disk.size > cap {
        return Err(Refusal::TooLarge {
            size: disk.size,
            cap,
        });
    }
    if disk.size == 0 || disk.sector_size == 0 {
        return Err(Refusal::NoMedia);
    }
    if disk.offline {
        return Err(Refusal::Offline);
    }
    if disk.read_only {
        return Err(Refusal::ReadOnly);
    }
    let letters = disk.letters();
    for f in needed_files {
        if let Some(l) = path_letter(f) {
            if letters.contains(&l) {
                return Err(Refusal::HoldsFile(f.clone()));
            }
        }
    }
    Ok(())
}

/// A disk as shown in the device list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub disk: Disk,
    /// `Some` when the disk is shown but cannot be chosen.
    pub blocked: Option<Refusal>,
}

/// Filters an enumeration down to what the UI may show. Returns the list and
/// the number of disks that were left out.
pub fn listable(disks: &[Disk], policy: &Policy, needed_files: &[PathBuf]) -> (Vec<Listed>, usize) {
    let mut shown = Vec::new();
    let mut hidden = 0;
    for d in disks {
        match check(d, policy, needed_files) {
            Ok(()) => shown.push(Listed {
                disk: d.clone(),
                blocked: None,
            }),
            Err(r) if r.hides_disk() => hidden += 1,
            Err(r) => shown.push(Listed {
                disk: d.clone(),
                blocked: Some(r),
            }),
        }
    }
    (shown, hidden)
}

/// Compares the disk found now with the one the user confirmed.
pub fn same_device(expected: &DiskIdentity, now: &Disk) -> Result<(), Refusal> {
    if expected.number != now.number {
        return Err(Refusal::Changed("different disk number".into()));
    }
    if expected.size != now.size {
        return Err(Refusal::Changed(format!(
            "size was {}, is now {}",
            human_size(expected.size),
            human_size(now.size)
        )));
    }
    if expected.serial != now.serial {
        return Err(Refusal::Changed("different serial number".into()));
    }
    if expected.model != now.model {
        return Err(Refusal::Changed("different model".into()));
    }
    Ok(())
}

/// Proof that a disk passed every rule and still is the confirmed device.
/// The raw-disk layer demands one before it opens a disk for writing; the
/// only way to get one is [`clear`].
#[derive(Debug)]
pub struct Clearance {
    identity: DiskIdentity,
    sector_size: u32,
    letters: Vec<char>,
}

impl Clearance {
    pub fn identity(&self) -> &DiskIdentity {
        &self.identity
    }
    pub fn disk_number(&self) -> u32 {
        self.identity.number
    }
    pub fn size(&self) -> u64 {
        self.identity.size
    }
    pub fn sector_size(&self) -> u32 {
        self.sector_size
    }
    /// Drive letters that were on the disk when it was cleared.
    pub fn letters(&self) -> &[char] {
        &self.letters
    }
}

/// Re-checks a freshly enumerated disk against the confirmed identity and all
/// rules. `current` must come from a new enumeration made just before writing.
pub fn clear(
    expected: &DiskIdentity,
    current: &[Disk],
    policy: &Policy,
    needed_files: &[PathBuf],
) -> Result<Clearance, Refusal> {
    let now = current
        .iter()
        .find(|d| d.number == expected.number)
        .ok_or_else(|| Refusal::Changed("it is no longer connected".into()))?;
    same_device(expected, now)?;
    check(now, policy, needed_files)?;
    Ok(Clearance {
        identity: now.identity(),
        sector_size: now.sector_size,
        letters: now.letters(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Partition;

    fn usb(number: u32, size_gb: u64) -> Disk {
        Disk {
            number,
            vendor: "Kestrel".into(),
            model: "Flash 3.1".into(),
            serial: format!("TEST{number:04}"),
            bus: Bus::Usb,
            removable: true,
            size: size_gb * 1_000_000_000,
            sector_size: 512,
            scheme: None,
            partitions: vec![Partition {
                number: 1,
                offset: 1 << 20,
                size: size_gb * 1_000_000_000 - (2 << 20),
                kind: "FAT32".into(),
                letter: Some((b'E' + number as u8) as char),
                label: "STICK".into(),
                fs: "FAT32".into(),
            }],
            is_system: false,
            holds_program: false,
            read_only: false,
            offline: false,
        }
    }

    #[test]
    fn plain_usb_stick_is_allowed() {
        assert_eq!(check(&usb(1, 32), &Policy::default(), &[]), Ok(()));
        let mut sd = usb(2, 64);
        sd.bus = Bus::Sd;
        assert_eq!(check(&sd, &Policy::default(), &[]), Ok(()));
    }

    #[test]
    fn system_disk_is_refused_whatever_else_is_true() {
        let mut d = usb(1, 32);
        d.is_system = true;
        let lax = Policy {
            show_usb_hdd: true,
            max_size: u64::MAX,
        };
        assert_eq!(check(&d, &lax, &[]), Err(Refusal::SystemDisk));
        // Windows To Go on a removable USB stick is still the system disk.
        assert!(Refusal::SystemDisk.hides_disk());
    }

    #[test]
    fn program_disk_is_refused() {
        let mut d = usb(1, 32);
        d.holds_program = true;
        assert_eq!(
            check(&d, &Policy::default(), &[]),
            Err(Refusal::HoldsProgram)
        );
    }

    #[test]
    fn internal_and_unknown_buses_are_refused() {
        for bus in [
            Bus::Sata,
            Bus::Nvme,
            Bus::Ata,
            Bus::Scsi,
            Bus::Raid,
            Bus::Virtual,
            Bus::Other("1394".into()),
            Bus::Unknown,
        ] {
            let mut d = usb(1, 32);
            d.bus = bus.clone();
            // Even when the disk claims removable media and the lax policy is on.
            let lax = Policy {
                show_usb_hdd: true,
                max_size: u64::MAX,
            };
            let r = check(&d, &lax, &[]).unwrap_err();
            assert!(matches!(r, Refusal::NotExternal(_)), "{bus:?} gave {r:?}");
            assert!(r.hides_disk());
        }
    }

    #[test]
    fn usb_hard_drives_need_the_explicit_option() {
        let mut d = usb(1, 120);
        d.removable = false;
        assert_eq!(check(&d, &Policy::default(), &[]), Err(Refusal::FixedMedia));
        let p = Policy {
            show_usb_hdd: true,
            ..Policy::default()
        };
        assert_eq!(check(&d, &p, &[]), Ok(()));
    }

    #[test]
    fn size_cap_applies_and_cannot_exceed_the_hard_maximum() {
        let d = usb(1, 500);
        assert!(matches!(
            check(&d, &Policy::default(), &[]),
            Err(Refusal::TooLarge { .. })
        ));
        let p = Policy {
            show_usb_hdd: false,
            max_size: 512 * GIB,
        };
        assert_eq!(check(&d, &p, &[]), Ok(()));
        let huge = usb(1, 4000);
        let p = Policy {
            show_usb_hdd: true,
            max_size: u64::MAX,
        };
        assert_eq!(p.effective_max(), HARD_MAX_SIZE);
        assert!(matches!(
            check(&huge, &p, &[]),
            Err(Refusal::TooLarge { .. })
        ));
        // Exactly at the cap is allowed, one byte over is not.
        let mut edge = usb(1, 1);
        edge.size = DEFAULT_MAX_SIZE;
        assert_eq!(check(&edge, &Policy::default(), &[]), Ok(()));
        edge.size += 1;
        assert!(check(&edge, &Policy::default(), &[]).is_err());
    }

    #[test]
    fn empty_readers_read_only_and_offline_disks_are_blocked_but_listed() {
        let mut d = usb(1, 32);
        d.size = 0;
        let r = check(&d, &Policy::default(), &[]).unwrap_err();
        assert_eq!(r, Refusal::NoMedia);
        assert!(!r.hides_disk());
        let mut d = usb(1, 32);
        d.read_only = true;
        assert_eq!(check(&d, &Policy::default(), &[]), Err(Refusal::ReadOnly));
        let mut d = usb(1, 32);
        d.offline = true;
        assert_eq!(check(&d, &Policy::default(), &[]), Err(Refusal::Offline));
    }

    #[test]
    fn disk_holding_the_image_is_refused() {
        let d = usb(1, 32); // letter F
        let image = PathBuf::from("f:\\isos\\setup.iso");
        assert_eq!(
            check(&d, &Policy::default(), std::slice::from_ref(&image)),
            Err(Refusal::HoldsFile(image))
        );
        let other = PathBuf::from("\\\\?\\D:\\isos\\setup.iso");
        assert_eq!(check(&d, &Policy::default(), &[other]), Ok(()));
        assert_eq!(path_letter(Path::new("\\\\server\\share\\a.iso")), None);
        assert_eq!(path_letter(Path::new("\\\\?\\d:\\a.iso")), Some('D'));
    }

    #[test]
    fn listing_hides_what_must_never_be_offered() {
        let mut system = usb(0, 500);
        system.bus = Bus::Nvme;
        system.removable = false;
        system.is_system = true;
        let mut internal = usb(1, 1000);
        internal.bus = Bus::Sata;
        internal.removable = false;
        let stick = usb(2, 16);
        let mut hdd = usb(3, 200);
        hdd.removable = false;
        let big = usb(4, 1000);
        let mut locked = usb(5, 8);
        locked.read_only = true;
        let all = [system, internal, stick.clone(), hdd, big, locked];
        let (shown, hidden) = listable(&all, &Policy::default(), &[]);
        assert_eq!(hidden, 4);
        assert_eq!(shown.len(), 2);
        assert_eq!(shown[0].disk, stick);
        assert_eq!(shown[0].blocked, None);
        assert_eq!(shown[1].blocked, Some(Refusal::ReadOnly));
        let p = Policy {
            show_usb_hdd: true,
            ..Policy::default()
        };
        let (shown, hidden) = listable(&all, &p, &[]);
        assert_eq!((shown.len(), hidden), (3, 3));
        // No policy ever lists the internal or the system disk.
        let p = Policy {
            show_usb_hdd: true,
            max_size: u64::MAX,
        };
        let (shown, _) = listable(&all, &p, &[]);
        assert!(shown.iter().all(|l| l.disk.number >= 2));
    }

    #[test]
    fn clearance_requires_the_same_device() {
        let stick = usb(2, 16);
        let id = stick.identity();
        let p = Policy::default();
        let c = clear(&id, std::slice::from_ref(&stick), &p, &[]).unwrap();
        assert_eq!(c.disk_number(), 2);
        assert_eq!(c.size(), stick.size);
        assert_eq!(c.letters(), &['G']);

        // Unplugged.
        assert!(matches!(clear(&id, &[], &p, &[]), Err(Refusal::Changed(_))));
        // Another stick took the same disk number.
        let mut swapped = usb(2, 16);
        swapped.serial = "OTHER".into();
        assert!(matches!(
            clear(&id, &[swapped], &p, &[]),
            Err(Refusal::Changed(_))
        ));
        let mut resized = usb(2, 32);
        resized.serial = stick.serial.clone();
        assert!(matches!(
            clear(&id, &[resized], &p, &[]),
            Err(Refusal::Changed(_))
        ));
        let mut renamed = stick.clone();
        renamed.model = "Other".into();
        assert!(clear(&id, &[renamed], &p, &[]).is_err());
        // Same device, but it has become the system disk in the meantime.
        let mut sys = stick.clone();
        sys.is_system = true;
        assert_eq!(
            clear(&id, &[sys], &p, &[]).unwrap_err(),
            Refusal::SystemDisk
        );
        // The disk number now belongs to an internal disk of identical size.
        let mut internal = stick.clone();
        internal.bus = Bus::Sata;
        assert!(matches!(
            clear(&id, &[internal], &p, &[]),
            Err(Refusal::NotExternal(_))
        ));
    }
}
