//! User settings are snapshotted when the next Win+H capture starts.
use crate::Result;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_FILE_BYTES: u64 = 8 * 1024;
pub const SILENCE_SECONDS_OPTIONS: &[u32] = &[1, 2, 3, 5, 10];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub microphone: Option<String>,
    pub silence_seconds: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            microphone: None,
            silence_seconds: 3,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        if !SILENCE_SECONDS_OPTIONS.contains(&self.silence_seconds) {
            return Err("静音时间仅支持 1、2、3、5、10 秒".into());
        }
        if self
            .microphone
            .as_ref()
            .is_some_and(|id| id.len() > 1024 || id.chars().any(char::is_control))
        {
            return Err("麦克风 ID 无效".into());
        }
        Ok(())
    }

    pub fn load_default() -> Result<Self> {
        Self::load(&Self::path()?)
    }

    pub fn save_default(&self) -> Result<()> {
        self.save(&Self::path()?)
    }

    fn path() -> Result<PathBuf> {
        let local = std::env::var_os("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .ok_or("LOCALAPPDATA 不可用")?;
        Ok(PathBuf::from(local).join("WeChatWinH/settings.json"))
    }

    fn load(path: &Path) -> Result<Self> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        };
        if file.metadata()?.len() > MAX_FILE_BYTES {
            return Err("设置文件超过 8 KiB".into());
        }
        let mut bytes = Vec::with_capacity(512);
        file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err("设置文件超过 8 KiB".into());
        }
        let settings: Self = serde_json::from_slice(&bytes)?;
        settings.validate()?;
        Ok(settings)
    }

    fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err("设置文件超过 8 KiB".into());
        }
        let filename = path.file_name().ok_or("设置文件路径无效")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut nonce = [0; 8];
        getrandom::fill(&mut nonce).map_err(|_| "无法创建设置临时文件")?;
        let temporary = path.with_file_name(format!(
            ".{}.{}.{:016x}.tmp",
            filename.to_string_lossy(),
            std::process::id(),
            u64::from_le_bytes(nonce)
        ));
        // create_new ensures cleanup only touches the temporary file we own.
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let result = (|| -> Result<()> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            // The sibling file shares a filesystem; rename replaces an existing
            // regular file on both Windows and Unix without exposing partial JSON.
            fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        path: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let mut nonce = [0; 8];
            getrandom::fill(&mut nonce).unwrap();
            let root = std::env::temp_dir().join(format!(
                "wechat-settings-{}-{:016x}",
                std::process::id(),
                u64::from_le_bytes(nonce)
            ));
            fs::create_dir(&root).unwrap();
            Self {
                path: root.join("settings.json"),
                root,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
            let _ = fs::remove_dir(&self.root);
        }
    }

    #[test]
    fn missing_and_partial_settings_use_declared_defaults() {
        let fixture = Fixture::new();
        assert_eq!(Settings::load(&fixture.path).unwrap(), Settings::default());
        fs::write(&fixture.path, br#"{"microphone":"device-a"}"#).unwrap();
        assert_eq!(
            Settings::load(&fixture.path).unwrap(),
            Settings {
                microphone: Some("device-a".into()),
                silence_seconds: 3
            }
        );
    }

    #[test]
    fn saves_replace_existing_json_and_invalid_changes_preserve_it() {
        let fixture = Fixture::new();
        let first = Settings {
            microphone: Some("device-a".into()),
            silence_seconds: 1,
        };
        first.save(&fixture.path).unwrap();
        assert_eq!(Settings::load(&fixture.path).unwrap(), first);
        let second = Settings {
            microphone: None,
            silence_seconds: 10,
        };
        second.save(&fixture.path).unwrap();
        assert_eq!(Settings::load(&fixture.path).unwrap(), second);
        let previous = fs::read(&fixture.path).unwrap();
        let invalid = Settings {
            silence_seconds: 4,
            ..second
        };
        assert!(invalid.save(&fixture.path).is_err());
        assert_eq!(fs::read(&fixture.path).unwrap(), previous);
        assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 1);
    }

    #[test]
    fn malformed_oversized_and_invalid_settings_are_not_silently_defaulted() {
        let fixture = Fixture::new();
        for bytes in [
            b"{".as_slice(),
            br#"{"silence_seconds":4}"#,
            br#"{"microphone":"bad\nID"}"#,
            br#"{"silence_seconds":"3"}"#,
        ] {
            fs::write(&fixture.path, bytes).unwrap();
            assert!(Settings::load(&fixture.path).is_err());
        }
        fs::write(&fixture.path, vec![b' '; MAX_FILE_BYTES as usize + 1]).unwrap();
        assert!(Settings::load(&fixture.path).is_err());
        for seconds in SILENCE_SECONDS_OPTIONS {
            assert!(
                Settings {
                    microphone: None,
                    silence_seconds: *seconds
                }
                .validate()
                .is_ok()
            );
        }
        assert!(
            Settings {
                microphone: Some("x".repeat(1024)),
                silence_seconds: 3
            }
            .validate()
            .is_ok()
        );
        assert!(
            Settings {
                microphone: Some("x".repeat(1025)),
                silence_seconds: 3
            }
            .validate()
            .is_err()
        );
    }
}
