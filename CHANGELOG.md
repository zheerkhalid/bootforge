# Changelog

All notable changes to BootForge are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the version numbers follow
[Semantic Versioning](https://semver.org/).

## [1.1.0] - 2026-11-24

### Added

- **Windows Setup options.** When you make a drive from a Windows setup image in file-copy mode,
  three new switches appear under Options:
  - *Windows 11 checks* skips the TPM 2.0, Secure Boot and memory checks, so that Windows 11 Setup
    continues on a PC that does not meet those requirements (Windows 11 images only).
  - *Local account* makes the first start ask for a local user name instead of a Microsoft
    account, and work without an internet connection.
  - *Privacy questions* answers the location, diagnostics and advertising questions with "no"
    and does not show them.

  BootForge does this by adding one small answer file (`autounattend.xml`) to the drive. The files
  of the image are not changed, the added file is verified like every other file, and deleting it
  from the drive gives the plain Windows Setup back. The confirmation dialog and the result page
  say which options were applied. Images that bring an answer file of their own keep it, and the
  options are not offered for them.
- **Checksum files are found for you.** If the folder of an image also holds the checksum file its
  publisher provides (`SHA256SUMS`, `sha256sum.txt`, `MD5SUMS`, `image.iso.sha256` and similar),
  BootForge reads the value listed for your image. The Checksums page fills it in, and the image
  card on the Create page offers "Check the image against it now" and then shows either
  "Checksum verified" or a clear warning that the download is damaged.
- **Recent images.** The empty Create page lists the last five images you used; one click selects
  an image again. Entries whose file is gone are removed when clicked.
- **The taskbar button flashes when a job ends** while you are working in another window.
- **Command line:** `BootForge.exe --hash <file> --expect <checksum>` compares and ends with exit
  code 2 when the file does not match (useful in scripts). Without `--expect`, a checksum file
  next to the file is used when there is one. `BootForge.exe --version` prints the version.
- The built-in self-test (`--selftest`) now also writes a drive image with the Windows Setup
  options switched on and reads the answer file back.

### Changed

- **The PC no longer goes to sleep while a job runs.** Writing, verifying, backing up and the
  bad-block check keep Windows awake until they have finished (the screen may still switch off).
- The Windows Setup choices are remembered and offered again for the next Windows image.
- The window opens a little taller (1280 x 860) so that all options of a Windows image fit
  without scrolling.
- The packaging script (`scripts/package.ps1`) uses the folder named in `CARGO_TARGET_DIR` when
  that is set, and removes portable zips of earlier versions from `dist`.

### Fixed

- **Restarting as administrator lost an image given with a relative path.** Starting BootForge
  with `--image some.iso` from a folder and then pressing "Restart as administrator" opened the
  elevated window without the image, because Windows starts elevated programs in another folder.
  The path is now made complete at start.
- **Needless redrawing during steps without a known length.** While a step with no byte count
  was running (a quick format, for example), every progress update was passed on at once instead
  of at most twenty times a second, which kept the window busy for nothing.

## [1.0.0] - 2026-11-16

First release.

- Inspects an image and recommends how to write it: ISO 9660, Joliet, Rock Ridge and UDF, BIOS
  and UEFI boot entries, hybrid images, Windows setup images with their editions and version.
- Two write modes: image mode (byte for byte) for hybrid Linux ISOs and raw disk images, and
  file-copy mode (GPT or MBR, FAT32) for UEFI boot, with an oversized `install.wim` split by the
  DISM tool of Windows.
- Verify after writing, and an optional bad-block check that also detects fake-capacity drives.
- Drive tools: back up a drive to an image file, restore it, reset a drive to an empty FAT32 or
  exFAT drive.
- Checksums page: MD5, SHA-1 and SHA-256 with a box to paste the published value into.
- Safety: only removable USB drives are offered, the drive is named in a confirmation and checked
  again right before the first write.
- Log page with saved logs, dark and light theme, demo mode, installer and portable build.

[1.1.0]: https://github.com/zheerkhalid/bootforge/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/zheerkhalid/bootforge/releases/tag/v1.0.0
