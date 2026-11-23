//! User settings, stored as JSON in `%LOCALAPPDATA%\BootForge\settings.json`.

use crate::model::SetupTweaks;
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
    /// The Windows Setup changes chosen last time (offered again for the next
    /// Windows image).
    pub setup: SetupTweaks,
    /// The images chosen most recently, newest first.
    pub recent_images: Vec<PathBuf>,
}

/// How many recently used images are remembered.
pub const RECENT_MAX: usize = 5;

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
            setup: SetupTweaks::default(),
            recent_images: Vec::new(),
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
    /// Puts `path` at the front of the recent images (once, compared without
    /// regard to case) and keeps the list at [`RECENT_MAX`] entries. Returns
    /// whether the list changed.
    pub fn remember_image(&mut self, path: &Path) -> bool {
        if self.recent_images.first().map(|p| p.as_path()) == Some(path) {
            return false;
        }
        let key = path.to_string_lossy().to_lowercase();
        self.recent_images
            .retain(|p| p.to_string_lossy().to_lowercase() != key);
        self.recent_images.insert(0, path.to_path_buf());
        self.recent_images.truncate(RECENT_MAX);
        true
    }

    /// Drops `path` from the recent images (it no longer exists).
    pub fn forget_image(&mut self, path: &Path) {
        self.recent_images.retain(|p| p != path);
    }

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
    fn recent_images_are_unique_newest_first_and_capped() {
        let mut s = Settings::default();
        for i in 0..7 {
            assert!(s.remember_image(Path::new(&format!("D:\\img\\{i}.iso"))));
        }
        assert_eq!(s.recent_images.len(), RECENT_MAX);
        assert_eq!(s.recent_images[0], Path::new("D:\\img\\6.iso"));
        // Choosing the newest again changes nothing; an older one moves up
        // without being listed twice, whatever its spelling.
        assert!(!s.remember_image(Path::new("D:\\img\\6.iso")));
        assert!(s.remember_image(Path::new("d:\\IMG\\4.iso")));
        assert_eq!(s.recent_images.len(), RECENT_MAX);
        assert_eq!(s.recent_images[0], Path::new("d:\\IMG\\4.iso"));
        assert_eq!(s.recent_images[1], Path::new("D:\\img\\6.iso"));
        s.forget_image(Path::new("D:\\img\\6.iso"));
        assert_eq!(s.recent_images.len(), RECENT_MAX - 1);
        // Remembered choices survive a round trip.
        s.setup.local_account = true;
        let dir = std::env::temp_dir().join(format!("bootforge-recent-{}", std::process::id()));
        s.save(&dir).unwrap();
        assert_eq!(Settings::load(&dir), s);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn safe_defaults() {
        let s = Settings::default();
        assert!(!s.policy.show_usb_hdd);
        assert!(s.verify);
        // Windows Setup is left alone unless the user asks.
        assert!(!s.setup.any());
    }
}
