//! Tests for the image readers: round trips through the synthetic builder,
//! images produced independently by the Windows IMAPI2 file system imaging
//! API (stored zero-run compressed under `tests/fixtures`), and damaged input.

use super::build::{
    fake_efi_binary, fake_wim, fat12_image, pattern, sample_linux_hybrid, sample_windows_setup,
    sample_windows_setup_sparse, seal_tag, IsoBuilder,
};
use super::*;
use crate::blockdev::MemSource;
use crate::model::{Arch, BootPlatform, ImageKind};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

// --------------------------------------------------------------------------------------
// Helpers
// --------------------------------------------------------------------------------------

fn open(bytes: Vec<u8>) -> IsoImage {
    IsoImage::open(Box::new(MemSource(bytes))).expect("image opens")
}

fn try_open(bytes: Vec<u8>) -> Result<IsoImage> {
    IsoImage::open(Box::new(MemSource(bytes)))
}

fn inspect_bytes(bytes: Vec<u8>) -> crate::model::ImageInfo {
    inspect_source(Path::new("test.iso"), Box::new(MemSource(bytes))).expect("inspect")
}

fn content(image: &IsoImage, path: &str) -> Vec<u8> {
    let e = image
        .find(path)
        .unwrap_or_else(|| panic!("{path} not found"));
    let mut v = Vec::new();
    image.open_file(e).read_to_end(&mut v).expect("file reads");
    assert_eq!(v.len() as u64, e.size);
    v
}

fn file_paths(image: &IsoImage) -> Vec<String> {
    let mut v: Vec<String> = image
        .entries()
        .iter()
        .filter(|e| !e.is_dir)
        .map(|e| e.path.clone())
        .collect();
    v.sort();
    v
}

/// Decodes a fixture: repeated (u32 literal length, literal, u32 zero run).
fn unrle(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut p = 0;
    while p + 4 <= data.len() {
        let lit = le32(data, p) as usize;
        p += 4;
        out.extend_from_slice(&data[p..p + lit]);
        p += lit;
        let zeros = le32(data, p) as usize;
        p += 4;
        out.resize(out.len() + zeros, 0);
    }
    out
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len())
        .position(|w| w == needle)
        .expect("needle present")
}

const UNI: &str = "Gr\u{fc}\u{df}e-\u{3a9}mega-\u{65e5}\u{672c}.txt";
const LONG: &str = "A rather long file name with spaces, Mixed Case and (punctuation).text";

/// The files most round-trip tests use.
fn standard_files() -> Vec<(String, Vec<u8>)> {
    vec![
        ("readme.txt".to_string(), b"hello\r\n".to_vec()),
        ("empty.dat".to_string(), Vec::new()),
        (UNI.to_string(), pattern(77, 11, 4)),
        (LONG.to_string(), pattern(1234, 5, 2)),
        ("docs/guide.txt".to_string(), pattern(300, 3, 1)),
        (
            "docs/a/b/c/d/e/f/g/h/deep.bin".to_string(),
            pattern(5000, 7, 13),
        ),
        ("data/big.bin".to_string(), pattern(70_000, 13, 7)),
        ("data/exact.bin".to_string(), pattern(4096, 17, 9)),
    ]
}

fn with_files(mut b: IsoBuilder, files: &[(String, Vec<u8>)]) -> IsoBuilder {
    for (p, d) in files {
        b = b.file(p, d.clone());
    }
    b
}

/// Checks that the image shows exactly `files` with the right contents.
fn check_exact(image: &IsoImage, files: &[(String, Vec<u8>)]) {
    let mut want: Vec<String> = files.iter().map(|(p, _)| p.clone()).collect();
    want.sort();
    assert_eq!(file_paths(image), want);
    for (p, d) in files {
        assert_eq!(&content(image, p), d, "content of {p}");
        let e = image.find(p).unwrap();
        assert_eq!(e.size, d.len() as u64);
        assert_eq!(e.extents.iter().map(|x| x.len).sum::<u64>(), e.size);
        let t = e.mtime.expect("mtime");
        assert_eq!(
            (t.year, t.month, t.day, t.hour, t.minute, t.second),
            (2024, 1, 2, 3, 4, 5)
        );
    }
    // Every directory precedes its children.
    let mut seen: Vec<&str> = Vec::new();
    for e in image.entries() {
        if let Some((parent, _)) = e.path.rsplit_once('/') {
            assert!(seen.contains(&parent), "{} before its parent", e.path);
        }
        if e.is_dir {
            seen.push(&e.path);
            assert!(e.extents.is_empty() && e.size == 0);
        }
    }
}

// --------------------------------------------------------------------------------------
// Round trips: builder -> reader
// --------------------------------------------------------------------------------------

#[test]
fn plain_iso9660_round_trip() {
    let image = open(
        IsoBuilder::new("plain vol")
            .file("readme.txt", b"hello".to_vec())
            .file("empty.dat", Vec::new())
            .file("noext", b"x".to_vec())
            .file("docs/guide.txt", pattern(3000, 3, 1))
            .file("docs/sub/data.bin", pattern(5000, 7, 13))
            .dir("hollow")
            .build(),
    );
    assert_eq!(image.name_space(), NameSpace::Iso9660);
    assert_eq!(image.label(), "PLAIN_VOL");
    assert!(image.has_iso9660() && !image.has_joliet() && !image.has_rock_ridge());
    assert_eq!(image.udf_revision(), None);
    assert!(image.notes().is_empty(), "{:?}", image.notes());
    assert_eq!(
        file_paths(&image),
        [
            "DOCS/GUIDE.TXT",
            "DOCS/SUB/DATA.BIN",
            "EMPTY.DAT",
            "NOEXT",
            "README.TXT"
        ]
    );
    assert!(image.find("hollow").unwrap().is_dir);
    assert_eq!(content(&image, "readme.txt"), b"hello");
    assert_eq!(content(&image, "docs\\sub\\DATA.bin"), pattern(5000, 7, 13));
    assert_eq!(content(&image, "/Docs/Guide.txt/"), pattern(3000, 3, 1));
    assert!(content(&image, "EMPTY.DAT").is_empty());
    assert!(image.find("missing.txt").is_none());
    assert_eq!(image.volume_size(), Some(image.image_size()));
    assert_eq!(image.find("DOCS/GUIDE.TXT").unwrap().name(), "GUIDE.TXT");
}

#[test]
fn joliet_round_trip() {
    let files = standard_files();
    let image = open(with_files(IsoBuilder::new("Joliet Label").joliet(true), &files).build());
    assert_eq!(image.name_space(), NameSpace::Joliet);
    assert_eq!(image.label(), "Joliet Label");
    assert_eq!(image.iso_label(), Some("JOLIET_LABEL"));
    assert!(image.has_joliet() && !image.has_rock_ridge());
    assert!(image.notes().is_empty(), "{:?}", image.notes());
    check_exact(&image, &files);
}

#[test]
fn rock_ridge_round_trip_with_continuation_areas() {
    let mut files = standard_files();
    // Too long for one directory record: continues in a CE area, twice over.
    let very_long = format!("{}.bin", "n".repeat(120) + &"\u{e9}".repeat(60));
    let huge = format!("{}.x", "z".repeat(240));
    files.push((very_long, pattern(10, 1, 1)));
    files.push((format!("docs/{huge}"), pattern(20, 1, 2)));
    let image = open(with_files(IsoBuilder::new("RR").rock_ridge(true), &files).build());
    assert_eq!(image.name_space(), NameSpace::RockRidge);
    assert!(image.has_rock_ridge() && !image.has_joliet());
    assert!(image.notes().is_empty(), "{:?}", image.notes());
    check_exact(&image, &files);
}

#[test]
fn rock_ridge_is_flagged_but_joliet_wins() {
    let files = standard_files();
    let image = open(
        with_files(
            IsoBuilder::new("BOTH").rock_ridge(true).joliet(true),
            &files,
        )
        .build(),
    );
    assert_eq!(image.name_space(), NameSpace::Joliet);
    assert!(image.has_rock_ridge() && image.has_joliet());
    check_exact(&image, &files);
}

#[test]
fn udf_round_trip_in_every_variant() {
    let files = standard_files();
    for long in [false, true] {
        for embed in [false, true] {
            for extended in [false, true] {
                for bridge in [false, true] {
                    let b = IsoBuilder::new("UDF Volume \u{3a9}")
                        .iso9660(bridge)
                        .joliet(bridge)
                        .udf(true)
                        .udf_long_ads(long)
                        .udf_embed(embed)
                        .udf_extended_entries(extended)
                        .dir("hollow");
                    let image = open(with_files(b, &files).build());
                    let what = format!("long={long} embed={embed} ext={extended} bridge={bridge}");
                    assert_eq!(image.name_space(), NameSpace::Udf, "{what}");
                    assert_eq!(image.udf_revision(), Some("1.02"), "{what}");
                    assert_eq!(image.label(), "UDF Volume \u{3a9}", "{what}");
                    assert_eq!(image.has_iso9660(), bridge);
                    assert_eq!(image.has_joliet(), bridge);
                    assert!(image.notes().is_empty(), "{what}: {:?}", image.notes());
                    assert!(image.find("hollow").unwrap().is_dir);
                    check_exact(&image, &files);
                    if embed {
                        // Embedded data is not sector aligned.
                        let e = image.find("readme.txt").unwrap();
                        assert_ne!(e.extents[0].offset % 2048, 0);
                    }
                }
            }
        }
    }
}

