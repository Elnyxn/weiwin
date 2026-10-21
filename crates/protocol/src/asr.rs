//! Layout observed at the current client's WebSocket receive boundary.
use crate::wire::{self, Error, Value};

#[derive(Debug, Clone, Copy)]
pub struct Update<'a> {
    pub sequence: u64,
    pub text: &'a str,
    pub replacement: Option<&'a str>,
    pub edit_offset: usize,
    pub committed_bytes: usize,
    pub start_ms: u64,
    pub end_ms: u64,
    // Preserve observed flags until their lifecycle meaning is confirmed.
    pub flags: [u64; 3],
}

pub fn decode(frame: &[u8]) -> Result<Option<Update<'_>>, Error> {
    Ok(decode_all(frame)?
        .into_iter()
        .max_by_key(|value| value.sequence))
}

pub fn decode_all(frame: &[u8]) -> Result<Vec<Update<'_>>, Error> {
    if let Some(code) = wire::integer(frame, 1)? {
        if code != 0 {
            return Err(Error::Remote(code));
        }
    }
    let http = wire::bytes(frame, 4)?.ok_or(Error::MissingField)?;
    let status = wire::integer(http, 2)?.ok_or(Error::MissingField)?;
    if status != 200 {
        return Err(Error::Remote(status));
    }
    let body = wire::bytes(http, 5)?.ok_or(Error::MissingField)?;
    if let Some(common) = wire::bytes(body, 1)? {
        let code = wire::integer(common, 1)?.unwrap_or(0);
        if code != 0 {
            return Err(Error::Remote(code));
        }
    }
    let mut updates = Vec::new();
    for field in wire::fields(body)? {
        let field = field?;
        if field.number != 3 {
            continue;
        }
        match field.value {
            Value::Bytes(fragment) => {
                if let Some(update) = decode_fragment(fragment)? {
                    updates.push(update);
                }
            }
            Value::Varint(_) => (),
            _ => return Err(Error::WrongType),
        }
    }
    Ok(updates)
}

fn decode_fragment(fragment: &[u8]) -> Result<Option<Update<'_>>, Error> {
    let result = match wire::bytes(fragment, 2)? {
        Some(value) => value,
        None => return Ok(None),
    };
    let Some(text) = wire::text(result, 3)? else {
        return Ok(None);
    };
    let edit = wire::bytes(result, 100)?;
    let replacement = edit.map(|p| wire::text(p, 2)).transpose()?.flatten();
    Ok(Some(Update {
        sequence: wire::integer(fragment, 1)?.unwrap_or(0),
        text,
        replacement,
        edit_offset: edit
            .map(|e| wire::integer(e, 4))
            .transpose()?
            .flatten()
            .unwrap_or(0) as usize,
        committed_bytes: edit
            .map(|e| wire::integer(e, 1))
            .transpose()?
            .flatten()
            .unwrap_or(0) as usize,
        start_ms: wire::integer(result, 5)?.unwrap_or(0),
        end_ms: wire::integer(result, 6)?.unwrap_or(0),
        flags: [
            wire::integer(result, 4)?.unwrap_or(0),
            wire::integer(result, 7)?.unwrap_or(0),
            wire::integer(result, 10)?.unwrap_or(0),
        ],
    }))
}
