# How BootForge works

BootForge writes ISO and disk images to USB drives. This document explains what
it does to a drive, what it refuses to do, and where its limits are. It is meant
to be honest rather than flattering: read the "Limits" section before relying on
it.

## The two write modes

### Image mode (byte for byte)

The image file is copied to the drive sector by sector, starting at sector 0,
exactly like `dd`. Nothing is interpreted or changed. This is the right mode for

- **hybrid ISOs**: ISO images that also carry an MBR and/or GPT in their first
  sectors (almost every current Linux distribution), and
- **raw disk images** (`.img`, drive backups made with BootForge).

Before the copy the first and last mebibyte of the drive are zeroed so that no
stale backup GPT of the previous layout survives behind a smaller image. With
"verify" on, the drive is read back afterwards and compared chunk by chunk
(CRC-32 per 1 MiB to name the first bad offset, SHA-256 over everything).

An ISO **without** a partition table (Windows setup images, for example) is not
offered in image mode: copied raw, it would not boot from USB.

### File-copy mode (FAT32)

For ISO images that contain UEFI boot loader files (`EFI\BOOT\BOOT*.EFI`):

1. old partition tables and file-system signatures are wiped (first and last MiB),
2. a new partition table is written (GPT with protective MBR, or MBR) with one
   partition starting at 1 MiB,
3. the partition is formatted FAT32 by BootForge's own formatter,
4. every directory and file of the ISO is copied out by BootForge's own
   ISO 9660 / Joliet / Rock Ridge / UDF reader and FAT32 writer (the drive stays
   locked and dismounted the whole time; Windows never mounts a half-written
   volume),
5. with "verify" on, the partition table and the FAT32 volume are parsed back
   and checked for consistency, and every file is read back from the drive and
   compared with its source.

UEFI firmware starts such a drive through the boot loader that is already in the
image. BootForge adds no boot code of its own.

Before the drive is touched, BootForge checks that the copy can succeed at all:
every name must be storable on FAT32 (no `: * ? " < > |`, no names differing
only in upper and lower case, as some Linux media have), no single file may
exceed 4 GB (see below for `install.wim`), and the drive must be large enough.
If not, the job is refused with the reason and the drive keeps its contents.

In GPT layouts the partition has the "basic data" type so that Windows gives it
a drive letter; UEFI firmware boots removable FAT volumes regardless of the type.

#### Windows images with an install.wim over 4 GB

FAT32 cannot hold a file of 4 GiB or more. When `sources\install.wim` is larger,
BootForge

1. extracts it from the ISO to a temporary folder,
2. runs the DISM tool that is part of Windows
   (`Dism /Split-Image … /FileSize:3800`) to split it into `install.swm`,
   `install2.swm`, …,