#[test]
fn windows_style_shows_the_udf_tree() {
    let files = standard_files();
    let bytes = with_files(IsoBuilder::new("WIN").windows_style(true), &files).build();
    let image = open(bytes.clone());
    assert_eq!(image.name_space(), NameSpace::Udf);
    assert!(image.has_iso9660() && !image.has_joliet());
    check_exact(&image, &files);

    // Without the UDF anchors only the ISO 9660 readme is left.
    let mut broken = bytes;
    let n = broken.len() / 2048;
    for lba in [256, n - 1] {
        broken[lba * 2048..lba * 2048 + 16].fill(0);
    }
    let image = open(broken);
    assert_eq!(image.name_space(), NameSpace::Iso9660);
    assert_eq!(file_paths(&image), ["README.TXT"]);
    assert!(content(&image, "README.TXT").starts_with(b"This disc contains"));
}

#[test]
fn multi_extent_files_in_every_name_space() {
    let files = vec![
        ("split.bin".to_string(), pattern(20_000, 7, 3)),
        ("dir/exact.bin".to_string(), pattern(8192, 5, 1)),
        ("small.txt".to_string(), b"tiny".to_vec()),
    ];
    let base = || with_files(IsoBuilder::new("MULTI").extent_size(4096), &files);
    let variants: Vec<(IsoBuilder, NameSpace)> = vec![
        (base().rock_ridge(true), NameSpace::RockRidge),
        (base().joliet(true), NameSpace::Joliet),
        (base().udf(true), NameSpace::Udf),
        (base().udf(true).udf_long_ads(true), NameSpace::Udf),
        (base().udf(true).udf_inline_ads(2), NameSpace::Udf),
        (
            base()
                .iso9660(false)
                .udf_inline_ads(3)
                .udf_long_ads(true)
                .udf_extended_entries(true),
            NameSpace::Udf,
        ),
    ];
    for (b, ns) in variants {
        let image = open(b.build());
        assert_eq!(image.name_space(), ns);
        assert!(image.notes().is_empty(), "{:?}", image.notes());
        check_exact(&image, &files);
        // The builder leaves a filler sector between pieces, so they cannot merge.
        let e = image.find("split.bin").unwrap();
        assert_eq!(e.extents.len(), 5, "{ns:?}");
        assert_eq!(e.extents[4].len, 20_000 - 4 * 4096);
        assert_eq!(image.find("dir/exact.bin").unwrap().extents.len(), 2);

        // Random access across the extent boundaries.
        let want = pattern(20_000, 7, 3);
        let mut buf = vec![0u8; 9000];
        image.read_at(e, 4000, &mut buf).unwrap();
        assert_eq!(buf, want[4000..13_000]);
        assert!(image.read_at(e, 19_999, &mut [0u8; 2]).is_err());
        let mut r = image.open_file(e);
        r.seek(SeekFrom::Start(12_288)).unwrap();
        let mut tail = Vec::new();
        r.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, want[12_288..]);
        assert_eq!(r.seek(SeekFrom::End(-4)).unwrap(), 19_996);
    }
    // Plain ISO 9660 names.
    let image = open(base().build());
    assert_eq!(content(&image, "SPLIT.BIN"), pattern(20_000, 7, 3));
    assert_eq!(image.find("SPLIT.BIN").unwrap().extents.len(), 5);
}

#[test]
fn contiguous_pieces_are_merged() {
    let mut b = IsoBuilder::new("M").udf(true);
    b = b.file("one.bin", pattern(10_000, 3, 3));
    let image = open(b.build());
    assert_eq!(image.find("one.bin").unwrap().extents.len(), 1);
}

#[test]
fn large_directories_and_deep_trees() {
    let mut files = Vec::new();
    for i in 0..300 {
        files.push((
            format!("many/file number {i:03} of three hundred.txt"),
            pattern(i, 3, i as u8),
        ));
    }
    let deep: String = (0..40).map(|i| format!("level{i}/")).collect();
    files.push((format!("{deep}bottom.txt"), b"deep".to_vec()));
    for b in [
        IsoBuilder::new("BIG").joliet(true),
        IsoBuilder::new("BIG").rock_ridge(true),
        IsoBuilder::new("BIG").udf(true),
        IsoBuilder::new("BIG").udf(true).udf_embed(true),
    ] {
        let image = open(with_files(b, &files).build());
        assert!(image.notes().is_empty(), "{:?}", image.notes());
        check_exact(&image, &files);
    }
    // Order within a directory is the recorded one (UDF keeps insertion order).
    let image = open(with_files(IsoBuilder::new("BIG").udf(true), &files).build());
    let names: Vec<&str> = image
        .entries()
        .iter()
        .filter(|e| e.path.starts_with("many/"))
        .map(|e| e.path.as_str())
        .collect();
    let want: Vec<&str> = files[..300].iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(names, want);
}

#[test]
fn sparse_udf_files_read_as_zeros() {
    let head = pattern(3000, 9, 1);
    let total = 5 * 1024 * 1024 * 1024u64 + 123;
    let image = open(
        IsoBuilder::new("SPARSE")
            .udf(true)
            .file("normal.txt", b"n".to_vec())
            .sparse_file("big/huge.bin", head.clone(), total)
            .sparse_file("hole.bin", Vec::new(), 10_000)
            .build(),
    );
    let e = image.find("big/huge.bin").unwrap();
    assert_eq!(e.size, total);
    assert_eq!(e.extents.len(), 2);
    assert!(!e.extents[0].sparse && e.extents[1].sparse);
    assert_eq!(e.extents.iter().map(|x| x.len).sum::<u64>(), total);
    let mut buf = vec![1u8; 5000];
    image.read_at(e, 0, &mut buf).unwrap();
    assert_eq!(buf[..3000], head[..]);
    assert!(buf[3000..].iter().all(|b| *b == 0));
    let mut buf = vec![1u8; 4096];
    image.read_at(e, total - 4096, &mut buf).unwrap();
    assert!(buf.iter().all(|b| *b == 0));
    assert_eq!(content(&image, "hole.bin"), vec![0u8; 10_000]);
    // Sparse files exist only in the UDF tree.
    assert!(image.find("normal.txt").is_some());
}

#[test]
fn builder_output_is_deterministic() {
    assert_eq!(sample_linux_hybrid(), sample_linux_hybrid());
    assert_eq!(sample_windows_setup(100_000), sample_windows_setup(100_000));
}

#[test]
fn hostile_names_are_made_safe() {
    assert_eq!(clean_name(".."), "__");
    assert_eq!(clean_name("."), "_");
    assert_eq!(clean_name(""), "_");
    assert_eq!(clean_name("a/b\\c\u{0}d"), "a_b_c_d");
    // A UDF name with a slash in it must not create a path component.
    let mut bytes = IsoBuilder::new("N")
        .iso9660(false)
        .file("ab.txt", b"x".to_vec())
        .build();
    let at = find_bytes(&bytes, b"\x08ab.txt");
    bytes[at + 2] = b'/';
    let image = open(bytes);
    assert_eq!(file_paths(&image), ["a_.txt"]);
}

// --------------------------------------------------------------------------------------
// Independently produced images (Windows IMAPI2)
// --------------------------------------------------------------------------------------

fn imapi_files() -> Vec<(String, Vec<u8>)> {
    vec![
        (
            "readme.txt".to_string(),
            b"BootForge reader fixture\r\n".to_vec(),
        ),
        ("Docs/Guide Notes.txt".to_string(), pattern(300, 3, 1)),
        (
            "Docs/deep/level3/level4/data.bin".to_string(),
            pattern(5000, 7, 13),
        ),
        (
            "Long file name with spaces and UPPER lower.txt".to_string(),
            pattern(1234, 5, 2),
        ),
        (UNI.to_string(), pattern(77, 11, 4)),
        ("empty.dat".to_string(), Vec::new()),
        ("big.bin".to_string(), pattern(70_000, 13, 7)),
        ("boot.img".to_string(), pattern(2048, 17, 9)),
    ]
}

/// Names, sizes and contents must match exactly (long-name name spaces).
fn check_imapi_exact(image: &IsoImage) {
    let files = imapi_files();
    let mut want: Vec<String> = files.iter().map(|(p, _)| p.clone()).collect();
    want.sort();
    assert_eq!(file_paths(image), want);
    for (p, d) in &files {
        assert_eq!(&content(image, p), d, "content of {p}");
    }
    for dir in [
        "Docs",
        "Docs/deep",
        "Docs/deep/level3",
        "Docs/deep/level3/level4",
    ] {
        assert!(image.find(dir).unwrap().is_dir);
    }
    assert!(image.entries().iter().all(|e| e.mtime.is_some()));
}

