//! WeType cmd4548 business payloads, inside the decrypted HTTP body.
//!
//! Both directions wrap their message in field 1. This module does not handle
//! transport, authentication or AES. Audio is an opaque OPUS_RAW_V1 container.
//!
//! Observed responses are full, revisable hypotheses: raw text can shrink, and
//! polishing can replace it even after audio has ended. Field 5 becomes 1 only
//! on the final snapshots, which can be repeated. Emit changed raw snapshots
//! immediately; do not append them or commit them merely because field 3 moves.
//! A caller chooses raw/polished text and deduplicates repeated final snapshots.
use crate::wire::{self, Error, Value};
use std::fmt;

pub const COMMAND: u32 = 4548;
pub const PROFILE: u64 = 5;
pub const TARGET_INTERVAL: u64 = 250;
pub const SESSION_KEY_BYTES: usize = 24;
const MAX_TARGET_BYTES: usize = 260;
const MAX_AUDIO_BYTES: usize = wire::MAX_FRAME - 512;

/// One upload, initial end marker, or subsequent end-poll payload.
///
/// Audio uploads have `end=false`, nonempty audio and sequence 1, 2, ... . The
/// first end marker has `end=true`, no audio and the next sequence. Later end
/// polls omit the sequence. Total bytes includes the first container header and
/// remains unchanged for every end marker/poll.
pub struct Request<'a> {
    pub session_key: &'a str,
    pub audio: Option<&'a [u8]>,
    pub chunk_sequence: Option<u32>,
    pub end: bool,
    pub total_audio_bytes: u64,
    pub target_process: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    Invalid(&'static str),
    TooLarge,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(reason) => write!(f, "invalid WeType request: {reason}"),
            Self::TooLarge => f.write_str("WeType request exceeds frame limit"),
        }
    }
}

impl std::error::Error for EncodeError {}

fn valid_session_key(key: &str) -> bool {
    key.len() == SESSION_KEY_BYTES && key.bytes().all(|byte| byte.is_ascii_digit())
}

pub fn encode_request(value: &Request<'_>) -> Result<Vec<u8>, EncodeError> {
    if !valid_session_key(value.session_key) {
        return Err(EncodeError::Invalid("session key must be 24 ASCII digits"));
    }
    if value.target_process.is_empty()
        || value.target_process.len() > MAX_TARGET_BYTES
        || value
            .target_process
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
    {
        return Err(EncodeError::Invalid("target must be a process basename"));
    }
    match (value.audio, value.end, value.chunk_sequence) {
        (Some(audio), false, Some(sequence)) if !audio.is_empty() && sequence != 0 => {
            if audio.len() > MAX_AUDIO_BYTES {
                return Err(EncodeError::TooLarge);
            }
            if value.total_audio_bytes < audio.len() as u64 {
                return Err(EncodeError::Invalid("total bytes precedes this chunk"));
            }
        }
        (None, true, None | Some(1..)) => (),
        _ => return Err(EncodeError::Invalid("audio/end/sequence combination")),
    }
    let mut body = Vec::with_capacity(value.audio.map_or(0, <[u8]>::len) + 128);
    wire::put_bytes(&mut body, 2, value.session_key.as_bytes());
    if let Some(audio) = value.audio {
        wire::put_bytes(&mut body, 4, audio);
    }
    wire::put_integer(&mut body, 5, PROFILE);
    if value.end {
        wire::put_integer(&mut body, 6, 1);
    }
    if let Some(sequence) = value.chunk_sequence {
        wire::put_integer(&mut body, 7, sequence.into());
    }
    wire::put_integer(&mut body, 11, value.total_audio_bytes);
    wire::put_integer(&mut body, 24, 1);
    let mut target = Vec::with_capacity(value.target_process.len() + 8);
    wire::put_bytes(&mut target, 1, value.target_process.as_bytes());
    wire::put_integer(&mut target, 2, TARGET_INTERVAL);
    wire::put_bytes(&mut body, 25, &target);
    let mut output = Vec::with_capacity(body.len() + 4);
    wire::put_bytes(&mut output, 1, &body);
    if output.len() > wire::MAX_FRAME {
        return Err(EncodeError::TooLarge);
    }
    Ok(output)
}

