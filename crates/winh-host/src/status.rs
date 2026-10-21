//! Small state snapshot for the GUI; contains neither dictation text nor credentials.
use crate::install::Result;
use serde::Serialize;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Serialize)]
struct Snapshot {
    backend: &'static str,
    phase: &'static str,
    microphone: String,
    peak: u32,
    partials: usize,
    finals: usize,
    last_error: String,
    pid: u32,
    updated_ms: u64,
}
struct Store {
    path: PathBuf,
    snapshot: Snapshot,
}
#[derive(Clone)]
pub struct Status(Arc<Mutex<Store>>);
impl Status {
    pub fn new(backend: &'static str) -> Result<Self> {
        let folder = PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA不可用")?)
            .join("WeChatWinH");
        std::fs::create_dir_all(&folder)?;
        let status = Self(Arc::new(Mutex::new(Store {
            path: folder.join("status.json"),
            snapshot: Snapshot {
                backend,
                phase: "connecting",
                microphone: String::new(),
                peak: 0,
                partials: 0,
                finals: 0,
                last_error: String::new(),
                pid: std::process::id(),
                updated_ms: 0,
            },
        })));
        status.touch();
        Ok(status)
    }
    fn change(&self, change: impl FnOnce(&mut Snapshot)) {
        let Ok(mut store) = self.0.lock() else {
            return;
        };
        change(&mut store.snapshot);
        store.snapshot.updated_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        if let Ok(bytes) = serde_json::to_vec(&store.snapshot) {
            let temp = store
                .path
                .with_extension(format!("{}.tmp", std::process::id()));
            if std::fs::write(&temp, bytes).is_ok() {
                let _ = std::fs::rename(&temp, &store.path);
            }
        }
    }
    pub fn touch(&self) {
        self.change(|_| {});
    }
    pub fn phase(&self, phase: &'static str) {
        self.change(|s| {
            s.phase = phase;
            if phase == "armed" {
                s.last_error.clear();
                s.partials = 0;
                s.finals = 0;
            }
            if matches!(phase, "paused" | "ready") {
                s.peak = 0;
            }
        });
    }
    pub fn microphone(&self, name: String) {
        self.change(|s| s.microphone = name);
    }
    pub fn progress(&self, peak: u32, partials: usize, finals: usize) {
        self.change(|s| {
            s.peak = peak;
            s.partials = partials;
            s.finals = finals;
        });
    }
    pub fn error(&self, error: String) {
        self.change(|s| {
            s.phase = "error";
            s.last_error = error;
            s.peak = 0;
        });
    }
    pub fn restored(&self) {
        self.change(|s| {
            s.backend = "windows";
            s.phase = "stopped";
            s.pid = 0;
            s.peak = 0;
        });
    }
}

pub fn active_backend_label() -> &'static str {
    let Some(root) = std::env::var_os("LOCALAPPDATA") else {
        return "识别";
    };
    let path = PathBuf::from(root).join("WeChatWinH/status.json");
    let Ok(file) = std::fs::File::open(path) else {
        return "识别";
    };
    use std::io::Read;
    let mut bytes = Vec::new();
    if file.take(16_385).read_to_end(&mut bytes).is_err() || bytes.len() > 16_384 {
        return "识别";
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return "识别";
    };
    match value["backend"].as_str() {
        Some("wetype") => "输入法识别",
        Some("wechat") => "微信识别",
        _ => "识别",
    }
}

pub fn startup_error(message: &str) {
    // Only update the snapshot owned by this failed child, never another running broker.
    let Some(root) = std::env::var_os("LOCALAPPDATA") else {
        return;
    };
    let path = PathBuf::from(root).join("WeChatWinH/status.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    if bytes.len() > 16_384 {
        return;
    }
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return;
    };
    if value["pid"].as_u64() != Some(u64::from(std::process::id())) {
        return;
    }
    value["phase"] = "error".into();
    value["last_error"] = message.into();
    value["updated_ms"] = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64)
        .into();
    if let Ok(bytes) = serde_json::to_vec(&value) {
        let _ = std::fs::write(path, bytes);
    }
}