#[test]
fn imapi_iso9660_only() {
    let bytes = unrle(include_bytes!("../../tests/fixtures/imapi_iso.iso.rle"));
    assert_eq!(bytes.len(), 141_312);
    let image = open(bytes);
    assert_eq!(image.name_space(), NameSpace::Iso9660);
    assert!(image.has_iso9660() && !image.has_joliet() && !image.has_rock_ridge());
    assert_eq!(image.udf_revision(), None);
    assert_eq!(image.label(), "BF_FIXTURE");
    assert!(image.notes().is_empty(), "{:?}", image.notes());
    assert_eq!(image.volume_size(), Some(141_312));
    // Names that fit 8.3 are recorded as they are, in upper case.
    let files = imapi_files();
    for (name, original) in [
        ("README.TXT", 0),
        ("EMPTY.DAT", 5),
        ("BIG.BIN", 6),
        ("BOOT.IMG", 7),
        ("DOCS/DEEP/LEVEL3/LEVEL4/DATA.BIN", 2),
    ] {
        assert_eq!(content(&image, name), files[original].1, "{name}");
        assert_eq!(image.find(name).unwrap().path, name);
    }
    // The other names are mangled by IMAPI2; match them by their unique sizes.
    let listed = file_paths(&image);
    assert_eq!(listed.len(), files.len());
    for (path, data) in &files {
        let e = image
            .entries()
            .iter()
            .find(|e| !e.is_dir && e.size == data.len() as u64)
            .unwrap_or_else(|| panic!("no file with the size of {path}"));
        assert_eq!(&content(&image, &e.path), data);
        assert!(
            !e.path.contains(';') && !e.path.ends_with('.'),
            "{}",
            e.path
        );
    }
}

#[test]
fn imapi_joliet() {
    let image = open(unrle(include_bytes!(
        "../../tests/fixtures/imapi_joliet.iso.rle"
    )));
    assert_eq!(image.name_space(), NameSpace::Joliet);
    assert!(image.has_iso9660() && image.has_joliet());
    assert_eq!(image.label(), "BF_FIXTURE");
    assert!(image.notes().is_empty(), "{:?}", image.notes());
    check_imapi_exact(&image);
}

#[test]
fn imapi_udf_revisions() {
    for (bytes, revision) in [
        (
            &include_bytes!("../../tests/fixtures/imapi_udf102.iso.rle")[..],
            "1.02",
        ),
        (
            &include_bytes!("../../tests/fixtures/imapi_udf150.iso.rle")[..],
            "1.50",
        ),
        (
            &include_bytes!("../../tests/fixtures/imapi_udf201.iso.rle")[..],
            "2.01",
        ),
    ] {
        let image = open(unrle(bytes));
        assert_eq!(image.name_space(), NameSpace::Udf, "{revision}");
        assert_eq!(image.udf_revision(), Some(revision));
        assert!(!image.has_iso9660() && !image.has_joliet());
        assert_eq!(image.label(), "BF_FIXTURE");
        // No CRC or checksum complaints: the tag arithmetic agrees with IMAPI2.
        assert!(image.notes().is_empty(), "{revision}: {:?}", image.notes());
        // IMAPI2 pads the image past the end of the partition.
        assert!(image.volume_size().unwrap() <= image.image_size());
        check_imapi_exact(&image);
    }
}

#[test]
fn imapi_bridge_prefers_udf() {
    let bytes = unrle(include_bytes!("../../tests/fixtures/imapi_all.iso.rle"));
    let image = open(bytes.clone());
    assert_eq!(image.name_space(), NameSpace::Udf);
    assert!(image.has_iso9660() && image.has_joliet());
    assert!(image.udf_revision().is_some());
    assert!(image.notes().is_empty(), "{:?}", image.notes());
    check_imapi_exact(&image);
    let info = inspect_bytes(bytes);
    assert_eq!(info.kind, ImageKind::Iso);
    assert_eq!(info.file_count, 8);
    assert_eq!(
        info.total_file_bytes,
        26 + 300 + 5000 + 1234 + 77 + 70_000 + 2048
    );
    assert_eq!(info.largest_file, Some(("big.bin".to_string(), 70_000)));
    assert!(info.notes.is_empty(), "{:?}", info.notes);
}

#[test]
fn imapi_udf_250_falls_back_to_joliet() {
    let image = open(unrle(include_bytes!(
        "../../tests/fixtures/imapi_udf250.iso.rle"
    )));
    assert_eq!(image.udf_revision(), Some("2.50"));
    assert_eq!(image.name_space(), NameSpace::Joliet);
    assert!(
        image
            .notes()
            .iter()
            .any(|n| n.contains("unsupported UDF variant")),
        "{:?}",
        image.notes()
    );
    check_imapi_exact(&image);
}

#[test]
fn imapi_el_torito() {
    let bytes = unrle(include_bytes!("../../tests/fixtures/imapi_boot.iso.rle"));
    let image = open(bytes.clone());
    assert_eq!(image.name_space(), NameSpace::Joliet);
    check_imapi_exact(&image);
    let boot = image.boot_entries();
    assert_eq!(boot.len(), 1);
    assert_eq!(boot[0].platform, BootPlatform::Bios);
    assert!(boot[0].bootable);
    assert_eq!(boot[0].emulation, "no emulation");
    assert!(boot[0].sectors >= 1);
    let mut loaded = vec![0u8; 2048];
    image
        .read_image(boot[0].lba as u64 * 2048, &mut loaded)
        .unwrap();
    assert_eq!(loaded, pattern(2048, 17, 9));
    let via_module = eltorito::read_boot_entries(&mut MemSource(bytes))
        .unwrap()
        .unwrap();
    assert_eq!(via_module, boot);

    let image = open(unrle(include_bytes!(
        "../../tests/fixtures/imapi_bootefi.iso.rle"
    )));
    assert_eq!(image.boot_entries().len(), 1);
    assert_eq!(image.boot_entries()[0].platform, BootPlatform::Uefi);
    let info = inspect_bytes(unrle(include_bytes!(
        "../../tests/fixtures/imapi_bootefi.iso.rle"
    )));
    assert!(info.uefi_el_torito() && !info.bios_el_torito());
    // The boot image is not a FAT volume: that is a note, not an error.
    assert!(
        info.notes.iter().any(|n| n.contains("FAT")),
        "{:?}",
        info.notes
    );
}

// --------------------------------------------------------------------------------------
// El Torito, partition tables, WIM
// --------------------------------------------------------------------------------------

fn catalog(entries: &[[u8; 32]]) -> Vec<u8> {
    let mut c = vec![0u8; 2048];
    for (i, e) in entries.iter().enumerate() {
        c[i * 32..(i + 1) * 32].copy_from_slice(e);
    }
    c
}

fn validation(platform: u8) -> [u8; 32] {
    let mut v = [0u8; 32];
    v[0] = 1;
    v[1] = platform;
    v[30] = 0x55;
    v[31] = 0xAA;
    let mut sum = 0u16;
    for i in 0..16 {
        sum = sum.wrapping_add(le16(&v, i * 2));
    }
    v[28..30].copy_from_slice(&0u16.wrapping_sub(sum).to_le_bytes());
    v
}

fn boot_entry(indicator: u8, media: u8, count: u16, lba: u32) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[0] = indicator;
    e[1] = media;
    e[6..8].copy_from_slice(&count.to_le_bytes());
    e[8..12].copy_from_slice(&lba.to_le_bytes());
    e
}

fn section(last: bool, platform: u8, count: u16) -> [u8; 32] {
    let mut h = [0u8; 32];
    h[0] = if last { 0x91 } else { 0x90 };
    h[1] = platform;
    h[2..4].copy_from_slice(&count.to_le_bytes());
    h
}

#[test]
fn el_torito_catalog_parsing() {
    let mut extension = [0u8; 32];
    extension[0] = 0x44;
    let c = catalog(&[
        validation(0),
        boot_entry(0x88, 0, 4, 100),
        section(false, 0xEF, 2),
        boot_entry(0x88, 0x20, 2880, 200),
        extension,
        boot_entry(0x00, 4, 1, 300),
        section(true, 0x01, 1),
        boot_entry(0x88, 2, 1, 400),
        boot_entry(0x88, 0, 1, 999), // after the final section: ignored
    ]);
    let e = eltorito::parse_catalog(&c).unwrap();
    assert_eq!(e.len(), 4);
    assert_eq!(
        (e[0].platform, e[0].lba, e[0].sectors),
        (BootPlatform::Bios, 100, 4)
    );
    assert_eq!(e[0].emulation, "no emulation");
    assert_eq!(
        (e[1].platform, e[1].lba, e[1].sectors),
        (BootPlatform::Uefi, 200, 2880)
    );
    assert!(e[1].bootable);
    assert_eq!((e[2].platform, e[2].bootable), (BootPlatform::Uefi, false));
    assert_eq!(e[2].emulation, "hard disk");
    assert_eq!(e[3].platform, BootPlatform::Other(1));
    assert_eq!(e[3].emulation, "1.44 MB floppy");

    let mut bad = c.clone();
    bad[5] ^= 1;
    assert!(eltorito::parse_catalog(&bad)
        .unwrap_err()
        .to_string()
        .contains("checksum"));
    let mut bad = c.clone();
    bad[31] = 0;
    assert!(eltorito::parse_catalog(&bad).is_err());
    assert!(eltorito::parse_catalog(&c[..40]).is_err());
    // A section header promising more entries than there is room for.
    let c = catalog(&[
        validation(0xEF),
        boot_entry(0x88, 0, 1, 50),
        section(false, 0, 60_000),
    ]);
    assert_eq!(eltorito::parse_catalog(&c).unwrap().len(), 1);
}

