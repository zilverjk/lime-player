use std::fs;
use std::path::PathBuf;

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Preferences {
    pub output_device_id: Option<String>,
    pub hog_mode_enabled: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            output_device_id: None,
            hog_mode_enabled: true,
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
