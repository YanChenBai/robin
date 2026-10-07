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

pub(crate) fn set_auto_reconnect(
    records: &mut [ConnectionRecord],
    fingerprint: &str,
    enabled: bool,
) -> Result<()> {
    let record = records
        .iter_mut()
        .find(|record| record.fingerprint == fingerprint);
    ensure!(record.is_some(), "电脑记录不存在，请重新连接后设置");
    record.unwrap().auto_reconnect = enabled;
    Ok(())
}

pub(crate) fn update_auto_reconnect(path: &Path, fingerprint: &str, enabled: bool) -> Result<()> {
    let mut records = load(path)?;
    set_auto_reconnect(&mut records, fingerprint, enabled)?;
    save(path, &records)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records() -> Vec<ConnectionRecord> {
        serde_json::from_str(r#"[
            {"fingerprint":"desktop","name":"Computer","autoReconnect":false,"address":"192.168.1.2:4212"},
            {"fingerprint":"other","name":"Other","autoReconnect":false,"address":"192.168.1.3:4212"}
        ]"#).unwrap()
    }

    #[test]
    fn offline_preference_survives_reload_without_changing_authorization_or_other_computers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.json");
        save(&path, &records()).unwrap();
        update_auto_reconnect(&path, "desktop", true).unwrap();
        let restored = load(&path).unwrap();
        assert!(restored[0].auto_reconnect);
        assert_eq!(restored[0].fingerprint, "desktop");
        assert_eq!(restored[0].address, "192.168.1.2:4212");
        assert_eq!(restored[0].name, "Computer");
        assert!(!restored[1].auto_reconnect);
        update_auto_reconnect(&path, "desktop", false).unwrap();
        assert!(!load(&path).unwrap()[0].auto_reconnect);
    }

    #[test]
    fn invalid_records_or_failed_writes_do_not_replace_saved_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.json");
        let original = save(&path, &records()).unwrap();
        assert!(update_auto_reconnect(&path, "unknown", true).is_err());
        std::fs::create_dir(path.with_extension("tmp")).unwrap();
        assert!(update_auto_reconnect(&path, "desktop", true).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        std::fs::write(&path, "invalid json").unwrap();
        assert!(update_auto_reconnect(&path, "desktop", true).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "invalid json");
    }
}