#[test]
fn a_bad_boot_catalog_is_only_a_note() {
    let good = sample_linux_hybrid();
    let image = open(good.clone());
    assert_eq!(image.boot_entries().len(), 2);
    let catalog_lba = le32(&good, 17 * 2048 + 71) as usize;
    let mut bad = good.clone();
    bad[catalog_lba * 2048 + 4] ^= 0x40;
    let image = open(bad);
    assert!(image.boot_entries().is_empty());
    assert!(image.notes().iter().any(|n| n.contains("boot catalog")));
    assert!(image.find("casper/vmlinuz").is_some());
    // A catalog pointer past the end of the image.
    let mut bad = good;
    bad[17 * 2048 + 71..17 * 2048 + 75].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
    let image = open(bad);
    assert!(image.boot_entries().is_empty());
    assert!(image.notes().iter().any(|n| n.contains("boot catalog")));
}

fn mbr_entry(disk: &mut [u8], slot: usize, status: u8, kind: u8, start: u32, count: u32) {
    let e = &mut disk[446 + slot * 16..462 + slot * 16];
    e[0] = status;
    e[4] = kind;
    e[8..12].copy_from_slice(&start.to_le_bytes());
    e[12..16].copy_from_slice(&count.to_le_bytes());
    disk[510] = 0x55;
    disk[511] = 0xAA;
}

fn gpt_disk(sector: usize) -> Vec<u8> {
    let mut d = vec![0u8; 64 * sector];
    mbr_entry(&mut d, 0, 0, 0xEE, 1, 63);
    let h = sector;
    d[h..h + 8].copy_from_slice(b"EFI PART");
    d[h + 72..h + 80].copy_from_slice(&2u64.to_le_bytes());
    d[h + 80..h + 84].copy_from_slice(&128u32.to_le_bytes());
    d[h + 84..h + 88].copy_from_slice(&128u32.to_le_bytes());
    let e = 2 * sector;
    d[e..e + 16].copy_from_slice(&[
        0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9,
        0x3B,
    ]);
    d[e + 32..e + 40].copy_from_slice(&40u64.to_le_bytes());
    d[e + 40..e + 48].copy_from_slice(&49u64.to_le_bytes());
    let e = e + 128;
    d[e..e + 16].copy_from_slice(&[
        0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D,
        0xE4,
    ]);
    d[e + 32..e + 40].copy_from_slice(&50u64.to_le_bytes());
    d[e + 40..e + 48].copy_from_slice(&59u64.to_le_bytes());
    d[e + 48] = 4;
    let e = e + 128;
    d[e..e + 16].copy_from_slice(&[0x11; 16]);
    d[e + 32..e + 40].copy_from_slice(&60u64.to_le_bytes());
    d[e + 40..e + 48].copy_from_slice(&61u64.to_le_bytes());
    d
}

#[test]
fn raw_disk_images_with_mbr_and_gpt() {
    let mut disk = vec![0u8; 4 * 1024 * 1024];
    mbr_entry(&mut disk, 0, 0x80, 0x0C, 2048, 1000);
    mbr_entry(&mut disk, 2, 0x00, 0x83, 4096, 2000);
    mbr_entry(&mut disk, 3, 0x00, 0x7F, 6096, 8);
    let info = inspect_bytes(disk.clone());
    assert_eq!(info.kind, ImageKind::DiskImage);
    assert!(info.has_mbr && !info.has_gpt && !info.is_hybrid());
    assert_eq!(info.partitions.len(), 3);
    assert_eq!(info.partitions[0].kind, "FAT32 LBA");
    assert!(info.partitions[0].bootable);
    assert_eq!(
        (info.partitions[0].offset, info.partitions[0].size),
        (2048 * 512, 1000 * 512)
    );
    assert_eq!(info.partitions[1].kind, "Linux");
    assert_eq!(info.partitions[2].kind, "0x7F");
    assert!(info.notes.is_empty(), "{:?}", info.notes);
    // A partition reaching past the end of the file: truncated.
    mbr_entry(&mut disk, 1, 0x00, 0xEF, 5000, 100_000);
    let info = inspect_bytes(disk.clone());
    assert_eq!(info.partitions[1].kind, "EFI System");
    assert!(info.notes.iter().any(|n| n.contains("incomplete")));
    // An insane status byte: not a partition table.
    disk[446] = 0x33;
    let info = inspect_bytes(disk);
    assert_eq!(info.kind, ImageKind::Unknown);
    assert!(!info.has_mbr);

    for sector in [512usize, 4096] {
        let info = inspect_bytes(gpt_disk(sector));
        assert_eq!(info.kind, ImageKind::DiskImage, "sector {sector}");
        assert!(info.has_gpt && info.has_mbr);
        let kinds: Vec<&str> = info.partitions.iter().map(|p| p.kind.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "EFI System",
                "Linux filesystem",
                "11111111-1111-1111-1111-111111111111"
            ]
        );
        assert_eq!(info.partitions[0].offset, 40 * sector as u64);
        assert_eq!(info.partitions[0].size, 10 * sector as u64);
        assert!(info.partitions[1].bootable && !info.partitions[0].bootable);
        assert_eq!(
            hybrid::detect(&mut MemSource(gpt_disk(sector))).sector_size,
            sector as u32
        );
    }

    // A bare FAT volume.
    let efi = fake_efi_binary(Arch::X64);
    let info = inspect_bytes(fat12_image(&[("EFI/BOOT/BOOTX64.EFI", &efi)]));
    assert_eq!(info.kind, ImageKind::DiskImage);
    assert!(!info.has_mbr);
    assert!(info.notes.iter().any(|n| n.contains("FAT")));
    assert_eq!(hybrid::mbr_kind(0x17), "Hidden (isohybrid, 0x17)");
    assert_eq!(hybrid::mbr_kind(0xEE), "GPT protective");
}

#[test]
fn fat_reader_finds_efi_loaders() {
    let x64 = fake_efi_binary(Arch::X64);
    let big = pattern(3000, 3, 3);
    let mut image = vec![0u8; 4096];
    image.extend(fat12_image(&[
        ("STARTUP.NSH", b"echo".as_slice()),
        ("EFI/BOOT/BOOTX64.EFI", &x64),
        ("EFI/BOOT/BOOTAA64.EFI", &big),
        ("EFI/BOOT/GRUB.CFG", b"cfg".as_slice()),
        ("EFI/OTHER/BOOTIA32.EFI", &x64),
    ]));
    let found = fatmini::efi_boot_files(&mut MemSource(image.clone()), 4096).unwrap();
    assert_eq!(found, ["BOOTX64.EFI", "BOOTAA64.EFI"]);
    assert!(fatmini::efi_boot_files(&mut MemSource(image.clone()), 0).is_err());
    assert!(fatmini::efi_boot_files(&mut MemSource(image.clone()), 1 << 40).is_err());
    let none = fat12_image(&[("README.TXT", b"x".as_slice())]);
    assert!(fatmini::efi_boot_files(&mut MemSource(none), 0)
        .unwrap()
        .is_empty());
    // Damage must not panic or hang.
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    for _ in 0..500 {
        let mut bad = image.clone();
        for _ in 0..1 + rng.below(6) {
            let at = 4096 + rng.below(4096);
            bad[at] = rng.next() as u8;
        }
        let _ = fatmini::efi_boot_files(&mut MemSource(bad), 4096);
    }
}

fn wim_info(bytes: &[u8]) -> Result<wim::WimInfo> {
    wim::read_wim_info(bytes.len() as u64, &mut |offset, buf| {
        let end = offset as usize + buf.len();
        buf.copy_from_slice(&bytes[offset as usize..end]);
        Ok(())
    })
}

#[test]
fn wim_metadata() {
    let w = fake_wim(
        &[
            ("Windows 11 Home", 9, (10, 0, 26100)),
            ("Windows 11 Pro <N> & more", 12, (10, 0, 22631)),
            ("Old x86", 0, (6, 1, 7601)),
            ("ARM thing", 5, (10, 0, 1)),
            ("Mystery", 77, (1, 2, 3)),
        ],
        50_000,
    );
    assert_eq!(w.len(), 50_000);
    let info = wim_info(&w).unwrap();
    assert_eq!(info.image_count, 5);
    assert_eq!((info.part, info.total_parts), (1, 1));
    let names: Vec<&str> = info.images.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Windows 11 Home",
            "Windows 11 Pro <N> & more",
            "Old x86",
            "ARM thing",
            "Mystery"
        ]
    );
    assert_eq!(info.images[0].version, "10.0.26100");
    assert_eq!(info.images[2].version, "6.1.7601");
    assert_eq!(info.images[1].index, 2);
    assert_eq!(info.images[4].arch, None);
    assert_eq!(info.archs(), [Arch::X86, Arch::X64, Arch::Arm, Arch::Arm64]);
    // A size smaller than the header and XML need grows the file.
    assert!(fake_wim(&[("A", 9, (1, 0, 0))], 0).len() > wim::WIM_HEADER_LEN);

    let mut bad = w.clone();
    bad[0] = b'X';
    assert!(wim_info(&bad).is_err());
    assert!(wim_info(&w[..100]).is_err());
    let mut bad = w.clone();
    bad[79] = 0x04; // compressed
    assert!(wim_info(&bad)
        .unwrap_err()
        .to_string()
        .contains("compressed"));
    let mut bad = w.clone();
    bad[80..88].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(wim_info(&bad).is_err());
    let mut bad = w.clone();
    bad[80..88].copy_from_slice(&49_990u64.to_le_bytes());
    assert!(wim_info(&bad).is_err());
    let mut bad = w.clone();
    let huge = (wim::MAX_XML_BYTES + 2).to_le_bytes();
    bad[72..79].copy_from_slice(&huge[..7]);
    bad[88..96].copy_from_slice(&huge);
    assert!(wim_info(&bad).unwrap_err().to_string().contains("large"));
    let mut bad = w;
    bad[72..79].fill(0);
    bad[88..96].fill(0);
    assert!(wim_info(&bad).is_err());
}

