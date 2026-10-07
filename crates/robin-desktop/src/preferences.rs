use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use winreg::{RegKey, enums::HKEY_CURRENT_USER};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct Preferences {
    pub capture_id: String,
    pub close_to_tray: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            capture_id: String::new(),
            close_to_tray: true,
        }
    }
}

impl Preferences {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, serde_json::to_vec(self)?)?;
        std::fs::rename(temporary, path)?;
        Ok(())
    }
}

pub fn startup_enabled() -> Result<bool> {
    let key = match RegKey::predef(HKEY_CURRENT_USER).open_subkey(RUN_KEY) {
        Ok(key) => key,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let actual: String = match key.get_value("Robin") {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    Ok(actual == startup_command(std::env::current_exe()?))
}

pub fn set_startup(enabled: bool) -> Result<()> {
    let key = RegKey::predef(HKEY_CURRENT_USER).create_subkey(RUN_KEY)?.0;
    if enabled {
        key.set_value("Robin", &startup_command(std::env::current_exe()?))?;
    } else if let Err(error) = key.delete_value("Robin")
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error.into());
    }
    Ok(())
}

fn startup_command(executable: PathBuf) -> String {
    format!("\"{}\" --background", executable.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_survive_restart_and_missing_fields_use_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        let mut preferences = Preferences::load(&path).unwrap();
        assert!(preferences.capture_id.is_empty());
        assert!(preferences.close_to_tray);
        preferences.capture_id = "headphones".into();
        preferences.close_to_tray = false;
        preferences.save(&path).unwrap();
        let restored = Preferences::load(&path).unwrap();
        assert_eq!(restored.capture_id, "headphones");
        assert!(!restored.close_to_tray);
        std::fs::write(&path, r#"{"capture_id":"speakers"}"#).unwrap();
        assert!(Preferences::load(&path).unwrap().close_to_tray);
    }

    #[test]
    fn startup_quotes_paths_with_spaces_and_starts_in_background() {
        assert_eq!(
            startup_command(PathBuf::from(r"C:\Program Files\Robin\robin.exe")),
            r#""C:\Program Files\Robin\robin.exe" --background"#
        );
    }
}
