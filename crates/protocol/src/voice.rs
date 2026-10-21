//! ASR business messages observed in desktop voice input (scene 36, profile 5).
use crate::wire;

pub struct Profile {
    pub client_version: u32,
    pub key: String,
}

impl Profile {
    fn common(&self, out: &mut Vec<u8>) {
        // Native int32 is sign extended to a protobuf varint, not zig-zag encoded.
        wire::put_integer(out, 1, self.client_version as i32 as i64 as u64);
        wire::put_bytes(out, 2, b"UnifiedPCWindows 11 x86_64");
        wire::put_bytes(out, 3, self.key.as_bytes());
        wire::put_integer(out, 4, 36);
    }

    pub fn context(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        self.common(&mut out);
        wire::put_integer(&mut out, 6, 5);
        wire::put_integer(&mut out, 8, 0);
        wire::put_bytes(&mut out, 9, &[0x10, 0]);
        out
    }

    pub fn audio(&self, sequence: u32, opus: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(opus.len() + 128);
        self.common(&mut out);
        wire::put_bytes(&mut out, 5, opus);
        wire::put_integer(&mut out, 6, 5);
        wire::put_integer(&mut out, 7, 0);
        wire::put_integer(&mut out, 8, u64::from(sequence));
        wire::put_bytes(&mut out, 9, &[8, 0, 0x10, 0]);
        out
    }
}
