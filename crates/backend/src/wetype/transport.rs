use crate::Result;
use std::{
    collections::BTreeMap,
    io,
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};
use tungstenite::{Message, WebSocket, client::IntoClientRequest, http, stream::MaybeTlsStream};
use wechat_voice_protocol::wire::{self, Value};

const HOST: &str = "wetype.weixin.qq.com";
pub struct Transport {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
}

impl Transport {
    pub fn connect() -> Result<Self> {
        Self::connect_cancellable(&|| false)
    }

    pub(super) fn connect_cancellable(cancelled: &dyn Fn() -> bool) -> Result<Self> {
        super::auth::check_cancelled(cancelled)?;
        let mut request = "wss://wetype.weixin.qq.com/ppp".into_client_request()?;
        request.headers_mut().insert(
            "sec-websocket-protocol",
            http::HeaderValue::from_static("wxws_pb"),
        );
        let address = (HOST, 443)
            .to_socket_addrs()?
            .find(|p| p.is_ipv4())
            .ok_or("WeType endpoint has no IPv4 address")?;
        super::auth::check_cancelled(cancelled)?;
        let tcp = TcpStream::connect_timeout(&address, Duration::from_secs(5))?;
        super::auth::check_cancelled(cancelled)?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
        tcp.set_write_timeout(Some(Duration::from_secs(5)))?;
        let (mut socket, _) = tungstenite::client_tls_with_config(request, tcp, None, None)?;
        super::auth::check_cancelled(cancelled)?;
        socket.set_config(|c| {
            c.max_message_size = Some(wire::MAX_FRAME);
            c.max_frame_size = Some(wire::MAX_FRAME);
        });
        let tcp = match socket.get_ref() {
            MaybeTlsStream::Plain(s) => s,
            #[cfg(windows)]
            MaybeTlsStream::NativeTls(s) => s.get_ref(),
            #[cfg(not(windows))]
            MaybeTlsStream::Rustls(s) => &s.sock,
            _ => return Err("unsupported TLS transport".into()),
        };
        tcp.set_read_timeout(Some(Duration::from_millis(25)))?;
        Ok(Self { socket })
    }

    pub fn send(&mut self, frame: Vec<u8>) -> Result<()> {
        if frame.is_empty() || frame.len() > wire::MAX_FRAME {
            return Err("invalid WeType outgoing frame size".into());
        }
        self.socket.send(Message::Binary(frame.into()))?;
        Ok(())
    }

