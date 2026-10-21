use std::{error::Error, fs, path::Path};
use wechat_voice_protocol::{asr, wire};

fn assemble(root: &Path) -> Result<(), Box<dyn Error + Send + Sync>> {
    use std::collections::BTreeMap;
    use wechat_voice_protocol::{receiver::Receiver, wire::Value};
    let mut paths = fs::read_dir(root)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|s| s.to_string_lossy().ends_with("-ws-downstream.bin"))
        })
        .collect::<Vec<_>>();
    paths.sort();
    if paths.len() > 1024 {
        return Err("too many response frames".into());
    }
    let mut receivers: BTreeMap<Vec<u8>, Receiver> = BTreeMap::new();
    for path in paths {
        if fs::metadata(&path)?.len() > wire::MAX_FRAME as u64 {
            return Err("frame exceeds limit".into());
        }
        let frame = fs::read(path)?;
        let Some(http) = wire::bytes(&frame, 4)? else {
            continue;
        };
        let Some(body) = wire::bytes(http, 5)? else {
            continue;
        };
        let Some(Value::Bytes(id)) = wire::find(body, 2)? else {
            continue;
        };
        let mut first_sequence = None;
        wechat_voice_protocol::stream::visit_downstream(&frame, |sequence| {
            first_sequence = Some(first_sequence.map_or(sequence, |old: u32| old.min(sequence)));
        })?;
        let receiver = receivers
            .entry(id.to_vec())
            .or_insert_with(|| Receiver::starting_at(first_sequence.unwrap_or(1)));
        receiver.ingest(&frame)?;
        receiver.events.clear();
    }
    if receivers.is_empty() {
        return Err("no speech streams found".into());
    }
    let updates: usize = receivers.values().map(|r| r.updates).sum();
    let chars: Vec<usize> = receivers.values().map(|r| r.text.chars().count()).collect();
    println!(
        "streams={} updates={updates} text_chars={chars:?}",
        receivers.len()
    );
    Ok(())
}

fn inspect(root: &Path) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut files = fs::read_dir(root)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with("-ws-downstream.bin"))
        })
        .collect::<Vec<_>>();
    files.sort();
    if files.len() > 1024 {
        return Err("too many frames".into());
    }
    let mut updates = 0;
    let mut changes = 0;
    let mut last = String::new();
    let mut max_chars = 0;
    let mut bytes = 0;
    let mut failures = 0;
    for path in &files {
        if fs::metadata(path)?.len() > wire::MAX_FRAME as u64 {
            return Err("frame exceeds limit".into());
        }
        let frame = fs::read(path)?;
        bytes += frame.len();
        match asr::decode(&frame) {
            Ok(Some(value)) => {
                updates += 1;
                max_chars = max_chars.max(value.text.chars().count());
                if value.text != last {
                    changes += 1;
                    last.clear();
                    last.push_str(value.text);
                }
            }
            Ok(None) => (),
            Err(error) => {
                failures += 1;
                eprintln!("{}: {error}", path.file_name().unwrap().to_string_lossy());
            }
        }
    }
    println!(
        "frames={} bytes={bytes} updates={updates} changes={changes} max_chars={max_chars} errors={failures}",
        files.len()
    );
    if updates == 0 || failures > 0 {
        return Err("capture validation failed".into());
    }
    Ok(())
}

fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut args = std::env::args_os().skip(1);
    match (args.next(), args.next(), args.next()) {
        #[cfg(windows)]
        (Some(command), Some(pid), Some(seconds)) if command == "recognize-microphone" => {
            use wechat_voice_backend::engine::{self, Event, Source};
            let seconds: u64 = seconds.to_string_lossy().parse()?;
            if !(1..=15).contains(&seconds) { return Err("diagnostic duration must be 1..15 seconds".into()); }
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let timer = stop.clone();
            let timeout = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(seconds));
                timer.store(true, std::sync::atomic::Ordering::Release);
            });
            let text = engine::run(pid.to_string_lossy().parse()?, Source::Microphone(None), stop, |event| {
                match event {
                    Event::Started => println!("mic_session=ready"),
                    Event::Activity(phase) => println!("activity={phase}"),
                    Event::Input { name, muted, volume } => println!("device={name} muted={muted:?} volume={volume:?}"),
                    Event::Progress { samples, peak, rms, pieces, responses, partials, finals } => println!("samples={samples} peak={peak} rms={rms:.1} uploaded={pieces} responses={responses} partials={partials} finals={finals}"),
                    Event::Speech(_) => (),
                }
                Ok(())
            })?;
            let _ = timeout.join();
            println!("mic_session=completed chars={}", text.chars().count());
            Ok(())
        }
        #[cfg(windows)]
        (Some(command), None, None) if command == "microphone-info" => {
            for (role, device) in wechat_voice_audio::microphone::devices()? {
                println!("role={role} name={} muted={:?} volume={:?} id={}", device.name, device.muted, device.volume, device.id);
            }
            Ok(())
        }
        #[cfg(windows)]
        (Some(command), Some(pid), Some(path)) if command == "recognize-wave" => {
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut finals = 0;
            let text = wechat_voice_backend::engine::run(pid.to_string_lossy().parse()?, wechat_voice_backend::engine::Source::Wave(path.into()), stop, |event| {
                if let wechat_voice_backend::engine::Event::Speech(event) = event { finals += usize::from(event.final_result); }
                Ok(())
            })?;
            println!("recognition=accepted chars={} finals={finals}", text.chars().count());
            println!("{text}");
            if text.is_empty() { return Err("service returned no speech text".into()); }
            Ok(())
        }
        #[cfg(windows)]
        (Some(command), Some(pid), Some(path)) if command == "replay-audio-fresh-key" => {
            let session = wechat_voice_backend::check::replay_audio_key(pid.to_string_lossy().parse()?, Path::new(&path), true)?;
            println!("opaque_key=accepted updates={} chars={}", session.recognition.updates, session.recognition.text.chars().count());
            Ok(())
        }
        #[cfg(windows)]
        (Some(command), device, None) if command == "record-check" => {
            use std::time::{Instant, Duration};
            let device = device.as_ref().map(|s| s.to_string_lossy());
            let mut microphone = wechat_voice_audio::microphone::Microphone::open(device.as_deref())?;
            println!("device={} muted={:?} volume={:?}", microphone.device.name, microphone.device.muted, microphone.device.volume);
            let mut encoder = wechat_voice_audio::encoder::OpusStream::new()?;
            let mut pending = Vec::new();
            let (mut samples, mut bytes) = (0, 0);
            let (mut peak, mut square) = (0_u16, 0_f64);
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                microphone.poll(|chunk| {
                    samples += chunk.len();
                    for &sample in chunk { peak = peak.max(sample.unsigned_abs()); square += f64::from(sample).powi(2); }
                    pending.extend_from_slice(chunk);
                    while pending.len() >= wechat_voice_audio::encoder::CHUNK_SAMPLES {
                        bytes += encoder.encode(&pending[..1920])?.len();
                        pending.drain(..1920);
                    }
                    Ok(())
                })?;
            }
            if samples == 0 { return Err("microphone produced no samples".into()); }
            println!("microphone=ready samples={samples} encoded_bytes={bytes} peak={peak} rms={:.1}", (square / samples as f64).sqrt());
            Ok(())
        }
        #[cfg(windows)]
        (Some(command), Some(pid), Some(path)) if command == "replay-audio" => {
            let session = wechat_voice_backend::check::replay_audio(pid.to_string_lossy().parse()?, Path::new(&path))?;
            println!("audio_replay=accepted sent={} received={} updates={} chars={}", session.sent, session.received, session.recognition.updates, session.recognition.text.chars().count());
            Ok(())
        }
        #[cfg(windows)]
        (Some(command), Some(pid), None) if command == "auth-init-check" => {
            let pid = pid.to_string_lossy().parse()?;
            let bytes = wechat_voice_backend::check::init(pid)?;
            println!("init=accepted reply_bytes={bytes}");
            Ok(())
        }
        #[cfg(windows)]
        (Some(command), Some(pid), None) if command == "runtime-account-status" => {
            let pid: u32 = pid.to_string_lossy().parse()?;
            let account = wechat_voice_backend::runtime_account::read(pid)?;
            println!("runtime=available token_bytes={} device_bytes={} app_id_bytes={} context_type={} remote_auth=unverified", account.wss_token.len(), account.device_id.len(), account.app_id.len(), account.context_type);
            Ok(())
        }
        (Some(command), path, None) if command == "account-status" => {
            let path = match path {
                Some(path) => path.into(),
                None => wechat_voice_backend::account::CloudAccount::default_path()?,
            };
            let account = wechat_voice_backend::account::CloudAccount::read(&path)?;
            println!("account=available session_bytes={} device_bytes={} remote_auth=unverified", account.session.len(), account.device_id.len());
            Ok(())
        }
        (Some(command), Some(path), None) if command == "inspect-capture" => {
            inspect(Path::new(&path))
        }
        (Some(command), Some(path), None) if command == "assemble-capture" => assemble(Path::new(&path)),
        (Some(command), Some(path), None) if command == "connect-check" => {
            let config = wechat_voice_backend::Connection::load(
                &Path::new(&path).join("private-connection.json"),
            )?;
            let mut session = wechat_voice_backend::Session::connect(&config)?;
            session.close()?;
            println!("websocket=connected protocol=wxws_pb auth=unverified");
            Ok(())
        }
        (Some(command), Some(path), None) if command == "replay-capture" => {
            let session = wechat_voice_backend::replay(Path::new(&path))?;
            println!(
                "sent={} received={} updates={} changes={} chars={}",
                session.sent,
                session.received,
                session.recognition.updates,
                session.recognition.changes,
                session.recognition.text.chars().count()
            );
            if session.recognition.updates == 0 {
                return Err("no recognition result returned".into());
            }
            Ok(())
        }
        _ => Err("usage: wechat-voice <inspect-capture|replay-capture|connect-check> DIRECTORY | account-status [FILE]".into()),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