#[test]
fn wim_xml_scanner_is_tolerant() {
    let images = wim::parse_xml_text(
        "<WIM><IMAGE INDEX=\"3\"><WINDOWS><ARCH>9</ARCH><PRODUCTNAME>P</PRODUCTNAME>\
         <VERSION><MAJOR>10</MAJOR><MINOR>0</MINOR></VERSION></WINDOWS>\
         <NAME> Plain &amp; simple </NAME><DISPLAYNAME></DISPLAYNAME></IMAGE>\
         <IMAGEX>no</IMAGEX>\
         <IMAGE><NAME>Second</NAME><DISPLAYNAME>Shown</DISPLAYNAME><WINDOWS><ARCH>x</ARCH></WINDOWS></IMAGE>\
         <IMAGE INDEX='9'><DISPLAYNAME/><NAME>Unclosed",
    );
    assert_eq!(images.len(), 3);
    assert_eq!(images[0].index, 3);
    assert_eq!(images[0].name, "Plain & simple");
    assert_eq!(images[0].arch, Some(Arch::X64));
    assert_eq!(images[0].version, "10.0");
    assert_eq!((images[1].index, images[1].name.as_str()), (2, "Shown"));
    assert_eq!((images[1].arch, images[1].version.as_str()), (None, ""));
    assert_eq!((images[2].index, images[2].name.as_str()), (9, ""));
    assert!(wim::parse_xml(&[]).is_empty());
    assert!(wim::parse_xml(&[0xFF, 0xFE, b'<', 0, b'I']).is_empty());
    assert!(wim::parse_xml_text("<IMAGE").is_empty());
    let mut rng = Rng(77);
    for _ in 0..300 {
        let junk: Vec<u8> = (0..rng.below(400)).map(|_| rng.next() as u8).collect();
        let _ = wim::parse_xml(&junk);
        let text: String = (0..rng.below(200))
            .map(|_| ['<', '>', '/', 'I', 'M', 'A', 'G', 'E', ' ', '"', '='][rng.below(11)])
            .collect();
        let _ = wim::parse_xml_text(&text);
    }
}

// --------------------------------------------------------------------------------------
// inspect
// --------------------------------------------------------------------------------------

#[test]
fn inspect_linux_sample() {
    let bytes = sample_linux_hybrid();
    assert!(bytes.len() < 2 * 1024 * 1024);
    let info = inspect_bytes(bytes.clone());
    assert_eq!(info.kind, ImageKind::Iso);
    assert_eq!(info.size, bytes.len() as u64);
    assert_eq!(info.label, "BF_LINUX_LIVE");
    assert!(info.has_iso9660 && info.has_joliet && info.has_rock_ridge);
    assert_eq!(info.udf, None);
    assert!(info.has_mbr && !info.has_gpt && info.is_hybrid());
    assert_eq!(info.partitions.len(), 2);
    assert_eq!(info.partitions[0].kind, "Hidden (isohybrid, 0x17)");
    assert!(info.partitions[0].bootable);
    assert_eq!(info.partitions[0].size, bytes.len() as u64);
    assert_eq!(info.partitions[1].kind, "EFI System");
    assert_eq!(info.boot_entries.len(), 2);
    assert!(info.bios_el_torito() && info.uefi_el_torito());
    assert_eq!(info.boot_entries[0].sectors, 4);
    assert_eq!(
        info.partitions[1].offset,
        info.boot_entries[1].lba as u64 * 2048
    );
    assert_eq!(info.efi_loaders, ["BOOTX64.EFI"]);
    assert!(info.uefi_file_bootable());
    assert_eq!(info.archs, [Arch::X64]);
    assert!(info.windows.is_none());
    assert_eq!(info.family, "Linux live/installer");
    assert_eq!(info.file_count, 11);
    assert_eq!(
        info.largest_file,
        Some(("casper/filesystem.squashfs".to_string(), 400_000))
    );
    assert!(info.total_file_bytes > 900_000);
    assert!(info.notes.is_empty(), "{:?}", info.notes);

    let image = open(bytes);
    assert_eq!(image.name_space(), NameSpace::Joliet);
    assert_eq!(content(&image, "casper/vmlinuz"), pattern(300_000, 11, 3));
    assert!(image
        .find("casper/filesystem.manifest-with-a-rather-long-name.txt")
        .is_some());
}

#[test]
fn inspect_windows_sample() {
    let bytes = sample_windows_setup(300_000);
    let info = inspect_bytes(bytes.clone());
    assert_eq!(info.kind, ImageKind::Iso);
    assert_eq!(info.label, "CCCOMA_X64FRE_EN-US_DV9");
    assert!(info.has_iso9660 && !info.has_joliet && !info.has_rock_ridge);
    assert_eq!(info.udf.as_deref(), Some("1.02"));
    assert!(!info.has_mbr && !info.has_gpt && !info.is_hybrid());
    assert!(info.bios_el_torito() && info.uefi_el_torito());
    assert_eq!(info.efi_loaders, ["BOOTX64.EFI"]);
    assert_eq!(info.archs, [Arch::X64]);
    assert_eq!(info.family, "Windows setup");
    let w = info.windows.expect("windows info");
    assert_eq!(w.install_path, "sources/install.wim");
    assert_eq!(w.install_size, 300_000);
    assert!(!w.is_esd && !w.over_4g && w.is_setup);
    assert_eq!(w.editions, ["Windows 11 Home", "Windows 11 Pro"]);
    assert_eq!(w.version, "10.0.26100");
    assert_eq!(info.file_count, 13);
    assert_eq!(
        info.largest_file,
        Some(("sources/install.wim".to_string(), 300_000))
    );
    assert!(info.notes.is_empty(), "{:?}", info.notes);

    let image = open(bytes);
    assert_eq!(image.name_space(), NameSpace::Udf);
    assert_eq!(content(&image, "BOOT/BCD"), pattern(8192, 9, 3));
    let wim_bytes = content(&image, "sources/install.wim");
    assert_eq!(wim_info(&wim_bytes).unwrap().images.len(), 2);
}

#[test]
fn inspect_windows_sample_over_4g() {
    let tail = 5 * 1024 * 1024 * 1024u64;
    let bytes = sample_windows_setup_sparse(100_000, tail);
    assert!(bytes.len() < 2 * 1024 * 1024);
    let info = inspect_bytes(bytes);
    let w = info.windows.expect("windows info");
    assert!(w.over_4g && w.is_setup);
    assert_eq!(w.install_size, 100_352 + tail - 352);
    assert_eq!(w.editions, ["Windows 11 Home", "Windows 11 Pro"]);
    assert_eq!(info.largest_file.unwrap().0, "sources/install.wim");
    assert!(info.total_file_bytes > tail);
}

#[test]
fn inspect_windows_variants() {
    // install.esd, no boot.wim: not a setup disc.
    let esd = fake_wim(&[("Windows 10 Pro", 0, (10, 0, 19045))], 20_000);
    let info = inspect_bytes(
        IsoBuilder::new("ESD")
            .joliet(true)
            .file("Sources/Install.ESD", esd)
            .file("setup.exe", b"x".to_vec())
            .build(),
    );
    let w = info.windows.unwrap();
    assert!(w.is_esd && !w.is_setup);
    assert_eq!(w.install_path, "Sources/Install.ESD");
    assert_eq!(info.archs, [Arch::X86]);
    assert_eq!(info.family, "Windows setup");

    // A damaged install.wim only costs the edition list.
    let info = inspect_bytes(
        IsoBuilder::new("BAD")
            .joliet(true)
            .file("sources/install.wim", pattern(5000, 3, 3))
            .file("sources/boot.wim", fake_wim(&[("PE", 9, (10, 0, 1))], 1000))
            .file("bootmgr", b"x".to_vec())
            .build(),
    );
    let w = info.windows.unwrap();
    assert!(w.is_setup && w.editions.is_empty() && w.version.is_empty());
    assert!(
        info.notes.iter().any(|n| n.contains("editions")),
        "{:?}",
        info.notes
    );

    // boot.wim alone: Windows PE.
    let info = inspect_bytes(
        IsoBuilder::new("PE")
            .joliet(true)
            .file(
                "sources/boot.wim",
                fake_wim(&[("PE", 12, (10, 0, 1))], 1000),
            )
            .file("bootmgr", b"x".to_vec())
            .build(),
    );
    assert!(info.windows.is_none());
    assert_eq!(info.family, "Windows PE / recovery");
    assert_eq!(info.archs, [Arch::Arm64]);

    // Nothing recognisable.
    let info = inspect_bytes(IsoBuilder::new("DATA").file("a.txt", b"x".to_vec()).build());
    assert_eq!((info.kind, info.family.as_str()), (ImageKind::Iso, ""));
    assert!(info.archs.is_empty() && info.boot_entries.is_empty());
}

