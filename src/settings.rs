use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub show_in_dock: bool,
    #[serde(default)]
    pub launch_at_login: bool,
    /// If set, the app re-asserts this input device as the system default
    /// whenever something else (e.g. macOS auto-switching to AirPods on
    /// connect) changes it. Stored by name because AudioDeviceID changes
    /// between sessions.
    #[serde(default)]
    pub preferred_input_device: Option<String>,
}

impl Settings {
    pub fn load() -> Self {
        Self::load_from_file().unwrap_or_default()
    }

    fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("mic-mute").join("settings.json"))
    }

    fn load_from_file() -> Option<Self> {
        let path = Self::config_path()?;
        let data = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&data).ok()
    }

    /// Returns the last-modified time of the settings file, or None if it doesn't exist.
    pub fn mtime() -> Option<std::time::SystemTime> {
        Self::config_path()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok())
    }

    pub fn save(&self) -> Result<()> {
        if let Some(path) = Self::config_path() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let data = serde_json::to_string_pretty(self)?;
            std::fs::write(path, data)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_settings_json_round_trip() {
        let s = Settings::default();
        let json = serde_json::to_string(&s).unwrap();
        let loaded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.show_in_dock, false);
        assert_eq!(loaded.launch_at_login, false);
    }

    #[test]
    fn test_settings_json_unknown_fields_ignored() {
        // Old configs from previous versions had a "mic_shortcut" key; make
        // sure they still parse cleanly after the field was removed.
        let loaded: Settings = serde_json::from_str(
            r#"{
                "mic_shortcut": { "key": "F13", "modifiers": ["shift"] },
                "show_in_dock": true,
                "launch_at_login": true
            }"#,
        )
        .unwrap();
        assert!(loaded.show_in_dock);
        assert!(loaded.launch_at_login);
    }

    #[test]
    fn test_settings_save_and_load() {
        use std::fs;

        let tmp_dir = std::env::temp_dir().join("mic-mute-test-settings");
        let tmp_path = tmp_dir.join("settings.json");
        let _ = fs::remove_file(&tmp_path);
        let _ = fs::create_dir_all(&tmp_dir);

        let s = Settings {
            show_in_dock: true,
            launch_at_login: false,
            preferred_input_device: Some("MacBook Pro Microphone".to_string()),
        };

        let json = serde_json::to_string_pretty(&s).unwrap();
        fs::write(&tmp_path, &json).unwrap();

        let loaded: Settings =
            serde_json::from_str(&fs::read_to_string(&tmp_path).unwrap()).unwrap();
        assert_eq!(loaded.show_in_dock, true);
        assert_eq!(
            loaded.preferred_input_device.as_deref(),
            Some("MacBook Pro Microphone")
        );

        let _ = fs::remove_file(&tmp_path);
    }

    #[test]
    fn test_settings_omits_preferred_when_missing() {
        let loaded: Settings = serde_json::from_str(r#"{"show_in_dock": true}"#).unwrap();
        assert!(loaded.show_in_dock);
        assert!(loaded.preferred_input_device.is_none());
    }
}