/// Numeric names intentionally avoid assigning unsupported meanings to flags.
///
/// In the capture field 11 is zero before final, then equals raw UTF-8 bytes;
/// field 17 behaves the same for polished text. These are not safe early-commit
/// instructions. Fields 15/16 become 1 when field 14 appears. Field 18 advances
/// through the audio stream and freezes while trailing responses still revise
/// text; it is not the current raw/polished string length.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResponseFlags {
    pub field_5: Option<u64>,
    pub field_6: Option<u64>,
    pub field_11: Option<u64>,
    pub field_12: Option<u64>,
    pub field_13: Option<u64>,
    pub field_15: Option<u64>,
    pub field_16: Option<u64>,
    pub field_17: Option<u64>,
    pub field_18: Option<u64>,
    pub field_100: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response<'a> {
    /// Compare to the active request's session key before using the snapshot.
    pub session_key: &'a str,
    /// Opaque field 3: changing it alone does not signify changed text.
    pub progress: Option<u64>,
    pub raw_text: &'a str,
    /// `Some("")` remains distinct from absent polishing.
    pub polished_text: Option<&'a str>,
    pub flags: ResponseFlags,
}

impl Response<'_> {
    pub fn is_final(&self) -> bool {
        self.flags.field_5 == Some(1)
    }
}

fn assign<T>(slot: &mut Option<T>, value: T) -> Result<(), Error> {
    if slot.is_some() {
        return Err(Error::InvalidTag);
    }
    *slot = Some(value);
    Ok(())
}

fn bytes(value: Value<'_>) -> Result<&[u8], Error> {
    match value {
        Value::Bytes(value) => Ok(value),
        _ => Err(Error::WrongType),
    }
}

fn text(value: Value<'_>) -> Result<&str, Error> {
    std::str::from_utf8(bytes(value)?).map_err(|_| Error::InvalidUtf8)
}

fn integer(value: Value<'_>) -> Result<u64, Error> {
    match value {
        Value::Varint(value) => Ok(value),
        _ => Err(Error::WrongType),
    }
}