#[test]
fn inspect_efi_loader_details() {
    // Loaders only inside the El Torito EFI image.
    let x64 = fake_efi_binary(Arch::X64);
    let esp = fat12_image(&[
        ("EFI/BOOT/BOOTX64.EFI", &x64),
        ("EFI/BOOT/BOOTIA32.EFI", &x64),
    ]);
    let info = inspect_bytes(
        IsoBuilder::new("EFIONLY")
            .rock_ridge(true)
            .uefi_boot(esp)
            .file("isolinux/isolinux.cfg", b"x".to_vec())
            .build(),
    );
    assert!(info.efi_loaders.is_empty() && !info.uefi_file_bootable());
    assert!(info.uefi_el_torito() && !info.bios_el_torito());
    assert_eq!(info.archs, [Arch::X86, Arch::X64]);
    assert!(
        info.notes
            .iter()
            .any(|n| n.contains("only inside the embedded EFI image") && n.contains("BOOTX64.EFI")),
        "{:?}",
        info.notes
    );
    assert_eq!(info.family, "Linux live/installer");

    // Several loaders; one whose header disagrees with its name; one that is no PE file.
    let info = inspect_bytes(
        IsoBuilder::new("MULTI")
            .joliet(true)
            .file("efi/boot/bootx64.efi", fake_efi_binary(Arch::Arm64))
            .file("efi/boot/bootia32.efi", fake_efi_binary(Arch::X86))
            .file("efi/boot/bootriscv64.efi", b"not a PE file".to_vec())
            .file("efi/boot/bootarm.efi", fake_efi_binary(Arch::Arm))
            .file("efi/boot/grub.efi", fake_efi_binary(Arch::X64))
            .file("efi/boot/sub/bootaa64.efi", fake_efi_binary(Arch::Arm64))
            .build(),
    );
    let mut loaders = info.efi_loaders.clone();
    loaders.sort();
    assert_eq!(
        loaders,
        [
            "BOOTARM.EFI",
            "BOOTIA32.EFI",
            "BOOTRISCV64.EFI",
            "BOOTX64.EFI"
        ]
    );
    assert_eq!(
        info.archs,
        [Arch::X86, Arch::Arm, Arch::Arm64, Arch::RiscV64]
    );
    assert!(info
        .notes
        .iter()
        .any(|n| n.contains("BOOTX64.EFI") && n.contains("ARM64")));
}

#[test]
fn inspect_hybrid_gpt_sample() {
    let x64 = fake_efi_binary(Arch::X64);
    let esp = fat12_image(&[("EFI/BOOT/BOOTX64.EFI", &x64)]);
    for udf in [false, true] {
        let bytes = IsoBuilder::new("GPT")
            .joliet(true)
            .udf(udf)
            .bios_boot(pattern(2048, 1, 1))
            .uefi_boot(esp.clone())
            .hybrid_gpt(true)
            .file("EFI/BOOT/BOOTX64.EFI", x64.clone())
            .file("live/vmlinuz", pattern(5000, 3, 3))
            .build();
        let info = inspect_bytes(bytes.clone());
        assert_eq!(info.kind, ImageKind::Iso);
        assert!(info.has_gpt && info.has_mbr && info.is_hybrid());
        let kinds: Vec<&str> = info.partitions.iter().map(|p| p.kind.as_str()).collect();
        assert_eq!(kinds, ["Microsoft basic data", "EFI System"]);
        assert_eq!(
            info.partitions[1].offset,
            info.boot_entries[1].lba as u64 * 2048
        );
        assert_eq!(info.partitions[1].size, esp.len() as u64);
        assert_eq!(info.udf.is_some(), udf);
        assert_eq!(info.family, "Linux live/installer");
        assert!(info.notes.is_empty(), "{:?}", info.notes);
        let tables = hybrid::detect(&mut MemSource(bytes));
        assert_eq!(tables.mbr_partitions[0].kind, "GPT protective");
    }
}

#[test]
fn inspect_unrecognised_input() {
    let info = inspect_bytes(Vec::new());
    assert_eq!(info.kind, ImageKind::Unknown);
    assert!(info.notes[0].contains("empty"));

    let info = inspect_bytes(vec![7u8; 100]);
    assert_eq!(info.kind, ImageKind::Unknown);
    assert!(info.notes[0].contains("shorter than one sector"));

    for (magic, name) in [
        (&[0x1F, 0x8B, 8, 0][..], "gzip"),
        (&[0xFD, b'7', b'z', b'X', b'Z', 0][..], "xz"),
        (&[0x28, 0xB5, 0x2F, 0xFD][..], "zstd"),
        (&b"PK\x03\x04"[..], "zip"),
        (&b"BZh9"[..], "bzip2"),
        (&[b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C][..], "7z"),
    ] {
        for len in [magic.len(), 100_000] {
            let mut bytes = pattern(len, 3, 1);
            bytes[..magic.len()].copy_from_slice(magic);
            let info = inspect_bytes(bytes);
            assert_eq!(info.kind, ImageKind::Unknown);
            assert!(
                info.notes
                    .iter()
                    .any(|n| n.contains(name) && n.contains("decompressed")),
                "{name}: {:?}",
                info.notes
            );
        }
    }

    let mut rng = Rng(42);
    for len in [2048usize, 5000, 40_000, 600_000] {
        let garbage: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        let info = inspect_bytes(garbage);
        assert_eq!(info.kind, ImageKind::Unknown);
        assert_eq!(info.file_count, 0);
        assert!(!info.notes.is_empty());
    }
    let info = inspect_bytes(vec![0u8; 100_000]);
    assert_eq!(info.kind, ImageKind::Unknown);
}

#[test]
fn inspect_reports_truncated_images() {
    let bytes = sample_linux_hybrid();
    let cut = bytes[..bytes.len() - 300_000].to_vec();
    let info = inspect_bytes(cut.clone());
    assert_eq!(info.kind, ImageKind::Iso);
    assert!(info
        .notes
        .iter()
        .any(|n| n.contains("download may be incomplete")));
    assert_eq!(info.file_count, 11);
    // Files that lie in the missing part fail on read, not on open.
    let image = open(cut);
    let e = image.find("casper/filesystem.squashfs").unwrap();
    let mut r = image.open_file(e);
    let mut sink = Vec::new();
    assert!(r.read_to_end(&mut sink).is_err());
    assert!(content(&image, "boot/grub/grub.cfg").starts_with(b"menuentry"));

    // A UDF-only image cut short still has its first anchor.
    let udf = unrle(include_bytes!("../../tests/fixtures/imapi_udf102.iso.rle"));
    let info = inspect_bytes(udf[..600_000].to_vec());
    assert_eq!(info.kind, ImageKind::Iso);
    assert!(info
        .notes
        .iter()
        .any(|n| n.contains("download may be incomplete")));
}

#[test]
fn inspect_reads_a_real_file() {
    let path = std::env::temp_dir().join(format!("bootforge-iso-{}.iso", std::process::id()));
    std::fs::write(&path, sample_windows_setup(50_000)).unwrap();
    let info = inspect(&path);
    std::fs::remove_file(&path).unwrap();
    let info = info.unwrap();
    assert_eq!(info.path, path);
    assert_eq!(info.kind, ImageKind::Iso);
    assert_eq!(info.family, "Windows setup");
    assert!(matches!(inspect(&path), Err(Error::Io(..))));
}

// --------------------------------------------------------------------------------------
// Damaged input
// --------------------------------------------------------------------------------------

/// Opens whatever can be opened and touches every entry; nothing may panic.
fn exercise(bytes: Vec<u8>) {
    if let Ok(image) = try_open(bytes.clone()) {
        let mut buf = [0u8; 300];
        for e in image.entries() {
            assert!(!e.path.is_empty() && !e.path.starts_with('/'));
            let n = e.size.min(buf.len() as u64) as usize;
            let _ = image.read_at(e, 0, &mut buf[..n]);
            if e.size > 10 {
                let _ = image.read_at(e, e.size - 10, &mut buf[..10]);
            }
        }
        let _ = image.label();
    }
    let _ = inspect_source(Path::new("fuzz"), Box::new(MemSource(bytes)));
}

fn compact_bridge() -> Vec<u8> {
    let x64 = fake_efi_binary(Arch::X64);
    let esp = fat12_image(&[("EFI/BOOT/BOOTX64.EFI", &x64)]);
    IsoBuilder::new("FUZZ")
        .joliet(true)
        .rock_ridge(true)
        .udf(true)
        .extent_size(2048)
        .udf_inline_ads(2)
        .bios_boot(pattern(2048, 1, 1))
        .uefi_boot(esp)
        .hybrid_gpt(true)
        .file("efi/boot/bootx64.efi", x64)
        .file(
            "sources/install.wim",
            fake_wim(&[("W", 9, (10, 0, 1))], 9000),
        )
        .file("sources/boot.wim", fake_wim(&[("P", 9, (10, 0, 1))], 1000))
        .file("bootmgr", pattern(100, 1, 1))
        .file(
            &format!("docs/{}.txt", "long".repeat(50)),
            pattern(5000, 3, 3),
        )
        .file("docs/a/b/c/deep.bin", pattern(3000, 5, 5))
        .sparse_file("hole.bin", pattern(100, 1, 1), 1 << 33)
        .build()
}

