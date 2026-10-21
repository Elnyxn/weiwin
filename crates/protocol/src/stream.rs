//! Stream service messages located in the native InitStreamReq writer.
use crate::wire::{self, Error, Value};

pub const VOICE_INPUT_BUSINESS: u32 = 3;

pub fn init(device_id: &[u8], stream_id: &str, business: u32) -> Vec<u8> {
    let mut message = Vec::with_capacity(device_id.len() + stream_id.len() + 16);
    wire::put_bytes(&mut message, 1, device_id);
    wire::put_bytes(&mut message, 2, stream_id.as_bytes());
    wire::put_integer(&mut message, 3, u64::from(business));
    message
}

pub struct Sync<'a> {
    pub device_id: &'a [u8],
    pub stream_id: &'a str,
    pub business: u32,
    pub piece_sequence: u32,
    pub piece: Option<&'a [u8]>,
    pub down_ack: u32,
    pub context: &'a [u8],
}

pub fn sync(value: &Sync<'_>) -> Vec<u8> {
    let mut message =
        Vec::with_capacity(value.piece.map_or(0, <[u8]>::len) + value.context.len() + 128);
    wire::put_bytes(&mut message, 1, value.device_id);
    wire::put_bytes(&mut message, 2, value.stream_id.as_bytes());
    wire::put_integer(&mut message, 3, u64::from(value.business));
    if let Some(data) = value.piece {
        let mut piece = Vec::with_capacity(data.len() + 16);
        wire::put_integer(&mut piece, 1, u64::from(value.piece_sequence));
        wire::put_bytes(&mut piece, 2, data);
        wire::put_bytes(&mut message, 4, &piece);
    }
    wire::put_integer(&mut message, 7, u64::from(value.down_ack));
    wire::put_bytes(&mut message, 9, value.context);
    message
}

pub fn visit_downstream(frame: &[u8], mut visitor: impl FnMut(u32)) -> Result<(), Error> {
    let Some(http) = wire::bytes(frame, 4)? else {
        return Ok(());
    };
    let Some(body) = wire::bytes(http, 5)? else {
        return Ok(());
    };
    for field in wire::fields(body)? {
        let field = field?;
        if field.number == 3 {
            if let Value::Bytes(piece) = field.value {
                if let Some(sequence) = wire::integer(piece, 1)? {
                    let sequence = u32::try_from(sequence).map_err(|_| Error::Overflow)?;
                    visitor(sequence);
                }
            }
        }
    }
    Ok(())
}
