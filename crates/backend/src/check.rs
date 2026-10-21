//! Independent init request; reads account material without changing the client.
use crate::{Connection, Result, Session, runtime_account};
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};
use wechat_voice_protocol::{rpc, stream, wire};

pub(crate) fn new_id() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| format!("random source failed: {e}"))?;
    bytes[6] = (bytes[6] & 15) | 0x40;
    bytes[8] = (bytes[8] & 63) | 0x80;
    Ok(format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes(bytes[..4].try_into().unwrap()),
        u16::from_be_bytes(bytes[4..6].try_into().unwrap()),
        u16::from_be_bytes(bytes[6..8].try_into().unwrap()),
        u16::from_be_bytes(bytes[8..10].try_into().unwrap()),
        u64::from_be_bytes([
            0, 0, bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
        ])
    ))
}

pub fn init(pid: u32) -> Result<usize> {
    let account = runtime_account::read(pid)?;
    let identity = rpc::Identity {
        cloud_uin: account.app_uin,
        wss_token: &account.wss_token,
        auth_type: account.auth_type(),
        host: "shortcloud.weixin.com",
        app_id: (account.session_fields & 4 != 0).then_some(account.app_id.as_str()),
        client_version: (account.session_fields & 32 != 0).then_some(account.client_version),
        server_group_key: (account.session_fields & 64 != 0).then_some(account.server_group_key),
    };
    let body = stream::init(
        account.device_id.as_bytes(),
        &new_id()?,
        stream::VOICE_INPUT_BUSINESS,
    );
    let frame = rpc::request(1, &identity, rpc::INIT_PATH, &body)?;
    let mut session = Session::connect(&Connection::official())?;
    let trace =
        PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is unavailable")?)
            .join("WeChatWinH/dev");
    fs::create_dir_all(&trace)?;
    session.trace_to(trace.join("last-init-response.bin"));
    session.send(frame)?;
    let deadline = Instant::now() + Duration::from_secs(8);
    let result = loop {
        let received = session.poll();
        if !session.last_frame.is_empty() {
            let root = PathBuf::from(
                std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is unavailable")?,
            )
            .join("WeChatWinH/dev");
            fs::create_dir_all(&root)?;
            fs::write(root.join("init-response.bin"), &session.last_frame)?;
        }
        match received {
            Err(error) => break Err(error),
            Ok(false) => break Err("service closed before init acknowledgement".into()),
            Ok(true) if session.received > 0 => break Ok(session.last_frame.len()),
            _ if Instant::now() >= deadline => break Err("init acknowledgement timed out".into()),
            _ => (),
        }
    };
    // Only closes our own independent connection, never a client process.
    let _ = session.close();
    result
}

pub fn replay_audio(pid: u32, root: &std::path::Path) -> Result<Session> {
    replay_audio_key(pid, root, false)
}

