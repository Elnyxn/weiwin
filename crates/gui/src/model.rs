use serde::Deserialize;
use std::{
    fs::File,
    io::Read,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_STATUS_BYTES: u64 = 16 * 1024;
pub const STALE_MS: u64 = 15_000;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Status {
    pub backend: String,
    pub phase: String,
    pub microphone: String,
    pub peak: u32,
    pub partials: usize,
    pub finals: usize,
    pub last_error: String,
    pub pid: u32,
    pub updated_ms: u64,
}

impl Status {
    pub fn backend_label(&self) -> &'static str {
        match self.backend.as_str() {
            "wetype" => "输入法识别",
            "wechat" => "微信识别",
            "windows" => "Windows 原生",
            _ => "识别",
        }
    }

    pub fn read(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        if file.metadata().map_err(|e| e.to_string())?.len() > MAX_STATUS_BYTES {
            return Err("状态文件过大".into());
        }
        let mut bytes = Vec::with_capacity(1024);
        file.take(MAX_STATUS_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_STATUS_BYTES {
            return Err("状态文件过大".into());
        }
        serde_json::from_slice(&bytes).map_err(|_| "状态文件暂不可读".into())
    }

    pub fn stale(&self, now_ms: u64) -> bool {
        self.updated_ms == 0 || now_ms.saturating_sub(self.updated_ms) > STALE_MS
    }

    pub fn headline(&self, now_ms: u64, alive: bool, ready: bool) -> &'static str {
        if self.backend == "windows" || self.phase == "stopped" {
            return if ready {
                "等待状态"
            } else {
                "Windows 原生"
            };
        }
        if self.phase == "error" {
            return "出现错误";
        }
        if !alive || !ready {
            return "服务未就绪";
        }
        if self.stale(now_ms) {
            return "状态未更新";
        }
        match self.phase.as_str() {
            "ready" => "待机",
            "armed" => "聆听中",
            "connecting" => "正在连接",
            "recognizing" => "识别中",
            "paused" => "静音暂停",
            _ => "等待状态",
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_and_liveness_never_claim_stale_recognition_is_active() {
        assert!(now_ms() > 0);
        let mut status = Status {
            backend: "wetype".into(),
            phase: "recognizing".into(),
            updated_ms: 1000,
            ..Status::default()
        };
        assert_eq!(status.headline(2000, true, true), "识别中");
        assert_eq!(status.backend_label(), "输入法识别");
        assert_eq!(status.headline(2000, false, true), "服务未就绪");
        assert_eq!(status.headline(2000, true, false), "服务未就绪");
        assert_eq!(status.headline(16_001, true, true), "状态未更新");
        status.phase = "paused".into();
        assert_eq!(status.headline(2000, true, true), "静音暂停");
        status.backend = "windows".into();
        assert_eq!(status.backend_label(), "Windows 原生");
        assert_eq!(status.headline(100_000, false, false), "Windows 原生");
        assert_eq!(status.headline(100_000, true, true), "等待状态");
    }

    #[test]
    fn status_reader_accepts_protocol_and_rejects_oversized_or_partial_files() {
        let path =
            std::env::temp_dir().join(format!("wechat-gui-status-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"backend":"wechat","phase":"armed","microphone":"JZAO","peak":21218,"partials":2,"finals":1,"last_error":"","pid":42,"updated_ms":123}"#).unwrap();
        let status = Status::read(&path).unwrap();
        assert_eq!(status.backend_label(), "微信识别");
        assert_eq!(status.microphone, "JZAO");
        assert_eq!(status.pid, 42);
        assert_eq!(status.peak, 21_218);
        std::fs::write(&path, "{").unwrap();
        assert!(Status::read(&path).is_err());
        std::fs::write(&path, vec![b' '; MAX_STATUS_BYTES as usize + 1]).unwrap();
        assert!(Status::read(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }
}