    /// None means no complete application message within this bounded poll.
    pub fn poll(&mut self) -> Result<Option<Response>> {
        match self.socket.read() {
            Ok(Message::Binary(data)) => Ok(Some(Response::decode(&data)?)),
            Ok(Message::Ping(_)) => {
                self.socket.flush()?;
                Ok(None)
            }
            Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => {
                Err("WeType speech connection closed".into())
            }
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn close(&mut self) {
        let _ = self.socket.close(None);
    }
}

pub fn request(
    id: u32,
    path: &str,
    headers: &BTreeMap<String, String>,
    body: &[u8],
) -> Result<Vec<u8>> {
    if !matches!(
        path,
        "/timestamp" | "/oauth_pubkey_v2" | "/gen_uin_v2" | "/api_v2"
    ) {
        return Err("unexpected WeType API path".into());
    }
    if body.len() > wire::MAX_FRAME / 2 || headers.len() > 48 {
        return Err("WeType request exceeds limit".into());
    }
    let mut http = Vec::with_capacity(body.len() + 1024);
    wire::put_bytes(&mut http, 1, b"POST");
    wire::put_bytes(&mut http, 3, path.as_bytes());
    wire::put_bytes(&mut http, 4, b"");
    // Preserve the SDK's protobuf header order, including the two authentication
    // headers at the front. This is a custom HTTP envelope, not raw HTTP text.
    const ORDER: &[&str] = &[
        "Kb-Uin",
        "Kb-SharedKeySuffix",
        "Kb-CmdId",
        "Kb-SubCmdId",
        "Kb-GenUinToken",
        "Kb-DeviceCodeRestrictionV2",
        "Kb-OsType",
        "Kb-Version",
        "Kb-SystemVersion",
        "Kb-PackageType",
        "Use_DebugNet",
        "Kb-TimeStamp",
        "Kb-BodyMd5",
        "Kb-TraceId",
        "Kb-TaskId",
        "Kb-Scene",
        "Kb-Sign",
        "Content-Length",
        "Kb-CompressionType",
        "Content-Type",
        "HOST",
    ];
    let ordered = ORDER
        .iter()
        .filter_map(|name| headers.get_key_value(*name))
        .chain(
            headers
                .iter()
                .filter(|(name, _)| !ORDER.contains(&name.as_str())),
        );
    for (name, value) in ordered {
        if name.len() > 256 || value.len() > 16_384 {
            return Err("WeType header exceeds limit".into());
        }
        let mut header = Vec::with_capacity(name.len() + value.len() + 8);
        wire::put_bytes(&mut header, 1, name.as_bytes());
        wire::put_bytes(&mut header, 2, value.as_bytes());
        wire::put_bytes(&mut http, 5, &header);
    }
    wire::put_bytes(&mut http, 6, body);
    let mut frame = Vec::with_capacity(http.len() + 16);
    wire::put_integer(&mut frame, 1, 0);
    wire::put_integer(&mut frame, 2, 0);
    wire::put_integer(&mut frame, 3, id.into());
    wire::put_bytes(&mut frame, 5, &http);
    if frame.len() > wire::MAX_FRAME {
        return Err("WeType request exceeds limit".into());
    }
    Ok(frame)
}

pub struct Response {
    pub id: u64,
    pub status: u64,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn decode(data: &[u8]) -> Result<Self> {
        if let Some(code) = wire::integer(data, 1)?.filter(|&code| code != 0) {
            return Err(format!("WeType transport code {code}").into());
        }
        let http = wire::bytes(data, 4)?.ok_or("missing WeType response")?;
        let mut headers = BTreeMap::new();
        for f in wire::fields(http)? {
            let f = f?;
            if f.number != 4 {
                continue;
            }
            let Value::Bytes(header) = f.value else {
                return Err("invalid WeType response header".into());
            };
            if headers.len() >= 48 {
                return Err("too many WeType response headers".into());
            }
            headers.insert(
                wire::text(header, 1)?
                    .ok_or("missing response header name")?
                    .into(),
                wire::text(header, 2)?
                    .ok_or("missing response header value")?
                    .into(),
            );
        }
        Ok(Self {
            id: wire::integer(data, 2)?.ok_or("missing WeType response id")?,
            status: wire::integer(http, 2)?.ok_or("missing WeType HTTP status")?,
            headers,
            body: wire::bytes(http, 5)?.unwrap_or_default().to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_matches_isolated_native_sdk_oracle_byte_for_byte() {
        // SDK 3.0.0.37 GETWsHttpData, synthetic token/salt/body only.
        let golden = include_bytes!("../../tests/fixtures/wetype-gen-http.bin");
        let http = wire::bytes(golden, 5).unwrap().unwrap();
        let mut headers = BTreeMap::new();
        for field in wire::fields(http).unwrap() {
            let field = field.unwrap();
            if let (5, Value::Bytes(value)) = (field.number, field.value) {
                headers.insert(
                    wire::text(value, 1).unwrap().unwrap().into(),
                    wire::text(value, 2).unwrap().unwrap().into(),
                );
            }
        }
        let output = request(
            wire::integer(golden, 3).unwrap().unwrap() as u32,
            wire::text(http, 3).unwrap().unwrap(),
            &headers,
            wire::bytes(http, 6).unwrap().unwrap(),
        )
        .unwrap();
        assert_eq!(output, golden.as_slice());
    }
}
