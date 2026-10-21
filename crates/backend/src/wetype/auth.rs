use super::{Response, Transport, crypto, ecdh::SecretKey, request};
use crate::Result;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use wechat_voice_protocol::wire;

const BOOTSTRAP: &[u8; 32] = include_bytes!("../../../../assets/wetype-bootstrap.bin");
const SIGN_SALT: &[u8; 16] = include_bytes!("../../../../assets/wetype-sign-salt.bin");
const VERSION: &str = "3.0.0.37";

#[derive(Serialize, Deserialize)]
pub struct Device {
    #[serde(default)]
    pub uuid: String,
    pub code: String,
    #[serde(default)]
    pub uin: u64,
    #[serde(default = "default_scene")]
    pub scene: u8,
}
fn default_scene() -> u8 {
    1
}

/// GenUinReq_V2 field 4 is a registration UUID, not an account identity.
/// The server rejects an empty value with HTTP 400/5001, while any non-empty
/// ASCII value was accepted in differential checks; a random v4 is used here.
fn registration_uuid() -> Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| "device UUID random generation failed")?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut text = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            text.push('-');
        }
        text.push_str(&format!("{byte:02x}"));
    }
    Ok(text)
}

impl Device {
    pub fn generate() -> Result<Self> {
        let mut bytes = [0; 48];
        getrandom::fill(&mut bytes).map_err(|_| "device random generation failed")?;
        // Match the observed anonymous Windows device model (17-byte left padding).
        let mut code = String::from("WIN000000000000My PC");
        for &byte in &bytes[16..28] {
            code.push((b'a' + byte % 26) as char);
        }
        let mut checksum = Md5::new();
        checksum.update(code.as_bytes());
        checksum.update(SIGN_SALT);
        code.push_str(&crypto::hex_upper(&checksum.finalize()));
        Ok(Self {
            uuid: registration_uuid()?,
            code,
            uin: 0,
            scene: 1,
        })
    }

    fn path() -> Result<std::path::PathBuf> {
        if let Some(path) = std::env::var_os("WETYPE_DEVICE_FILE") {
            return Ok(path.into());
        }
        Ok(std::path::PathBuf::from(
            std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA unavailable")?,
        )
        .join("WeChatWinH/wetype-device.json"))
    }

