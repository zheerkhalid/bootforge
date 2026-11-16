#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use bootforge::ui::{self, Options};
use eframe::egui;
use std::path::PathBuf;

const USAGE: &str = "BootForge

  BootForge.exe                      open the program
  BootForge.exe --image <file>       open with this image selected
        [--disk <n>]                 and this disk number preselected (it still has to
                                     pass every safety rule and be confirmed in the window)
  BootForge.exe --demo               show invented drives and images; no drive is touched
  BootForge.exe --screenshot <png>   render the window to a PNG and exit
        [--view create|create-linux|empty|confirm|progress|done|checksums|tools|log|
                settings|about] [--light]
        (the view can also be set with the BOOTFORGE_VIEW environment variable)
  BootForge.exe --selftest           build synthetic ISOs, write them to image files in the
                                     temp folder in both modes, verify by parsing them back
        [--keep <folder>]            leave the image files in this folder afterwards
  BootForge.exe --inspect <file>     print what BootForge finds out about an image
  BootForge.exe --hash <file>        print MD5, SHA-1 and SHA-256 of a file";

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    let console = has("--help")
        || has("-h")
        || has("/?")
        || has("--selftest")
        || has("--inspect")
        || has("--hash");
    if console {
        bootforge::backend::attach_console();
    }
    if has("--help") || has("-h") || has("/?") {
        println!("{USAGE}");
        return Ok(());
    }
    if has("--selftest") {
        let keep = value("--keep").map(PathBuf::from);
        match bootforge::selftest::run_keeping(keep.as_deref()) {
            Ok(log) => {
                for line in log {
                    println!("{line}");
                }
                println!("SELFTEST PASSED");
                std::process::exit(0);
            }
            Err(e) => {
                println!("{e}");
                println!("SELFTEST FAILED");
                std::process::exit(1);
            }
        }
    }
    if let Some(file) = value("--inspect") {
        match bootforge::selftest::inspect_report(&PathBuf::from(file)) {
            Ok(lines) => {
                for line in lines {
                    println!("{line}");
                }
                std::process::exit(0);
            }
            Err(e) => {
                println!("{e}");
                std::process::exit(1);
            }
        }
    }
    if let Some(file) = value("--hash") {
        let cancel = bootforge::model::Cancel::new();
        match bootforge::backend::hash_file(&PathBuf::from(file), &cancel, &mut |_| {}) {
            Ok(sums) => {
                println!("MD5     {}", sums.md5);
                println!("SHA-1   {}", sums.sha1);
                println!("SHA-256 {}", sums.sha256);
                std::process::exit(0);
            }
            Err(e) => {
                println!("{e}");
                std::process::exit(1);
            }
        }
    }

    let options = Options {
        demo: has("--demo"),
        screenshot: value("--screenshot").map(PathBuf::from),
        view: value("--view").or_else(|| std::env::var("BOOTFORGE_VIEW").ok()),
        light: has("--light"),
        image: value("--image").map(PathBuf::from),
        disk: value("--disk").and_then(|n| n.parse().ok()),
    };
    ui::run(options, window_icon())
}

fn window_icon() -> egui::IconData {
    let bytes = include_bytes!(concat!(env!("OUT_DIR"), "/icon-256.png"));
    match image::load_from_memory(bytes) {
        Ok(img) => {
            let rgba = img.into_rgba8();
            let (width, height) = rgba.dimensions();
            egui::IconData {
                rgba: rgba.into_raw(),
                width,
                height,
            }
        }
        Err(_) => egui::IconData::default(),
    }
}
