//! Read the desktop client's existing iLink account without changing it.
use std::{
    fs,
    path::{Path, PathBuf},
};

pub struct CloudAccount {
    pub session: String,
    pub device_id: String,
    pub expires_at: u64,
}

impl CloudAccount {
    pub fn read(path: &Path) -> crate::Result<Self> {
        if fs::metadata(path)?.len() > 32_768 {
            return Err("account file exceeds limit".into());
        }
        Self::parse(&fs::read_to_string(path)?)
    }

    pub fn default_path() -> crate::Result<PathBuf> {
        let appdata = std::env::var_os("APPDATA").ok_or("APPDATA is unavailable")?;
        Ok(PathBuf::from(appdata).join("Tencent/xwechat/ilink/wechat/cloud_account.txt"))
    }

    fn parse(input: &str) -> crate::Result<Self> {
        let mut session = None;
        let mut device_id = None;
        let mut expires_at = None;
        for line in input.trim_start_matches('\u{feff}').lines() {
            let Some(delimiter) = line.find(['=', ':']) else {
                continue;
            };
            let (key, tail) = line.split_at(delimiter);
            let value = &tail[1..];
            let value = value.trim().trim_matches('"');
            match key.trim() {
                "kTdiKeyCloudSession" => session = Some(value.to_owned()),
                "kTdiKeyDeviceId" => device_id = Some(value.to_owned()),
                "kTdiKeyCloudSessionExpireTime" => expires_at = Some(value.parse()?),
                _ => (),
            }
        }
        let session = session
            .filter(|s| !s.is_empty())
            .ok_or("cloud session is missing")?;
        let device_id = device_id
            .filter(|s| !s.is_empty())
            .ok_or("device id is missing")?;
        Ok(Self {
            session,
            device_id,
            expires_at: expires_at.ok_or("session expiry is missing")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_windows_profile_and_preserves_token_delimiters() {
        let value = CloudAccount::parse("\u{feff}kTdiKeyCloudSession: a:b==\r\nkTdiKeyDeviceId: device\r\nkTdiKeyCloudSessionExpireTime: 12345\r\n").unwrap();
        assert_eq!(value.session, "a:b==");
        assert_eq!(value.device_id, "device");
        assert_eq!(value.expires_at, 12345);
    }

    #[test]
    fn rejects_missing_account_material() {
        assert!(
            CloudAccount::parse(
                "kTdiKeyCloudSession=\nkTdiKeyDeviceId=x\nkTdiKeyCloudSessionExpireTime=123"
            )
            .is_err()
        );
    }
}