pub fn replay_audio_key(pid: u32, root: &std::path::Path, opaque_key: bool) -> Result<Session> {
    let account = runtime_account::read(pid)?;
    let identity = rpc::Identity {
        cloud_uin: account.app_uin,
        wss_token: &account.wss_token,
        auth_type: account.auth_type(),
        host: "shortcloud.weixin.com",
        app_id: (account.session_fields & 4 != 0).then_some(account.app_id.as_str()),
        client_version: (account.session_fields & 32 != 0).then_some(account.client_version),
        server_group_key: (account.session_fields & 64 != 0).then_some(account.server_group_key),
    };
    let mut paths = fs::read_dir(root)?
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().is_some_and(|s| {
                s.to_string_lossy().ends_with("-upstream.bin")
                    && !s.to_string_lossy().contains("-ws-")
            })
        })
        .collect::<Vec<_>>();
    paths.sort();
    if paths.is_empty() || paths.len() > 512 {
        return Err("invalid audio capture count".into());
    }
    let mut pieces = Vec::new();
    let mut original_id = Vec::new();
    for path in paths {
        if fs::metadata(&path)?.len() > 65_536 {
            return Err("capture frame too large".into());
        }
        let bytes = fs::read(path)?;
        let id = wire::bytes(&bytes, 1)?.ok_or("capture stream id is missing")?;
        if original_id.is_empty() {
            original_id.extend_from_slice(id);
        }
        if id != original_id {
            break;
        }
        pieces.push(
            wire::bytes(&bytes, 3)?
                .ok_or("capture audio is missing")?
                .to_vec(),
        );
    }
    let context_path = fs::read_dir(root)?
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|s| s.to_string_lossy().ends_with("-create-config.bin"))
        })
        .min()
        .ok_or("create context is missing")?;
    let context = fs::read(context_path)?;
    if context.len() > 4096 {
        return Err("create context exceeds limit".into());
    }
    // The observed client key is wxid-prefix_GetTickCount_serial_scene.
    // Reusing a completed recognition key is not a fresh audio session.
    let key = wire::text(&context, 3)?.ok_or("recognition key is missing")?;
    let mut parts = key.rsplitn(4, '_');
    let scene = parts.next().ok_or("recognition key scene is missing")?;
    let serial = parts.next().ok_or("recognition key serial is missing")?;
    let _old_tick = parts.next().ok_or("recognition key clock is missing")?;
    let prefix = parts.next().ok_or("recognition key identity is missing")?;
    let tick = unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount() };
    let key = if opaque_key {
        new_id()?
    } else {
        format!("{prefix}_{tick}_{serial}_{scene}")
    };
    let context = wire::replace_bytes(&context, 3, key.as_bytes())?;
    for piece in &mut pieces {
        *piece = wire::replace_bytes(piece, 3, key.as_bytes())?;
    }
    let stream_id = new_id()?;
    let mut session = Session::connect(&Connection::official())?;
    let trace =
        PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is unavailable")?)
            .join("WeChatWinH/dev");
    fs::create_dir_all(&trace)?;
    session.trace_to(trace.join("last-replay-response.bin"));
    let body = stream::init(
        account.device_id.as_bytes(),
        &stream_id,
        stream::VOICE_INPUT_BUSINESS,
    );
    session.send(rpc::request(1, &identity, rpc::INIT_PATH, &body)?)?;
    let deadline = Instant::now() + Duration::from_secs(8);
    while session.received == 0 {
        if !session.poll()? {
            return Err("service closed before init".into());
        }
        if Instant::now() >= deadline {
            return Err("init timed out".into());
        }
    }
    let start = Instant::now();
    let mut request_sequence = 1_u32;
    for (index, piece) in pieces.iter().enumerate() {
        let due = Duration::from_millis(index as u64 * 120);
        while start.elapsed() < due {
            if !session.poll()? {
                return Err("service closed during audio replay".into());
            }
        }
        let message = stream::sync(&stream::Sync {
            device_id: account.device_id.as_bytes(),
            stream_id: &stream_id,
            business: stream::VOICE_INPUT_BUSINESS,
            piece_sequence: index as u32 + 1,
            piece: Some(piece),
            down_ack: session.recognition.down_ack,
            context: &context,
        });
        request_sequence += 1;
        session.send(rpc::request(
            request_sequence,
            &identity,
            rpc::SYNC_PATH,
            &message,
        )?)?;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !session.poll()? {
            break;
        }
        let message = stream::sync(&stream::Sync {
            device_id: account.device_id.as_bytes(),
            stream_id: &stream_id,
            business: stream::VOICE_INPUT_BUSINESS,
            piece_sequence: 0,
            piece: None,
            down_ack: session.recognition.down_ack,
            context: &context,
        });
        request_sequence += 1;
        session.send(rpc::request(
            request_sequence,
            &identity,
            rpc::SYNC_PATH,
            &message,
        )?)?;
        std::thread::sleep(Duration::from_millis(100));
    }
    let root =
        PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is unavailable")?)
            .join("WeChatWinH/dev");
    fs::create_dir_all(&root)?;
    fs::write(root.join("replay-response.bin"), &session.last_frame)?;
    let _ = session.close();
    if session.recognition.updates == 0 {
        return Err("no transcription returned for replayed audio".into());
    }
    Ok(session)
}
