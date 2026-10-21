use wechat_voice_backend::wetype::{Client, Device};
fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let started = std::time::Instant::now();
    let device = if let Some(path) = std::env::args_os().nth(1) {
        serde_json::from_slice(&std::fs::read(path)?)?
    } else {
        Device::generate()?
    };
    let mut client = Client::authenticate(device)?;
    println!(
        "independent_key_exchange=ok anonymous_uin={} elapsed_ms={}",
        client.has_identity(),
        started.elapsed().as_millis()
    );
    client.close();
    Ok(())
}