3. copies those parts into `sources\` on the drive instead of the `.wim`.

Windows Setup reads split images natively. This needs temporary disk space of
about twice the size of the `.wim`, and DISM needs administrator rights (which
BootForge has at that point anyway). The split happens **before** the drive is
repartitioned, so a DISM failure leaves the drive as it was (apart from an
optional bad-block check, which is destructive by nature).

An `install.esd` over 4 GiB cannot be split by DISM; BootForge says so and does
not offer file-copy mode for such an image.

#### Windows Setup options

For a Windows setup image (one with `sources\boot.wim` and a setup loader) the
Create page offers three optional changes to how Setup behaves:

| Option | What the answer file contains |
| --- | --- |
| Skip TPM, Secure Boot and RAM checks (Windows 11 images) | `windowsPE` pass: `reg add HKLM\SYSTEM\Setup\LabConfig` with `BypassTPMCheck`, `BypassSecureBootCheck` and `BypassRAMCheck` set to 1, before Setup examines the PC |
| Local account, no Microsoft sign-in | `specialize` pass: `BypassNRO` = 1 under `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\OOBE`; `oobeSystem` pass: `HideOnlineAccountScreens` = true |
| Skip privacy questions | `oobeSystem` pass: `ProtectYourPC` = 3 |

When at least one is ticked, BootForge writes `autounattend.xml` into the root
of the drive next to the files of the image. Windows Setup looks for a file of
that name on the media it starts from. Nothing inside the image is modified,
no key, user name or password is put into the file, and deleting the file
from the drive undoes all of it. The file is part of the verification pass.
It is written for the CPU architecture of the image's UEFI loader (x64, ARM64
or x86).

An image that already has an `autounattend.xml` keeps it: the options are not
offered, and the engine refuses rather than overwrite it. The options are
remembered for the next Windows image.

## What the inspector reports

When an image is chosen, BootForge reads only its directory structures (fast,
even for large images) and reports

- the kind: ISO (ISO 9660, Joliet, Rock Ridge, UDF and its revision), raw disk
  image, or unknown,
- whether an ISO is a hybrid (MBR and/or GPT present) and the partitions listed
  there,
- the El Torito boot catalog entries (BIOS and/or UEFI),
- UEFI loaders present as files and the CPU architectures they are for,
- Windows setup images: `sources\install.wim` / `.esd`, its size, the editions,
  architecture and version from the WIM's XML,
- volume label, file count, total size, and notes such as "the file is smaller
  than the volume it describes".

From these facts it recommends a mode and explains why, and it disables a mode
that cannot work, with the reason.

## Other tools

- **Checksums**: MD5, SHA-1 and SHA-256 of any file in one pass, with a box to
  paste a published checksum into for comparison. When the image's folder
  holds a checksum file (`<image>.sha256`, `.sha256sum`, `.sha1`, `.md5`, or a
  list such as `SHA256SUMS`, `sha256sum.txt`, `CHECKSUM`, `SHA1SUMS`,
  `MD5SUMS`) that names the image, the listed value is filled in and the Create
  page offers to check the image against it. Only small text files in the
  image's own folder are read; nothing is downloaded.
- **Back up a drive**: reads the whole drive into an image file (and prints its
  checksums). **Restore** is image mode with that file.
- **Restore a drive to a normal empty drive**: wipe, one partition (MBR or GPT),
  FAT32 or exFAT.
- **Bad-block check** (optional, destructive): writes a pattern derived from each
  block's own address over the whole drive, reads it back, and reports damaged
  ranges and drives that fake their capacity (a block reading back another
  block's pattern).
- **Log**: everything the engine does is logged; logs are saved under
  `%LOCALAPPDATA%\BootForge\logs` (can be turned off).

## Safety

BootForge erases drives, so it is built to make mistakes hard.

**What can be chosen.** A disk is offered only when all of this is true:

- it is attached by USB or is an SD/MMC card (any other or unknown bus is refused),
- it reports removable media, or "Show USB hard drives" was switched on in
  Settings,
- it is not larger than the size limit in Settings (256 GB by default, never
  more than 2 TB),
- it does not hold Windows, the partition Windows booted from, or a page file,
- it does not hold the running BootForge program,
- it does not hold the image that is about to be written (or the backup file
  being created).

Internal disks and the system disk are not merely hidden: the same rule set is
applied again inside the engine immediately before the drive is opened, and the
raw-disk layer cannot be asked to open a disk for writing without a clearance
object that only that check can create. The rules have unit tests
(`src/safety.rs`).

**Confirmation.** Before anything destructive a dialog names the device model,
size, connection, and every partition with drive letter, volume label and file
system, and requires ticking "I understand…" first.

**Identity re-check.** Between confirmation and the first write the disk list is
read again; the disk number, size, model and serial number must be unchanged,
and they are compared once more on the opened handle. If a drive was swapped in
the meantime, nothing is written.

The raw-disk layer additionally refuses, on its own, any disk carrying the
volume Windows or BootForge runs from, and the engine asks Windows which disk
really holds the image file (so an image reached through a mount point or a
substituted drive letter is caught too).

**Locking.** Every volume on the drive is locked and dismounted before the first
write and stays locked until the end; if any volume cannot be locked (a file on
it is open somewhere), everything locked so far is released and the job stops
with a message naming the volume. Unlocking happens on success, error and
cancel alike.

**Cancel** stops between chunks. A cancelled write leaves a drive with partial
contents: BootForge says so, and "Restore a drive to a normal empty drive" makes
it usable again.

**Sleep.** While a job runs, BootForge asks Windows not to put the PC to sleep
(`SetThreadExecutionState`); the request ends with the job.

**Administrator rights** are needed for raw disk access. BootForge itself starts
without them (so the checksum and inspection tools need no elevation) and
restarts elevated when you ask it to write.

## Limits (please read)

- **Not tested on real hardware.** During development BootForge was never
  allowed to write to a real drive. Everything destructive was exercised against
  image files through the same block-device interface: the raw writer, the
  partitioner, the FAT32/exFAT formatters, the ISO extractor and the verifier
  are covered by unit tests and by `BootForge.exe --selftest`, which writes
  synthetic ISOs to image files in both modes and parses the results back. The
  thin Windows layer that opens `\\.\PhysicalDriveN`, locks volumes and ejects
  the drive was compiled and reviewed but **never executed**. No drive produced
  by BootForge has been booted on a real computer yet.
- **The Windows Setup options were not tried on a real installation.** The
  answer file uses documented settings and the registry values Setup itself
  reads, and tests check its structure and that it arrives on the drive
  unchanged, but no Windows installation was run with it during development.
  Microsoft changes Setup from release to release; an option that a future
  Windows version ignores simply has no effect.
- **File-copy mode is UEFI only.** Booting a BIOS/CSM machine from copied files
  needs a boot sector and boot loader that BootForge does not ship (see NOTICE).
  For BIOS booting use image mode with a hybrid ISO. Windows 7-era and other
  BIOS-only installers that are not hybrid cannot be made bootable.
- **No NTFS.** There is no NTFS formatter and no "NTFS partition + small EFI
  partition with a UEFI NTFS driver" layout, because that needs a third-party
  driver binary. Large Windows images are handled by splitting the `.wim`
  instead; oversized `.esd` images and any other single file over 4 GB cannot be
  written in file-copy mode.
- **Secure Boot** is neither helped nor hindered: what boots is whatever loader
  the image contains.
- **Linux images in file-copy mode** often work, but some distributions locate
  their files by volume label or expect the exact ISO layout; BootForge keeps
  the label (shortened to FAT32's 11 characters and warns when that shortens
  it). Image mode is the reliable choice for hybrid ISOs and is the default for
  them. Persistent storage for live systems is not offered.
- **UDF support** covers what installation media use: UDF 1.02 to 2.01 with a
  plain physical partition. Discs using virtual, sparable or metadata
  partitions (packet-written or Blu-ray style UDF 2.50+) are detected and, if
  the image also has an ISO 9660/Joliet tree, read through that instead.
- **exFAT formatter**: written from the published specification and validated by
  BootForge's own parser only; it has not been checked by Windows on a real
  drive. FAT32 is the default for "restore to an empty drive".
- **Compressed images** (`.gz`, `.xz`, `.zip`, …) are recognised but not
  decompressed; unpack them first.
- **Multi-boot drives**, Windows To Go, persistence, and downloading images are
  out of scope.
- **4Kn drives** (4096-byte logical sectors) are supported by the partitioner
  and formatters in tests, but are rare among USB sticks and equally untested
  on hardware.
- The **bad-block check** catches write/read mismatches and address wrapping; it
  is not a full burn-in test and takes about as long as writing and reading the
  whole drive once.

## Command line

```
BootForge.exe --demo                 invented drives and images; nothing is touched
BootForge.exe --selftest             write synthetic ISOs to image files, verify
        [--keep <folder>]            keep those image files for inspection
