//! Build script: renders the BootForge icon (vector, via tiny-skia) into a
//! multi-size .ico and a PNG for the window, then embeds the icon, manifest and version info.

#![allow(clippy::field_reassign_with_default)]

use std::{env, fs, path::PathBuf};
use tiny_skia::*;

fn rounded_rect(x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> Path {
    let mut pb = PathBuilder::new();
    pb.move_to(x0 + r, y0);
    pb.line_to(x1 - r, y0);
    pb.quad_to(x1, y0, x1, y0 + r);
    pb.line_to(x1, y1 - r);
    pb.quad_to(x1, y1, x1 - r, y1);
    pb.line_to(x0 + r, y1);
    pb.quad_to(x0, y1, x0, y1 - r);
    pb.line_to(x0, y0 + r);
    pb.quad_to(x0, y0, x0 + r, y0);
    pb.close();
    pb.finish().expect("rounded rect")
}

fn render_icon(size: u32) -> Vec<u8> {
    let mut pm = Pixmap::new(size, size).expect("pixmap");
    let s = size as f32 / 256.0;
    let id = Transform::identity();

    // Background: rounded square, steel -> deep graphite gradient.
    let (x0, y0, x1, y1) = (8.0 * s, 8.0 * s, 248.0 * s, 248.0 * s);
    let bg = rounded_rect(x0, y0, x1, y1, 56.0 * s);
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.shader = LinearGradient::new(
        Point::from_xy(x0, y0),
        Point::from_xy(x1, y1),
        vec![
            GradientStop::new(0.0, Color::from_rgba8(0x5b, 0x6e, 0x86, 255)),
            GradientStop::new(1.0, Color::from_rgba8(0x16, 0x1d, 0x2b, 255)),
        ],
        SpreadMode::Pad,
        id,
    )
    .expect("gradient");
    pm.fill_path(&bg, &paint, FillRule::Winding, id, None);

    let fill = |pm: &mut Pixmap, path: &Path, r: u8, g: u8, b: u8, a: u8| {
        let mut p = Paint::default();
        p.anti_alias = true;
        p.set_color_rgba8(r, g, b, a);
        pm.fill_path(path, &p, FillRule::Winding, id, None);
    };
    let rr =
        |a: f32, b: f32, c: f32, d: f32, r: f32| rounded_rect(a * s, b * s, c * s, d * s, r * s);

    // USB stick, upright: plug on top, body below.
    let plug = rr(98.0, 38.0, 158.0, 96.0, 8.0);
    fill(&mut pm, &plug, 0xd3, 0xdc, 0xe6, 255);
    if size >= 32 {
        // The two contact windows of the plug.
        let a = rr(110.0, 54.0, 124.0, 70.0, 2.0);
        let b = rr(132.0, 54.0, 146.0, 70.0, 2.0);
        fill(&mut pm, &a, 0x2a, 0x35, 0x47, 255);
        fill(&mut pm, &b, 0x2a, 0x35, 0x47, 255);
    }
    let body = rr(74.0, 86.0, 182.0, 222.0, 22.0);
    fill(&mut pm, &body, 0xff, 0xff, 0xff, 255);

    // Lime bolt on the body.
    let pts: [(f32, f32); 6] = [
        (140.0, 104.0),
        (100.0, 160.0),
        (124.0, 160.0),
        (114.0, 206.0),
        (156.0, 146.0),
        (131.0, 146.0),
    ];
    let mut p = PathBuilder::new();
    for (i, (x, y)) in pts.iter().enumerate() {
        if i == 0 {
            p.move_to(x * s, y * s)
        } else {
            p.line_to(x * s, y * s)
        }
    }
    p.close();
    let bolt = p.finish().expect("bolt");
    fill(&mut pm, &bolt, 0x6f, 0xb5, 0x12, 255);

    pm.encode_png().expect("png")
}

fn write_ico(path: &PathBuf, sizes: &[u32]) {
    let pngs: Vec<Vec<u8>> = sizes.iter().map(|&s| render_icon(s)).collect();
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (i, &s) in sizes.iter().enumerate() {
        let b = if s >= 256 { 0 } else { s as u8 };
        out.extend_from_slice(&[b, b, 0, 0, 1, 0, 32, 0]);
        out.extend_from_slice(&(pngs[i].len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += pngs[i].len() as u32;
    }
    for p in &pngs {
        out.extend_from_slice(p);
    }
    fs::write(path, out).expect("write ico");
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let ico = out.join("bootforge.ico");
    write_ico(&ico, &[16, 20, 24, 32, 40, 48, 64, 256]);
    let png = render_icon(256);
    fs::write(out.join("icon-256.png"), &png).expect("write png");
    let docs_png = PathBuf::from("docs/assets/bootforge-icon.png");
    if fs::read(&docs_png).ok().as_deref() != Some(&png[..]) {
        let _ = fs::create_dir_all("docs/assets");
        let _ = fs::write(&docs_png, &png);
    }

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let ver = env::var("CARGO_PKG_VERSION").expect("version");
    let parts: Vec<u16> = ver
        .split(['.', '-'])
        .take(3)
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    let (ma, mi, pa) = (
        parts[0],
        parts.get(1).copied().unwrap_or(0),
        parts.get(2).copied().unwrap_or(0),
    );
    let ver4 = format!("{ma}.{mi}.{pa}.0");
    println!("cargo:rerun-if-changed=res/bootforge.manifest.in");
    let manifest = fs::read_to_string("res/bootforge.manifest.in")
        .expect("manifest")
        .replace("@VERSION4@", &ver4);
    let manifest_path = out.join("bootforge.manifest");
    fs::write(&manifest_path, manifest).expect("write manifest");
    let esc = |p: &PathBuf| p.display().to_string().replace('\\', "\\\\");
    let rc = format!(
        r#"#pragma code_page(65001)
1 ICON "{ico}"
1 24 "{manifest}"
1 VERSIONINFO
FILEVERSION {ma},{mi},{pa},0
PRODUCTVERSION {ma},{mi},{pa},0
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "Zheer Khalid (zheer.dev)"
      VALUE "FileDescription", "BootForge bootable USB creator"
      VALUE "FileVersion", "{ver}"
      VALUE "InternalName", "BootForge"
      VALUE "LegalCopyright", "Copyright (c) 2026 Zheer Khalid. MIT License."
      VALUE "OriginalFilename", "BootForge.exe"
      VALUE "ProductName", "BootForge"
      VALUE "ProductVersion", "{ver}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        ico = esc(&ico),
        manifest = esc(&manifest_path),
    );
    let rc_path = out.join("bootforge.rc");
    fs::write(&rc_path, rc).expect("write rc");
    embed_resource::compile(&rc_path, embed_resource::NONE);
    println!("cargo:rustc-link-arg-bins=/MANIFEST:NO");
}
