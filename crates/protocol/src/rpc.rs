//! WebSocket protobuf envelope, recovered from the SDK's actual writer.
use crate::wire::{self, Error};

pub const INIT_PATH: &str = "/ilink/base/web_init_stream";
pub const SYNC_PATH: &str = "/ilink/base/web_sync_stream";

pub struct Identity<'a> {
    pub cloud_uin: u64,
    pub wss_token: &'a str,
    pub auth_type: &'a str,
    pub host: &'a str,
    pub app_id: Option<&'a str>,
    pub client_version: Option<u32>,
    pub server_group_key: Option<u32>,
}

fn header(http: &mut Vec<u8>, name: &str, value: &str) {
    let mut pair = Vec::with_capacity(name.len() + value.len() + 8);
    wire::put_bytes(&mut pair, 1, name.as_bytes());
    wire::put_bytes(&mut pair, 2, value.as_bytes());
    wire::put_bytes(http, 5, &pair);
}

pub fn request(
    sequence: u32,
    identity: &Identity<'_>,
    path: &str,
    body: &[u8],
) -> Result<Vec<u8>, Error> {
    if body.len() > wire::MAX_FRAME - 4096 || identity.wss_token.len() > 2048 {
        return Err(Error::TooLarge);
    }
    let mut http = Vec::with_capacity(body.len() + 512);
    wire::put_bytes(&mut http, 1, b"POST");
    wire::put_bytes(&mut http, 3, path.as_bytes());
    header(&mut http, "Host", identity.host);
    header(
        &mut http,
        "Authorization",
        &format!("Bearer {}", identity.wss_token),
    );
    header(&mut http, "AuthorizationType", identity.auth_type);
    header(&mut http, "Content-Type", "application/x-protobuf");
    header(&mut http, "Content-Length", &body.len().to_string());
    if let Some(version) = identity.client_version {
        header(&mut http, "iLink-App-ClientVersion", &version.to_string());
    }
    if let Some(group) = identity.server_group_key {
        header(&mut http, "Server_Group_Key", &group.to_string());
    }
    if let Some(app) = identity.app_id {
        header(&mut http, "iLink-App-Id", app);
    }
    wire::put_bytes(&mut http, 6, body);
    let mut envelope = Vec::with_capacity(http.len() + 32);
    wire::put_integer(&mut envelope, 2, 1901);
    wire::put_integer(&mut envelope, 3, u64::from(sequence));
    wire::put_bytes(&mut envelope, 5, &http);
    wire::put_integer(&mut envelope, 6, identity.cloud_uin);
    Ok(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_original_sdk_serialization() {
        // Generated with dummy inputs by the hash-gated standalone SDK oracle.
        let identity = Identity {
            cloud_uin: 0x1122334455667788,
            wss_token: "fixture-token",
            auth_type: "1",
            host: "szilinkshort.weixin.com",
            app_id: None,
            client_version: None,
            server_group_key: None,
        };
        let bytes = request(17, &identity, INIT_PATH, b"\x08\x01\x12\x04test").unwrap();
        assert_eq!(
            bytes.as_slice(),
            include_bytes!("../tests/data/oracle-root-http.bin")
        );
    }
}
