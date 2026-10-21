//! Blocking transport: one connection, bounded frames, no retry loop.
pub mod account;
#[cfg(windows)]
pub mod check;
#[cfg(windows)]
pub mod engine;
#[cfg(windows)]
pub mod runtime_account;
pub mod settings;
pub mod wetype;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    error::Error,
    fs, io,
    net::{TcpStream, ToSocketAddrs},
    path::Path,
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket, client::IntoClientRequest, http, stream::MaybeTlsStream};
use wechat_voice_protocol::{receiver::Receiver, wire};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Deserialize)]
pub struct Connection {
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

impl Connection {
    pub fn official() -> Self {
        Self {
            url: "wss://szilinkshort.weixin.com".into(),
            headers: BTreeMap::new(),
        }
    }
    pub fn load(path: &Path) -> Result<Self> {
        let metadata = fs::metadata(path)?;
        if metadata.len() > 32_768 {
            return Err("connection config exceeds limit".into());
        }
        let value: Self = serde_json::from_slice(&fs::read(path)?)?;
        // Captured credentials must never be forwarded to arbitrary hosts.
        if value.url != "wss://szilinkshort.weixin.com"
            && value.url != "wss://szilinkshort.weixin.com/"
        {
            return Err("unexpected speech endpoint".into());
        }
        Ok(value)
    }
}

pub struct Session {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    pub recognition: Receiver,
    pub sent: usize,
    pub received: usize,
    pub last_frame: bytes::Bytes,
    trace_file: Option<std::path::PathBuf>,
}

impl Session {
    pub fn connect(config: &Connection) -> Result<Self> {
        let mut request = config.url.as_str().into_client_request()?;
        request.headers_mut().insert(
            "sec-websocket-protocol",
            http::HeaderValue::from_static("wxws_pb"),
        );
        for (name, value) in &config.headers {
            let name = http::HeaderName::from_bytes(name.as_bytes())?;
            // WebSocket handshake fields are generated afresh by the client.
            if matches!(
                name.as_str(),
                "connection" | "upgrade" | "sec-websocket-key" | "sec-websocket-version"
            ) {
                continue;
            }
            request
                .headers_mut()
                .insert(name, http::HeaderValue::from_str(value)?);
        }
        let address = ("szilinkshort.weixin.com", 443)
            .to_socket_addrs()?
            .find(|address| address.is_ipv4())
            .ok_or("speech host has no IPv4 address")?;
        let tcp = TcpStream::connect_timeout(&address, Duration::from_secs(5))?;
        tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
        tcp.set_write_timeout(Some(Duration::from_secs(5)))?;
        let (socket, _) = tungstenite::client_tls_with_config(request, tcp, None, None)?;
        let tcp = match socket.get_ref() {
            MaybeTlsStream::Plain(stream) => stream,
            #[cfg(windows)]
            MaybeTlsStream::NativeTls(stream) => stream.get_ref(),
            #[cfg(not(windows))]
            MaybeTlsStream::Rustls(stream) => &stream.sock,
            _ => return Err("unsupported TLS transport".into()),
        };
        tcp.set_read_timeout(Some(Duration::from_millis(25)))?;
        tcp.set_write_timeout(Some(Duration::from_secs(5)))?;
        tcp.set_nodelay(true)?;
        Ok(Self {
            socket,
            recognition: Receiver::default(),
            sent: 0,
            received: 0,
            last_frame: bytes::Bytes::new(),
            trace_file: None,
        })
    }

    pub fn send(&mut self, frame: Vec<u8>) -> Result<()> {
        if frame.is_empty() || frame.len() > wire::MAX_FRAME {
            return Err("invalid outgoing frame size".into());
        }
        self.socket.send(Message::Binary(frame.into()))?;
        self.sent += 1;
        Ok(())
    }

    pub fn trace_to(&mut self, path: std::path::PathBuf) {
        self.trace_file = Some(path);
    }

    /// Finish this independent WebSocket connection gracefully.
    pub fn close(&mut self) -> Result<()> {
        self.socket.close(None)?;
        Ok(())
    }

    /// Returns false when the remote endpoint closes the session.
    pub fn poll(&mut self) -> Result<bool> {
        match self.socket.read() {
            Ok(Message::Binary(frame)) => {
                self.received += 1;
                self.last_frame = frame.clone();
                if let Some(path) = &self.trace_file {
                    fs::write(path, &frame)?;
                    if let Some(sequence) = wire::integer(&frame, 2)?.filter(|&n| n <= 4) {
                        fs::write(path.with_extension(format!("seq{sequence}.bin")), &frame)?;
                    }
                }
                self.recognition.ingest(&frame)?;
                Ok(true)
            }
            Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => Ok(false),
            Ok(_) => Ok(true),
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(true)
            }
            Err(error) => Err(error.into()),
        }
    }
}

/// Transport replay for protocol verification, not fresh microphone acceptance.
pub fn replay(root: &Path) -> Result<Session> {
    let connection = Connection::load(&root.join("private-connection.json"))?;
    let mut packets = fs::read_dir(root)?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with("-ws-upstream.bin"))
        })
        .collect::<Vec<_>>();
    packets.sort();
    if packets.is_empty() || packets.len() > 512 {
        return Err("capture must contain 1..512 WebSocket send frames".into());
    }
    let first = fs::metadata(&packets[0])?.modified()?;
    let mut session = Session::connect(&connection)?;
    let started = Instant::now();
    for path in packets {
        let meta = fs::metadata(&path)?;
        if meta.len() > wire::MAX_FRAME as u64 {
            return Err("capture frame exceeds limit".into());
        }
        let time = meta.modified()?.duration_since(first).unwrap_or_default();
        if time > Duration::from_secs(60) {
            return Err("capture duration exceeds limit".into());
        }
        while started.elapsed() < time {
            if !session.poll()? {
                return Err("remote closed before replay finished".into());
            }
        }
        session.send(fs::read(path)?)?;
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && session.poll()? {}
    Ok(session)
}
