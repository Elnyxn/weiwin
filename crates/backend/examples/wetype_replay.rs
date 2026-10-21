//! Replay authorized, privately captured audio with a new ASR session/key/signature.
//! This never operates an IME/WeChat process or a target text box.
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};
use wechat_voice_backend::wetype::{Client, Device};
use wechat_voice_protocol::{wetype, wire};

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 {
        return Err("usage: wetype_replay PRIVATE_DEVICE_JSON PRIVATE_CAPTURE".into());
    }
    let device: Device = serde_json::from_slice(&fs::read(&args[0])?)?;
    let root = PathBuf::from(&args[1]);
    let events: Vec<serde_json::Value> = fs::read_to_string(root.join("events.jsonl"))?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    let mut packets = Vec::new();
    let mut start_ms = None;
    for event in events {
        let (Some(pid), Some(seq), Some(time)) = (
            event["pid"].as_u64(),
            event["seq"].as_u64(),
            event["time"].as_u64(),
        ) else {
            continue;
        };
        let p = root.join(format!("{pid}-{seq}-plain.bin"));
        if event["kind"] != "up" || !p.exists() {
            continue;
        }
        let body = fs::read(p)?;
        let start = *start_ms.get_or_insert(time);
        packets.push((Duration::from_millis(time - start), body));
    }
    if packets.is_empty() || packets.len() > 512 {
        return Err("invalid capture packet count".into());
    }
    let mut entropy = [0; 16];
    getrandom::fill(&mut entropy)?;
    let session_key = format!(
        "{:024}",
        u128::from_le_bytes(entropy) % 1_000_000_000_000_000_000_000_000
    );
    let mut client = Client::bootstrap(device)?;
    let started = Instant::now();
    let mut first_text = None;
    let mut partials = 0;
    let mut finals = 0;
    let mut last = String::new();
    let mut receive =
        |client: &mut Client| -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
            if let Some(response) = client.poll()? {
                if let Some(folder) = std::env::var_os("WETYPE_TRACE_DIR") {
                    let folder = PathBuf::from(folder);
                    fs::create_dir_all(&folder)?;
                    fs::write(
                        folder.join(format!("asr-{}.bin", response.id)),
                        &response.body,
                    )?;
                }
                let result = wetype::decode_response(&response.body)?;
                if result.session_key != session_key {
                    return Err("wrong ASR session key".into());
                }
                let text = result.polished_text.unwrap_or(result.raw_text);
                if !text.is_empty() && first_text.is_none() {
                    first_text = Some(started.elapsed().as_millis());
                }
                if text != last || result.is_final() {
                    if result.is_final() {
                        finals += 1;
                    } else {
                        partials += 1;
                    }
                    last = text.into();
                }
                return Ok(result.is_final());
            }
            Ok(false)
        };
    let mut final_seen = false;
    let mut last_total = 0;
    for (time, body) in packets {
        while started.elapsed() < time {
            final_seen |= receive(&mut client)?;
        }
        let input = wire::bytes(&body, 1)?.ok_or("missing captured business body")?;
        let request = wetype::Request {
            session_key: &session_key,
            audio: wire::bytes(input, 4)?,
            chunk_sequence: wire::integer(input, 7)?.map(|n| n as u32),
            end: wire::integer(input, 6)? == Some(1),
            total_audio_bytes: wire::integer(input, 11)?.ok_or("missing captured audio size")?,
            target_process: "wechat-winh.exe",
        };
        last_total = request.total_audio_bytes;
        client.send_voice(&wetype::encode_request(&request)?)?;
        final_seen |= receive(&mut client)?;
    }
    let end = Instant::now() + Duration::from_secs(4);
    let mut last_end = Instant::now();
    while !final_seen && Instant::now() < end {
        final_seen |= receive(&mut client)?;
        if !final_seen && last_end.elapsed() >= Duration::from_millis(100) {
            client.send_voice(&wetype::encode_request(&wetype::Request {
                session_key: &session_key,
                audio: None,
                chunk_sequence: None,
                end: true,
                total_audio_bytes: last_total,
                target_process: "wechat-winh.exe",
            })?)?;
            last_end = Instant::now();
        }
    }
    client.close();
    println!(
        "new_session=true partials={partials} finals={finals} final_chars={} first_text_ms={first_text:?}",
        last.chars().count()
    );
    if finals == 0 || last.is_empty() {
        return Err("no nonempty final recognition".into());
    }
    Ok(())
}