    pub fn load_default() -> Result<Self> {
        let path = Self::path()?;
        match std::fs::read(&path) {
            Ok(data) if data.len() <= 4096 => Ok(serde_json::from_slice(&data)?),
            Ok(_) => Err("anonymous device config exceeds limit".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let device = Self::generate()?;
                device.save_default()?;
                Ok(device)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn save_default(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&temporary, serde_json::to_vec(self)?)?;
        std::fs::rename(temporary, path)?;
        Ok(())
    }
}

struct Clock {
    server_ms: u64,
    started: Instant,
}
impl Clock {
    fn now(&self) -> u64 {
        self.server_ms
            .saturating_add(self.started.elapsed().as_millis().min(u64::MAX as u128) as u64)
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Bootstrap,
    Generate,
    Business(u32, u32),
}

pub struct Client {
    transport: Transport,
    device: Device,
    key: [u8; 32],
    clock: Clock,
    next: u32,
    gen_uin_token: String,
    server_public_key: String,
}

fn headers(
    kind: Kind,
    device: &Device,
    id: u32,
    timestamp: u64,
    body: &[u8],
) -> Result<BTreeMap<String, String>> {
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(|_| "trace random generation failed")?;
    let mut headers = BTreeMap::from([
        ("Kb-Uin".into(), device.uin.to_string()),
        ("Kb-OsType".into(), "6".into()),
        ("Kb-Version".into(), VERSION.into()),
        ("Kb-SystemVersion".into(), "Windows.10.0.26300.0".into()),
        ("Kb-PackageType".into(), "3".into()),
        ("Use_DebugNet".into(), "0".into()),
        ("Kb-TimeStamp".into(), timestamp.to_string()),
        ("Kb-BodyMd5".into(), crypto::hex_upper(&Md5::digest(body))),
        (
            "Kb-TraceId".into(),
            random.iter().map(|&b| (b'a' + b % 26) as char).collect(),
        ),
        ("Kb-TaskId".into(), id.to_string()),
        ("Kb-Scene".into(), device.scene.to_string()),
        ("Content-Length".into(), body.len().to_string()),
        ("Kb-CompressionType".into(), "0".into()),
        ("Content-Type".into(), "application/octet-stream".into()),
        ("HOST".into(), "wetype.weixin.qq.com".into()),
    ]);
    match kind {
        Kind::Bootstrap => {
            headers.insert("Kb-DeviceCodeRestrictionV2".into(), "1".into());
        }
        Kind::Generate => {
            headers.remove("Kb-Uin");
        }
        Kind::Business(cmd, subcmd) => {
            headers.insert("Kb-CmdId".into(), cmd.to_string());
            headers.insert("Kb-SubCmdId".into(), subcmd.to_string());
        }
    }
    let fields: &[&str] = match kind {
        Kind::Bootstrap => &[
            "Kb-OsType",
            "Kb-Version",
            "Kb-Scene",
            "Kb-TimeStamp",
            "Kb-BodyMd5",
            "Kb-TraceId",
            "Kb-TaskId",
        ],
        Kind::Generate => &[], // The server token is added before this signature is computed.
        Kind::Business(..) => &[
            "Kb-OsType",
            "Kb-Version",
            "Kb-Scene",
            "Kb-CmdId",
            "Kb-SubCmdId",
            "Kb-TimeStamp",
            "Kb-BodyMd5",
            "Kb-Uin",
            "Kb-TraceId",
            "Kb-TaskId",
        ],
    };
    let mut hash = Sha256::new();
    for field in fields {
        hash.update(headers[*field].as_bytes());
    }
    hash.update(SIGN_SALT);
    headers.insert("Kb-Sign".into(), crypto::hex_upper(&hash.finalize()));
    Ok(headers)
}

pub(super) fn check_cancelled(cancelled: &dyn Fn() -> bool) -> Result<()> {
    if cancelled() {
        return Err("voice initialization cancelled".into());
    }
    Ok(())
}

fn wait(transport: &mut Transport, id: u32, cancelled: &dyn Fn() -> bool) -> Result<Response> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(8) {
        check_cancelled(cancelled)?;
        if let Some(response) = transport.poll()? {
            check_cancelled(cancelled)?;
            if response.id != u64::from(id) {
                continue;
            }
            if response.status != 200 {
                if let Some(root) = std::env::var_os("WETYPE_TRACE_DIR") {
                    let root = std::path::PathBuf::from(root);
                    std::fs::create_dir_all(&root)?;
                    std::fs::write(root.join(format!("http-{id}-error.bin")), &response.body)?;
                    std::fs::write(
                        root.join(format!("http-{id}-headers.json")),
                        serde_json::to_vec(&response.headers)?,
                    )?;
                }
                return Err(format!(
                    "WeType HTTP {} during request {id} ({} bytes)",
                    response.status,
                    response.body.len()
                )
                .into());
            }
            return Ok(response);
        }
    }
    Err("WeType handshake timed out".into())
}

impl Client {
    pub fn from_default_device() -> Result<Self> {
        Self::from_default_device_cancellable(&|| false)
    }

    pub fn from_default_device_cancellable(cancelled: &dyn Fn() -> bool) -> Result<Self> {
        check_cancelled(cancelled)?;
        let client = Self::authenticate_cancellable(Device::load_default()?, cancelled)?;
        check_cancelled(cancelled)?;
        client.device.save_default()?;
        Ok(client)
    }
    /// Establish an independent key. No installed application or user session is read.
    pub fn bootstrap(device: Device) -> Result<Self> {
        Self::bootstrap_cancellable(device, &|| false)
    }

    fn bootstrap_cancellable(mut device: Device, cancelled: &dyn Fn() -> bool) -> Result<Self> {
        check_cancelled(cancelled)?;
        let machine_name = super::machine::name()?;
        // Older saved device files predate the GenUin registration UUID.
        if device.uuid.is_empty() {
            device.uuid = registration_uuid()?;
        }
        if device.code.len() != 64
            || device.code.chars().any(char::is_control)
            || device.uuid.len() > 128
            || device.uuid.chars().any(char::is_control)
        {
            return Err("invalid anonymous device identity".into());
        }
        let mut transport = Transport::connect_cancellable(cancelled)?;
        let timestamp_headers = BTreeMap::from([
            ("Content-Type".into(), "application/octet-stream".into()),
            ("HOST".into(), "wetype.weixin.qq.com".into()),
        ]);
        transport.send(request(1, "/timestamp", &timestamp_headers, &[])?)?;
        let timestamp = wait(&mut transport, 1, cancelled)?;
        let server_ms = wire::integer(&timestamp.body, 1)?.ok_or("missing server clock")?;
        if !(1_000_000_000_000..100_000_000_000_000).contains(&server_ms) {
            return Err("invalid WeType server clock".into());
        }
        let clock = Clock {
            server_ms,
            started: Instant::now(),
        };
        let secret = SecretKey::generate()?;
        check_cancelled(cancelled)?;
        let public = crypto::hex_upper(&secret.public_key());
        let mut body = Vec::with_capacity(180);
        wire::put_bytes(&mut body, 1, public.as_bytes());
        wire::put_bytes(&mut body, 2, b"");
        wire::put_integer(&mut body, 3, u64::from(device.uin == 0));
        wire::put_bytes(&mut body, 4, device.code.as_bytes());
        if device.uin == 0 {
            wire::put_bytes(&mut body, 6, b"");
        } else {
            wire::put_integer(&mut body, 5, device.uin);
        }
        wire::put_bytes(&mut body, 7, b"");
        wire::put_bytes(&mut body, 8, machine_name.as_bytes());
        wire::put_bytes(&mut body, 9, b"");
        let encrypted = crypto::encrypt(BOOTSTRAP, &body);
        let id = 0x7fff_fffe;
        let headers = headers(Kind::Bootstrap, &device, id, clock.now(), &encrypted)?;
        transport.send(request(id, "/oauth_pubkey_v2", &headers, &encrypted)?)?;
        let response = wait(&mut transport, id, cancelled)?;
        let plaintext = crypto::decrypt(BOOTSTRAP, &response.body)?;
        let status = wire::integer(&plaintext, 1)?.ok_or("missing key exchange status")?;
        if status != 0 {
            return Err(format!("WeType key exchange code {}", status as i64).into());
        }
        let peer = wire::text(&plaintext, 2)?.ok_or("missing server public key")?;
        let key = secret.agree(&crypto::unhex(peer)?)?;
        check_cancelled(cancelled)?;
        let gen_uin_token = wire::text(&plaintext, 4)?.unwrap_or_default().to_owned();
        Ok(Self {
            transport,
            device,
            key,
            clock,
            next: 3,
            gen_uin_token,
            server_public_key: peer.to_owned(),
        })
    }

    pub fn authenticate(device: Device) -> Result<Self> {
        Self::authenticate_cancellable(device, &|| false)
    }

    fn authenticate_cancellable(device: Device, cancelled: &dyn Fn() -> bool) -> Result<Self> {
        let mut client = Self::bootstrap_cancellable(device, cancelled)?;
        if !client.has_identity() {
            client.generate_uin(cancelled)?;
        }
        Ok(client)
    }

    fn generate_uin(&mut self, cancelled: &dyn Fn() -> bool) -> Result<()> {
        check_cancelled(cancelled)?;
        if self.gen_uin_token.is_empty() {
            return Err("server did not issue an anonymous registration token".into());
        }
        let mut body = Vec::with_capacity(200);
        wire::put_bytes(&mut body, 1, self.device.code.as_bytes());
        // Native saves this OAuth response key to ppp immediately before it
        // reads GetLatestPublicKey for GenUinReq_V2 field 2.
        wire::put_bytes(&mut body, 2, self.server_public_key.as_bytes());
        wire::put_bytes(&mut body, 4, self.device.uuid.as_bytes());
        wire::put_bytes(&mut body, 5, b"");
        let body = crypto::encrypt(&self.key, &body);
        let id = 0x7fff_fffd;
        let mut headers = headers(Kind::Generate, &self.device, id, self.clock.now(), &body)?;
        headers.insert("Kb-GenUinToken".into(), self.gen_uin_token.clone());
        headers.insert("Kb-DeviceCodeRestrictionV2".into(), "1".into());
        let mut hash = Sha256::new();
        for field in [
            "Kb-OsType",
            "Kb-Version",
            "Kb-Scene",
            "Kb-TimeStamp",
            "Kb-BodyMd5",
            "Kb-GenUinToken",
            "Kb-TraceId",
            "Kb-TaskId",
        ] {
            hash.update(headers[field].as_bytes());
        }
        hash.update(SIGN_SALT);
        headers.insert("Kb-Sign".into(), crypto::hex_upper(&hash.finalize()));
        self.transport
            .send(request(id, "/gen_uin_v2", &headers, &body)?)?;
        let response = wait(&mut self.transport, id, cancelled)?;
        let response = crypto::decrypt(&self.key, &response.body)?;
        let code = wire::integer(&response, 1)?.ok_or("missing anonymous registration status")?;
        if code != 0 {
            return Err(format!("WeType registration code {}", code as i32).into());
        }
        self.device.uin = wire::integer(&response, 2)?
            .filter(|&n| n != 0)
            .ok_or("missing anonymous device identity")?;
        self.gen_uin_token.clear();
        Ok(())
    }

    pub fn has_identity(&self) -> bool {
        self.device.uin != 0
    }

    /// Persisted anonymous identity, including the assigned uin after registration.
    pub fn device(&self) -> &Device {
        &self.device
    }

    pub fn send_voice(&mut self, body: &[u8]) -> Result<u32> {
        if !self.has_identity() {
            return Err("anonymous WeType identity has not been initialized".into());
        }
        let id = self.next;
        self.next = self
            .next
            .checked_add(1)
            .ok_or("WeType request count exceeded")?;
        let body = crypto::encrypt(&self.key, body);
        let mut headers = headers(
            Kind::Business(4548, 0),
            &self.device,
            id,
            self.clock.now(),
            &body,
        )?;
        headers.insert(
            "Kb-SharedKeySuffix".into(),
            std::str::from_utf8(&self.key[28..])?.into(),
        );
        self.transport
            .send(request(id, "/api_v2", &headers, &body)?)?;
        Ok(id)
    }

    pub fn poll(&mut self) -> Result<Option<Response>> {
        self.transport
            .poll()?
            .map(|mut response| {
                if response.status != 200 {
                    return Err(format!("WeType HTTP {}", response.status).into());
                }
                response.body = crypto::decrypt(&self.key, &response.body)?;
                Ok(response)
            })
            .transpose()
    }

    pub fn close(&mut self) {
        self.transport.close();
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        for b in &mut self.key {
            unsafe {
                std::ptr::write_volatile(b, 0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_initialization_does_not_contact_the_service() {
        let device = Device::generate().unwrap();
        let started = Instant::now();
        let error = match Client::authenticate_cancellable(device, &|| true) {
            Err(error) => error,
            Ok(_) => panic!("cancelled initialization succeeded"),
        };
        assert_eq!(error.to_string(), "voice initialization cancelled");
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn registration_uuid_is_lowercase_v4() {
        let uuid = registration_uuid().unwrap();
        assert_eq!(uuid.len(), 36);
        for (index, byte) in uuid.bytes().enumerate() {
            match index {
                8 | 13 | 18 | 23 => assert_eq!(byte, b'-'),
                _ => assert!(byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            }
        }
        assert_eq!(uuid.as_bytes()[14], b'4');
        assert!(matches!(uuid.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
        assert_ne!(uuid, registration_uuid().unwrap());
    }

    #[test]
    fn generated_device_has_registration_uuid() {
        let device = Device::generate().unwrap();
        assert_eq!(device.code.len(), 64);
        assert_eq!(device.uuid.len(), 36);
        assert_eq!(device.uin, 0);
    }
}