#[test]
fn truncated_images_never_panic() {
    let bases = [
        sample_linux_hybrid(),
        sample_windows_setup(40_000),
        compact_bridge(),
        unrle(include_bytes!("../../tests/fixtures/imapi_all.iso.rle")),
        unrle(include_bytes!("../../tests/fixtures/imapi_udf250.iso.rle")),
    ];
    for base in &bases {
        let mut lengths: Vec<usize> = (0..base.len()).step_by(base.len() / 90 + 1).collect();
        for sector in [
            16usize, 17, 18, 19, 20, 32, 33, 48, 64, 256, 257, 258, 259, 260, 270,
        ] {
            for delta in [0usize, 1, 16, 511, 2047] {
                lengths.push(sector * 2048 + delta);
            }
        }
        lengths.extend([
            1,
            2,
            511,
            512,
            513,
            2047,
            2048,
            base.len() - 1,
            base.len() - 2048,
        ]);
        for len in lengths {
            if len <= base.len() {
                exercise(base[..len].to_vec());
            }
        }
    }
}

#[test]
fn random_damage_never_panics_or_hangs() {
    let bases = [
        compact_bridge(),
        with_files(IsoBuilder::new("P").rock_ridge(true), &standard_files()).build(),
        with_files(
            IsoBuilder::new("U").iso9660(false).udf_embed(true),
            &standard_files(),
        )
        .build(),
        sample_windows_setup(20_000),
        unrle(include_bytes!("../../tests/fixtures/imapi_all.iso.rle")),
        unrle(include_bytes!("../../tests/fixtures/imapi_boot.iso.rle")),
    ];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for base in &bases {
        // Positions holding data: flipping zeros in padding teaches little.
        let live: Vec<usize> = (0..base.len()).filter(|i| base[*i] != 0).collect();
        for round in 0..350 {
            let mut bad = base.clone();
            for _ in 0..1 + rng.below(8) {
                let at = if round % 3 == 0 {
                    rng.below(bad.len())
                } else {
                    live[rng.below(live.len())]
                };
                bad[at] = match rng.below(4) {
                    0 => 0,
                    1 => 0xFF,
                    2 => bad[at] ^ (1 << rng.below(8)),
                    _ => rng.next() as u8,
                };
            }
            exercise(bad);
        }
        // Whole sectors zeroed or filled with noise.
        for _ in 0..60 {
            let mut bad = base.clone();
            let sector = live[rng.below(live.len())] / 2048;
            let noise = rng.below(2) == 0;
            for b in &mut bad[sector * 2048..(sector + 1) * 2048] {
                *b = if noise { rng.next() as u8 } else { 0 };
            }
            exercise(bad);
        }
    }
}

fn iso_root(bytes: &[u8], descriptor: usize) -> (usize, usize) {
    let at = descriptor * 2048;
    (
        le32(bytes, at + 158) as usize * 2048,
        le32(bytes, at + 166) as usize,
    )
}

/// Offset of the directory record named `name` in the directory at `dir`.
fn record_at(bytes: &[u8], dir: (usize, usize), name: &[u8]) -> usize {
    dir.0 + find_bytes(&bytes[dir.0..dir.0 + dir.1], name) - 33
}

fn plain_image() -> Vec<u8> {
    IsoBuilder::new("PLAIN")
        .file("top.txt", b"top".to_vec())
        .file("docs/a.txt", b"aaa".to_vec())
        .file("docs/sub/b.txt", b"bbb".to_vec())
        .file("zeta/z.txt", b"zzz".to_vec())
        .build()
}

#[test]
fn damaged_iso9660_structures() {
    let good = plain_image();
    let root = iso_root(&good, 16);
    assert_eq!(open(good.clone()).entries().len(), 7);

    // Zeroed or garbled primary descriptor: no longer an ISO.
    let mut bad = good.clone();
    bad[16 * 2048..17 * 2048].fill(0);
    assert!(matches!(try_open(bad.clone()), Err(Error::Invalid(_))));
    assert_eq!(inspect_bytes(bad).kind, ImageKind::Unknown);
    let mut bad = good.clone();
    bad[16 * 2048 + 128..16 * 2048 + 132].copy_from_slice(&[7, 0, 0, 7]); // block size
    assert!(try_open(bad).is_err());

    // A bad record length in the root directory: unreadable, but still an ISO.
    let top = record_at(&good, root, b"TOP.TXT;1");
    for len in [1u8, 20, 33] {
        let mut bad = good.clone();
        bad[top] = len;
        let err = try_open(bad.clone()).unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{err}");
        let info = inspect_bytes(bad);
        assert_eq!(info.kind, ImageKind::Iso);
        assert!(!info.notes.is_empty());
    }
    // A name length running past its record.
    let mut bad = good.clone();
    bad[top + 32] = 200;
    assert!(try_open(bad).is_err());

    // A broken sub-directory is skipped with a note.
    let docs = record_at(&good, root, b"DOCS");
    let docs_dir = (
        le32(&good, docs + 2) as usize * 2048,
        le32(&good, docs + 10) as usize,
    );
    let mut bad = good.clone();
    bad[record_at(&good, docs_dir, b"A.TXT;1")] = 7;
    let image = open(bad);
    assert!(image.find("DOCS").unwrap().is_dir);
    assert!(image.find("DOCS/A.TXT").is_none());
    assert_eq!(content(&image, "ZETA/Z.TXT"), b"zzz");
    assert!(
        image.notes().iter().any(|n| n.contains("DOCS")),
        "{:?}",
        image.notes()
    );

    // Directory extent past the end of the image, and an absurd size.
    for (field, value) in [
        (2usize, 0x00FF_FFFFu32),
        (10, 0xFFFF_F800),
        (10, 0x4000_0000),
    ] {
        let mut bad = good.clone();
        bad[docs + field..docs + field + 4].copy_from_slice(&value.to_le_bytes());
        let image = open(bad);
        assert!(image.find("DOCS/A.TXT").is_none());
        assert_eq!(image.notes().len(), 2, "{:?}", image.notes());
        assert_eq!(content(&image, "TOP.TXT"), b"top");
    }

    // A directory cycle: DOCS/SUB points back at the root.
    let sub = record_at(&good, docs_dir, b"SUB");
    let mut bad = good.clone();
    bad[sub + 2..sub + 6].copy_from_slice(&((root.0 / 2048) as u32).to_le_bytes());
    let image = open(bad);
    assert!(image.find("DOCS/SUB").unwrap().is_dir);
    assert!(image.find("DOCS/SUB/B.TXT").is_none());
    assert!(
        image.notes().iter().any(|n| n.contains("loops back")),
        "{:?}",
        image.notes()
    );
    assert_eq!(image.entries().len(), 6);
    // ... and a directory that is its own child.
    let mut bad = good.clone();
    bad[sub + 2..sub + 6].copy_from_slice(&((docs_dir.0 / 2048) as u32).to_le_bytes());
    assert!(open(bad).find("DOCS/SUB/A.TXT").is_none());

    // A file extent past the end: listed, but reading fails cleanly.
    let mut bad = good.clone();
    bad[top + 2..top + 6].copy_from_slice(&0x7FFF_FFF0u32.to_le_bytes());
    bad[top + 10..top + 14].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    let image = open(bad);
    let e = image.find("TOP.TXT").unwrap();
    assert_eq!(e.size, 0xFFFF_FFFF);
    assert!(matches!(
        image.read_at(e, 0, &mut [0u8; 3]),
        Err(Error::Invalid(_))
    ));
    assert!(image.open_file(e).read(&mut [0u8; 3]).is_err());
}

#[test]
fn a_damaged_joliet_tree_falls_back_to_iso9660() {
    let good = IsoBuilder::new("FB")
        .joliet(true)
        .file("Mixed Case.txt", b"data".to_vec())
        .build();
    let joliet_root = iso_root(&good, 17);
    let mut bad = good.clone();
    bad[joliet_root.0] = 5;
    let image = open(bad);
    assert_eq!(image.name_space(), NameSpace::Iso9660);
    assert!(image.has_joliet());
    assert_eq!(content(&image, "MIXED_CA.TXT"), b"data");
    assert!(
        image
            .notes()
            .iter()
            .any(|n| n.contains("Joliet") && n.contains("ISO 9660")),
        "{:?}",
        image.notes()
    );
    // Damage the ISO 9660 root as well: nothing is left.
    let mut worse = good;
    worse[joliet_root.0] = 5;
    let root = iso_root(&worse, 16);
    worse[root.0] = 5;
    assert!(try_open(worse).is_err());
}

fn udf_bridge() -> Vec<u8> {
    IsoBuilder::new("BRIDGE")
        .joliet(true)
        .udf(true)
        .file("top.txt", b"top".to_vec())
        .file("docs/a.txt", pattern(5000, 3, 3))
        .file("docs/sub/b.txt", b"bbb".to_vec())
        .build()
}

