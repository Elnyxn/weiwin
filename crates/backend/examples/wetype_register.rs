//! Register a fresh anonymous device or reuse a persisted one, then save it.
//! No installed application, account, or user session is read.
use std::{fs, path::PathBuf};
use wechat_voice_backend::wetype::{Client, Device};

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: wetype_register DEVICE_JSON")?;
    let device = match fs::read(&path) {
        Ok(data) => serde_json::from_slice(&data)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Device::generate()?,
        Err(error) => return Err(error.into()),
    };
    let fresh = device.uin == 0;
    let mut client = Client::authenticate(device)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, serde_json::to_vec_pretty(client.device())?)?;
    println!("registered={fresh} anonymous_uin={}", client.has_identity());
    client.close();
    Ok(())
}
