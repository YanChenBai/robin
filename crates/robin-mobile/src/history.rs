use anyhow::{Result, ensure};
use robin_core::receiver::ConnectionRecord;
use std::path::Path;

pub(crate) fn load(path: &Path) -> Result<Vec<ConnectionRecord>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn save(path: &Path, records: &[ConnectionRecord]) -> Result<String> {
    let contents = serde_json::to_string(records)?;
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, &contents)?;
    std::fs::rename(temporary, path)?;
    Ok(contents)
}

pub(crate) fn load_auto_connect(path: &Path) -> Result<bool> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let preferences: serde_json::Value = serde_json::from_slice(&bytes)?;
            ensure!(preferences.is_object(), "接收设置无效");
            match preferences.get("autoConnect") {
                Some(value) => value.as_bool().ok_or_else(|| anyhow::anyhow!("自动连接设置无效")),
                None => Ok(true),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn save_auto_connect(path: &Path, enabled: bool) -> Result<()> {
    let mut preferences: serde_json::Value = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => return Err(error.into()),
    };
    ensure!(preferences.is_object(), "接收设置无效");
    preferences["autoConnect"] = enabled.into();
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, serde_json::to_vec(&preferences)?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_preference_survives_reload_and_preserves_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        assert!(load_auto_connect(&path).unwrap());
        std::fs::write(&path, r#"{"other":20}"#).unwrap();
        save_auto_connect(&path, false).unwrap();
        assert!(!load_auto_connect(&path).unwrap());
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["other"], 20);
        save_auto_connect(&path, true).unwrap();
        assert!(load_auto_connect(&path).unwrap());
    }

    #[test]
    fn legacy_device_preferences_are_removed_when_history_is_saved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.json");
        std::fs::write(&path, r#"[{"fingerprint":"desktop","name":"Computer","autoReconnect":false,"address":"192.168.1.2:4212"}]"#).unwrap();
        let records = load(&path).unwrap();
        assert_eq!(records[0].fingerprint, "desktop");
        assert_eq!(records[0].address, "192.168.1.2:4212");
        let saved = save(&path, &records).unwrap();
        assert!(!saved.contains("autoReconnect"));
    }

    #[test]
    fn invalid_settings_or_failed_writes_do_not_replace_saved_preferences() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        save_auto_connect(&path, true).unwrap();
        let original = std::fs::read(&path).unwrap();
        std::fs::create_dir(path.with_extension("tmp")).unwrap();
        assert!(save_auto_connect(&path, false).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::write(&path, "invalid json").unwrap();
        assert!(save_auto_connect(&path, false).is_err());
        assert!(load_auto_connect(&path).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "invalid json");
    }
}