/// Sector of the UDF partition start, from the partition descriptor.
fn udf_partition(bytes: &[u8]) -> usize {
    let pd = 34 * 2048;
    assert_eq!(le16(bytes, pd), 5);
    le32(bytes, pd + 188) as usize
}

#[test]
fn damaged_udf_structures() {
    let good = udf_bridge();
    let part = udf_partition(&good);
    let clean = open(good.clone());
    assert_eq!(clean.name_space(), NameSpace::Udf);
    assert!(clean.notes().is_empty());

    // Only the CRC is off (a reserved byte of the file set descriptor changed).
    let mut bad = good.clone();
    bad[part * 2048 + 490] ^= 0x55;
    let image = open(bad);
    assert_eq!(image.name_space(), NameSpace::Udf);
    assert_eq!(image.notes().len(), 1);
    assert!(image.notes()[0].contains("CRC"));
    assert_eq!(content(&image, "docs/a.txt"), pattern(5000, 3, 3));

    // A wrong tag checksum on the root file entry: the UDF tree is refused.
    let root_fe = (part + 2) * 2048;
    assert_eq!(le16(&good, root_fe), 261);
    let mut bad = good.clone();
    bad[root_fe + 4] ^= 0xFF;
    let image = open(bad);
    assert_eq!(image.name_space(), NameSpace::Joliet);
    assert_eq!(image.udf_revision(), Some("1.02"));
    assert!(
        image
            .notes()
            .iter()
            .any(|n| n.contains("UDF") && n.contains("checksum")),
        "{:?}",
        image.notes()
    );
    assert_eq!(content(&image, "docs/a.txt"), pattern(5000, 3, 3));

    // The same damage without an ISO 9660 side: nothing can be shown.
    let udf_only = IsoBuilder::new("ONLY")
        .iso9660(false)
        .file("top.txt", b"top".to_vec())
        .file("docs/a.txt", b"aaa".to_vec())
        .file("docs/sub/b.txt", b"bbb".to_vec())
        .build();
    let only_part = udf_partition(&udf_only);
    let mut bad = udf_only.clone();
    bad[(only_part + 2) * 2048 + 4] ^= 0xFF;
    assert!(matches!(try_open(bad.clone()), Err(Error::Invalid(_))));
    let info = inspect_bytes(bad);
    assert_eq!(info.kind, ImageKind::Iso);
    assert!(!info.notes.is_empty());

    // A damaged file entry below the root is left out (lenient pass).
    let image = open(udf_only.clone());
    let top = image.find("top.txt").unwrap().clone();
    let docs_a = image.find("docs/a.txt").unwrap().clone();
    drop(image);
    let fe_of = |name: &[u8]| -> usize {
        // The file identifier holds the ICB block 14 bytes before the name.
        let at = find_bytes(&udf_only, name);
        (only_part + le32(&udf_only, at - 14) as usize) * 2048
    };
    let top_fe = fe_of(b"\x08top.txt");
    assert!(matches!(le16(&udf_only, top_fe), 261));
    let mut bad = udf_only.clone();
    bad[top_fe + 4] ^= 1;
    let image = open(bad);
    assert!(image.find("top.txt").is_none());
    assert_eq!(content(&image, "docs/a.txt"), b"aaa");
    assert!(
        image.notes().iter().any(|n| n.contains("top.txt")),
        "{:?}",
        image.notes()
    );
    assert!(top.size == 3 && docs_a.size == 3);

    // A directory cycle: docs/sub points back at the root file entry.
    let sub_name = find_bytes(&udf_only, b"\x08sub");
    let mut bad = udf_only.clone();
    bad[sub_name - 14..sub_name - 10].copy_from_slice(&2u32.to_le_bytes());
    let image = open(bad);
    assert!(image.find("docs/sub").unwrap().is_dir);
    assert!(image.find("docs/sub/b.txt").is_none());
    assert!(
        image.notes().iter().any(|n| n.contains("loops back")),
        "{:?}",
        image.notes()
    );

    // Extents outside the partition and absurd lengths in a file entry.
    let a_fe = fe_of(b"\x08a.txt");
    for (at, value) in [(176 + 4, 0x7FFF_FFFFu32), (172, 0xFFFF), (168, 0xFFFF_FFF0)] {
        let mut bad = udf_only.clone();
        bad[a_fe + at..a_fe + at + 4].copy_from_slice(&value.to_le_bytes());
        seal_tag(
            &mut bad[a_fe..a_fe + 2048],
            261,
            le32(&udf_only, a_fe + 12),
            168,
        );
        let image = open(bad);
        assert!(image.find("docs/a.txt").is_none(), "field {at}");
        assert_eq!(content(&image, "top.txt"), b"top");
    }
    // An absurd information length is listed and reads as a sparse tail.
    let mut bad = udf_only.clone();
    bad[a_fe + 56..a_fe + 64].copy_from_slice(&(1u64 << 50).to_le_bytes());
    let image = open(bad);
    let e = image.find("docs/a.txt").unwrap();
    assert_eq!(e.size, 1 << 50);
    let mut buf = [9u8; 8];
    image.read_at(e, 1 << 40, &mut buf).unwrap();
    assert_eq!(buf, [0u8; 8]);
    assert_eq!(inspect_bytes(udf_only).total_file_bytes, 9);
}

#[test]
fn udf_reserve_sequence_and_unsupported_variants() {
    let good = udf_bridge();

    // Main sequence wiped: the reserve copy is used.
    let mut bad = good.clone();
    bad[32 * 2048..40 * 2048].fill(0);
    let image = open(bad);
    assert_eq!(image.name_space(), NameSpace::Udf);
    assert!(
        image.notes().iter().any(|n| n.contains("reserve")),
        "{:?}",
        image.notes()
    );
    // Main sequence with a bad tag: same.
    let mut bad = good.clone();
    bad[35 * 2048 + 4] ^= 0x10;
    let image = open(bad);
    assert_eq!(image.name_space(), NameSpace::Udf);
    assert!(image.notes().iter().any(|n| n.contains("reserve")));
    // Both wiped: fall back to Joliet.
    let mut bad = good.clone();
    bad[32 * 2048..64 * 2048].fill(0);
    let image = open(bad);
    assert_eq!(image.name_space(), NameSpace::Joliet);
    assert_eq!(image.udf_revision(), Some("unknown"));
    assert!(image.notes().iter().any(|n| n.contains("UDF")));

    // The first anchor wiped: the one in the last sector is found.
    let mut bad = good.clone();
    bad[256 * 2048..257 * 2048].fill(0);
    assert_eq!(open(bad).name_space(), NameSpace::Udf);

    // Type 2 partition maps (metadata, virtual, sparable) and a 512-byte block size.
    for (id, word) in [
        ("*UDF Metadata Partition", "metadata"),
        ("*UDF Virtual Partition", "virtual"),
        ("*UDF Sparable Partition", "sparable"),
    ] {
        let mut bad = good.clone();
        for lvd in [35usize, 51] {
            let s = &mut bad[lvd * 2048..(lvd + 1) * 2048];
            assert_eq!(le16(s, 0), 6);
            s[264..268].copy_from_slice(&64u32.to_le_bytes());
            s[440..504].fill(0);
            s[440] = 2;
            s[441] = 64;
            s[445..445 + id.len()].copy_from_slice(id.as_bytes());
            s[240..242].copy_from_slice(&0x0250u16.to_le_bytes());
            seal_tag(s, 6, lvd as u32, 488);
        }
        let image = open(bad);
        assert_eq!(image.name_space(), NameSpace::Joliet);
        assert_eq!(image.udf_revision(), Some("2.50"));
        assert_eq!(image.label(), "BRIDGE");
        assert!(
            image
                .notes()
                .iter()
                .any(|n| n.contains("unsupported UDF variant") && n.contains(word)),
            "{:?}",
            image.notes()
        );
        assert_eq!(image.notes().len(), 1);
    }
    let mut bad = good.clone();
    for lvd in [35usize, 51] {
        let s = &mut bad[lvd * 2048..(lvd + 1) * 2048];
        s[212..216].copy_from_slice(&512u32.to_le_bytes());
        seal_tag(s, 6, lvd as u32, 430);
    }
    let image = open(bad);
    assert_eq!(image.name_space(), NameSpace::Joliet);
    assert!(image.notes()[0].contains("logical block size 512"));

    // Absurd partition map counts and lengths.
    for (at, value) in [(264usize, 0xFFFF_FFFFu32), (268, 1000), (264, 1)] {
        let mut bad = good.clone();
        for lvd in [35usize, 51] {
            let s = &mut bad[lvd * 2048..(lvd + 1) * 2048];
            s[at..at + 4].copy_from_slice(&value.to_le_bytes());
            seal_tag(s, 6, lvd as u32, 430);
        }
        assert_eq!(open(bad).name_space(), NameSpace::Joliet);
    }
}

#[test]
fn udf_tag_arithmetic() {
    // CRC-16/XMODEM check value.
    assert_eq!(udf::crc16(b"123456789"), 0x31C3);
    assert_eq!(udf::crc16(&[]), 0);
    let mut d = vec![0u8; 512];
    d[100] = 7;
    seal_tag(&mut d, 2, 256, 496);
    assert_eq!(udf::tag_checksum(&d), d[4]);
    assert_eq!(le32(&d, 12), 256);
}
