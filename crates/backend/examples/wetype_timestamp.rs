//! Development acceptance: independent anonymous timestamp endpoint only.
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use wechat_voice_backend::wetype::{Transport, request};
use wechat_voice_protocol::wire;

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut socket = Transport::connect()?;
    let headers = BTreeMap::from([
        ("Content-Type".into(), "application/octet-stream".into()),
        ("HOST".into(), "wetype.weixin.qq.com".into()),
        ("Kb-CmdId".into(), "0".into()),
        ("Kb-SubCmdId".into(), "0".into()),
        ("Kb-OsType".into(), "6".into()),
        ("Kb-Version".into(), "3.0.0.37".into()),
    ]);
    socket.send(request(1, "/timestamp", &headers, b"")?)?;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(8) {
        if let Some(response) = socket.poll()? {
            let timestamp = wire::integer(&response.body, 1)?;
            println!(
                "status={} body_bytes={} timestamp_present={} elapsed_ms={}",
                response.status,
                response.body.len(),
                timestamp.is_some(),
                start.elapsed().as_millis()
            );
            socket.close();
            return if response.status == 200 && timestamp.is_some() {
                Ok(())
            } else {
                Err("timestamp rejected".into())
            };
        }
    }
    socket.close();
    Err("timestamp timed out".into())
}