BootForge.exe --inspect <file>       print what is known about an image
BootForge.exe --hash <file>          MD5, SHA-1, SHA-256
        [--expect <checksum>]        compare; exit code 2 when it does not match
                                     (without it: a checksum file next to the file)
BootForge.exe --version              print the version
BootForge.exe --image <file> [--disk <n>]   open with an image (and a disk) preselected
BootForge.exe --screenshot <png> [--view …] [--light]
```

## How it was checked

- `cargo test`: unit tests for every module, including corrupt-input tests for
  all parsers, CRC known-answer tests for GPT, byte-level checks of the FAT32
  and exFAT structures, and the refusal rules.
- The ISO/UDF reader is additionally tested against small images produced by an
  independent writer (the IMAPI2 component of Windows): ISO 9660, Joliet,
  UDF 1.02 / 1.50 / 2.01, and El Torito BIOS/UEFI boot images
  (`tests/fixtures`).
- `BootForge.exe --selftest` runs both write modes end to end on image files.
- `scripts/crosscheck.py` is a separate, deliberately simple Python reader for
  GPT (CRC-32 from zlib), MBR and FAT32 (long names, FSInfo, both FATs). Run on
  the images kept by `--selftest --keep <folder>`, it confirms that the
  partition tables and FAT32 volumes written by BootForge are readable by code
  that shares nothing with BootForge's own readers.

None of this replaces booting a real computer from a real drive, which has not
been done.

## Source layout

| Module | Purpose |
| --- | --- |
| `blockdev` | `BlockDevice` / `ReadAt` traits, file- and memory-backed devices |
| `safety` | which disks may be written; identity re-check; clearance |
| `device` | read-only disk enumeration, hot-plug watcher, locked raw disk access |
| `iso` | ISO 9660 / Joliet / Rock Ridge / UDF / El Torito reader, WIM metadata, image inspection, synthetic image builder for tests |
| `part` | MBR and GPT writer and reader |
| `fat32`, `exfat` | formatters, FAT32 writer and consistency checker |
| `imaging`, `hash` | raw write, verify, backup, bad-block check, checksums |
| `plan` | advice: which mode, why, what blocks a write |
| `unattend` | the Windows Setup answer file |
| `engine` | jobs, stages, progress, the orchestration of all of the above |
| `ui` | the egui interface |
| `selftest`, `demo` | `--selftest`, invented data for `--demo` |
