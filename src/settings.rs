//! User settings, stored as JSON in `%LOCALAPPDATA%\BootForge\settings.json`.

use crate::safety::Policy;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeChoice {
    Dark,
    Light,
    System,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeChoice,
    /// Which disks may be offered at all (see `safety`).
    pub policy: Policy,
    /// Default for "verify after writing".
    pub verify: bool,
    /// Default for "eject when done".
    pub eject: bool,
    /// Default for "quick format".
    pub quick_format: bool,
    /// Write a log file for every operation into the `logs` folder.
    pub save_logs: bool,
    /// Folder the image picker opens in.
    pub last_image_dir: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: ThemeChoice::Dark,
            policy: Policy::default(),
            verify: true,
            eject: false,
            quick_format: true,
            save_logs: true,
            last_image_dir: None,
        }
    }
}

/// `%LOCALAPPDATA%\BootForge` (falls back to the temp dir).
pub fn app_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("BootForge")
}

impl Settings {
    pub fn load(dir: &Path) -> Settings {
        std::fs::read_to_string(dir.join("settings.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(dir.join("settings.json"), json)
            .map_err(|e| format!("Could not save settings: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_tolerant_loading() {
        let dir = std::env::temp_dir().join(format!("bootforge-settings-{}", std::process::id()));
        assert_eq!(Settings::load(&dir), Settings::default());
        let mut s = Settings {
            theme: ThemeChoice::Light,
            eject: true,
            ..Default::default()
        };
        s.policy.show_usb_hdd = true;
        s.policy.max_size = 512 * crate::safety::GIB;
        s.save(&dir).unwrap();
        assert_eq!(Settings::load(&dir), s);
        // Partial files keep defaults for the rest; broken files fall back entirely.
        std::fs::write(
            dir.join("settings.json"),
            r#"{"theme":"System","future_option":true}"#,
        )
        .unwrap();
        let loaded = Settings::load(&dir);
        assert_eq!(loaded.theme, ThemeChoice::System);
        assert!(loaded.verify);
        assert!(!loaded.policy.show_usb_hdd);
        std::fs::write(dir.join("settings.json"), "{{{").unwrap();
        assert_eq!(Settings::load(&dir), Settings::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn safe_defaults() {
        let s = Settings::default();
        assert!(!s.policy.show_usb_hdd);
        assert!(s.verify);
    }
}
