use std::fs;
use std::path::PathBuf;

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Preferences {
    pub output_device_id: Option<String>,
    pub hog_mode_enabled: bool,
    /// Convert FLAC files at a non-standard sample rate (e.g. 37 800 Hz) to one output devices
    /// accept when they are scanned, keeping a backup of the original (`audio::rate_repair`).
    /// Rewrites files on disk, so it can be switched off here (`settings.json`); there is no UI for it.
    pub repair_nonstandard_sample_rates: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            output_device_id: None,
            hog_mode_enabled: true,
            repair_nonstandard_sample_rates: true,
        }
    }
}

impl Preferences {
    pub fn load() -> Self {
        let Some(path) = settings_path() else {
            return Self::default();
        };
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                eprintln!("Could not read Lime Player settings: {error}");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = settings_path().ok_or_else(|| "Could not determine the settings directory".to_owned())?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
        fs::write(path, bytes).map_err(|error| error.to_string())
    }
}

fn settings_path() -> Option<PathBuf> {
    ProjectDirs::from("com", "Lime Player", "Lime Player")
        .map(|project| project.config_dir().join("settings.json"))
}

/// Where the originals of files rewritten by the sample-rate repair are kept: the app's data
/// directory, so the user's library folders only ever contain the repaired file.
pub fn rate_repair_backup_dir() -> Option<PathBuf> {
    ProjectDirs::from("com", "Lime Player", "Lime Player").map(|project| project.data_dir().join("rate-repair-backups"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_rate_repair_is_on_by_default() {
        assert!(Preferences::default().repair_nonstandard_sample_rates);
    }

    #[test]
    fn an_older_settings_file_without_the_repair_key_keeps_the_default() {
        let preferences: Preferences = serde_json::from_str(r#"{"output_device_id":"dac","hog_mode_enabled":false}"#).unwrap();
        assert!(preferences.repair_nonstandard_sample_rates);
        assert!(!preferences.hog_mode_enabled);
    }

    #[test]
    fn the_repair_can_be_switched_off() {
        let preferences: Preferences = serde_json::from_str(r#"{"repair_nonstandard_sample_rates":false}"#).unwrap();
        assert!(!preferences.repair_nonstandard_sample_rates);
    }
}