/// Decode a decrypted ASR result body; never interpret a snapshot as an append.
pub fn decode_response(input: &[u8]) -> Result<Response<'_>, Error> {
    let mut body = None;
    for field in wire::fields(input)? {
        let field = field?;
        if field.number == 1 {
            assign(&mut body, bytes(field.value)?)?;
        }
    }
    let body = body.ok_or(Error::MissingField)?;
    let (mut common, mut session, mut progress, mut raw, mut polished) =
        (None, None, None, None, None);
    let mut flags = ResponseFlags::default();
    for field in wire::fields(body)? {
        let field = field?;
        match field.number {
            1 => assign(&mut common, bytes(field.value)?)?,
            2 => assign(&mut session, text(field.value)?)?,
            3 => assign(&mut progress, integer(field.value)?)?,
            4 => assign(&mut raw, text(field.value)?)?,
            14 => assign(&mut polished, text(field.value)?)?,
            number => {
                let slot = match number {
                    5 => &mut flags.field_5,
                    6 => &mut flags.field_6,
                    11 => &mut flags.field_11,
                    12 => &mut flags.field_12,
                    13 => &mut flags.field_13,
                    15 => &mut flags.field_15,
                    16 => &mut flags.field_16,
                    17 => &mut flags.field_17,
                    18 => &mut flags.field_18,
                    100 => &mut flags.field_100,
                    _ => continue,
                };
                assign(slot, integer(field.value)?)?;
            }
        }
    }
    let common = common.ok_or(Error::MissingField)?;
    let mut code = None;
    for field in wire::fields(common)? {
        let field = field?;
        if field.number == 1 {
            assign(&mut code, integer(field.value)?)?;
        }
    }
    let code = code.ok_or(Error::MissingField)?;
    if code != 0 {
        return Err(Error::Remote(code));
    }
    let session_key = session.ok_or(Error::MissingField)?;
    if !valid_session_key(session_key) {
        return Err(Error::WrongType);
    }
    Ok(Response {
        session_key,
        progress,
        raw_text: raw.ok_or(Error::MissingField)?,
        polished_text: polished,
        flags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const KEY: &str = "123456789012345678901234";

    fn response(raw: &[u8], polished: Option<&[u8]>, final_flag: u64) -> Vec<u8> {
        let mut common = Vec::new();
        wire::put_integer(&mut common, 1, 0);
        let mut body = Vec::new();
        wire::put_bytes(&mut body, 1, &common);
        wire::put_bytes(&mut body, 2, KEY.as_bytes());
        wire::put_integer(&mut body, 3, 100100);
        wire::put_bytes(&mut body, 4, raw);
        wire::put_integer(&mut body, 5, final_flag);
        wire::put_integer(&mut body, 6, 100);
        wire::put_integer(
            &mut body,
            11,
            if final_flag == 1 { raw.len() as u64 } else { 0 },
        );
        wire::put_integer(&mut body, 12, 0);
        wire::put_integer(&mut body, 13, 1);
        if let Some(polished) = polished {
            wire::put_bytes(&mut body, 14, polished);
        }
        wire::put_integer(&mut body, 15, polished.is_some().into());
        wire::put_integer(&mut body, 16, polished.is_some().into());
        wire::put_integer(
            &mut body,
            17,
            if final_flag == 1 {
                polished.map_or(0, <[u8]>::len) as u64
            } else {
                0
            },
        );
        wire::put_integer(&mut body, 18, 55);
        wire::put_integer(&mut body, 100, 3500);
        let mut outer = Vec::new();
        wire::put_bytes(&mut outer, 1, &body);
        outer
    }

    #[test]
    fn upload_and_two_end_shapes_preserve_cumulative_bytes() {
        let audio = b"#!OPUS_RAW_V1\x02\x01\x00\x03\x00abc";
        let mut request = Request {
            session_key: KEY,
            audio: Some(audio),
            chunk_sequence: Some(1),
            end: false,
            total_audio_bytes: audio.len() as u64,
            target_process: "sample.exe",
        };
        let encoded = encode_request(&request).unwrap();
        let body = wire::bytes(&encoded, 1).unwrap().unwrap();
        assert_eq!(wire::bytes(body, 4).unwrap(), Some(audio.as_slice()));
        assert_eq!(wire::integer(body, 7).unwrap(), Some(1));
        assert_eq!(wire::integer(body, 6).unwrap(), None);
        assert_eq!(wire::integer(body, 5).unwrap(), Some(5));
        assert_eq!(wire::integer(body, 24).unwrap(), Some(1));
        let target = wire::bytes(body, 25).unwrap().unwrap();
        assert_eq!(wire::text(target, 1).unwrap(), Some("sample.exe"));
        assert_eq!(wire::integer(target, 2).unwrap(), Some(250));
        request.audio = None;
        request.end = true;
        request.chunk_sequence = Some(2);
        let encoded = encode_request(&request).unwrap();
        let body = wire::bytes(&encoded, 1).unwrap().unwrap();
        assert_eq!(wire::bytes(body, 4).unwrap(), None);
        assert_eq!(wire::integer(body, 6).unwrap(), Some(1));
        assert_eq!(wire::integer(body, 7).unwrap(), Some(2));
        assert_eq!(wire::integer(body, 11).unwrap(), Some(audio.len() as u64));
        request.chunk_sequence = None;
        let encoded = encode_request(&request).unwrap();
        let body = wire::bytes(&encoded, 1).unwrap().unwrap();
        assert_eq!(wire::integer(body, 7).unwrap(), None);
        assert_eq!(wire::integer(body, 11).unwrap(), Some(audio.len() as u64));
    }

    #[test]
    fn raw_revisions_are_available_before_final_and_polish_can_replace_them() {
        let first = response(b"test", None, 0);
        let revised = response(b"te", None, 0);
        let polishing = response(b"test draft", Some(b"Test draft."), 0);
        let final_frame = response(b"test draft", Some(b"Test."), 1);
        let a = decode_response(&first).unwrap();
        let b = decode_response(&revised).unwrap();
        assert_eq!(a.raw_text, "test");
        assert_eq!(b.raw_text, "te");
        assert!(!a.is_final() && !b.is_final());
        assert_eq!(b.flags.field_11, Some(0));
        let update = decode_response(&polishing).unwrap();
        assert_eq!(update.polished_text, Some("Test draft."));
        assert!(!update.is_final());
        let final_result = decode_response(&final_frame).unwrap();
        assert_eq!(final_result.raw_text, "test draft");
        assert_eq!(final_result.polished_text, Some("Test."));
        assert!(final_result.is_final());
        assert_eq!(final_result, decode_response(&final_frame).unwrap());
    }

    #[test]
    fn fields_are_utf8_snapshots_and_absent_polish_differs_from_empty() {
        let frame = response("测试一二".as_bytes(), Some("测试12。".as_bytes()), 1);
        let result = decode_response(&frame).unwrap();
        assert_eq!(result.raw_text, "测试一二");
        assert_eq!(result.polished_text, Some("测试12。"));
        assert_eq!(result.flags.field_11, Some(12));
        assert_eq!(result.flags.field_17, Some(11));
        let frame = response(b"draft", Some(b""), 1);
        assert_eq!(decode_response(&frame).unwrap().polished_text, Some(""));
    }

    #[test]
    fn rejects_invalid_types_utf8_status_duplicates_and_bad_tails() {
        let frame = response(&[0xff], None, 0);
        assert_eq!(decode_response(&frame), Err(Error::InvalidUtf8));
        let frame = response(b"safe", None, 0);
        let body = wire::bytes(&frame, 1).unwrap().unwrap();
        let mut duplicate = body.to_vec();
        wire::put_bytes(&mut duplicate, 4, b"other");
        let mut outer = Vec::new();
        wire::put_bytes(&mut outer, 1, &duplicate);
        assert_eq!(decode_response(&outer), Err(Error::InvalidTag));
        let mut common = Vec::new();
        wire::put_integer(&mut common, 1, 17);
        let changed = wire::replace_bytes(body, 1, &common).unwrap();
        let mut outer = Vec::new();
        wire::put_bytes(&mut outer, 1, &changed);
        assert_eq!(decode_response(&outer), Err(Error::Remote(17)));
        let mut outer = frame.clone();
        outer.extend_from_slice(&[0x12, 0x7f]);
        assert_eq!(decode_response(&outer), Err(Error::Truncated));
        let mut wrong = Vec::new();
        wire::put_integer(&mut wrong, 1, 1);
        assert_eq!(decode_response(&wrong), Err(Error::WrongType));
        assert_eq!(
            decode_response(&vec![0; wire::MAX_FRAME + 1]),
            Err(Error::TooLarge)
        );
    }

    #[test]
    fn request_validation_rejects_ambiguous_shapes_and_paths_without_encoding_secrets() {
        let mut request = Request {
            session_key: KEY,
            audio: Some(b"audio"),
            chunk_sequence: Some(1),
            end: false,
            total_audio_bytes: 5,
            target_process: "sample.exe",
        };
        request.end = true;
        assert!(encode_request(&request).is_err());
        request.end = false;
        request.chunk_sequence = Some(0);
        assert!(encode_request(&request).is_err());
        request.chunk_sequence = Some(1);
        request.total_audio_bytes = 1;
        assert!(encode_request(&request).is_err());
        request.total_audio_bytes = 5;
        request.target_process = "C:\\sample.exe";
        assert!(encode_request(&request).is_err());
        request.target_process = "sample.exe";
        request.session_key = "PRIVATE_INVALID_SESSION";
        let error = encode_request(&request).unwrap_err().to_string();
        assert!(!error.contains("PRIVATE_INVALID_SESSION"));
        assert!(!error.contains("audio"));
    }
}
