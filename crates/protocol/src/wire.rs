//! Borrowed protobuf fields. No allocation or generated schema is needed.
use std::fmt;

pub const MAX_FRAME: usize = 1 << 20;
const MAX_FIELDS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    TooLarge,
    Truncated,
    Overflow,
    InvalidTag,
    UnsupportedWireType,
    TooManyFields,
    WrongType,
    InvalidUtf8,
    MissingField,
    Remote(u64),
    InvalidEdit,
    WindowExceeded,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Remote(code) => write!(f, "remote code {}", *code as i64),
            _ => write!(f, "{self:?}"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed32(&'a [u8]),
    Fixed64(&'a [u8]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field<'a> {
    pub number: u32,
    pub value: Value<'a>,
}

fn varint(input: &[u8], pos: &mut usize) -> Result<u64, Error> {
    let mut value = 0;
    for index in 0..10 {
        let byte = *input.get(*pos).ok_or(Error::Truncated)?;
        *pos += 1;
        if index == 9 && byte > 1 {
            return Err(Error::Overflow);
        }
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte < 0x80 {
            return Ok(value);
        }
    }
    Err(Error::Overflow)
}

pub struct Fields<'a> {
    input: &'a [u8],
    pos: usize,
    count: usize,
    failed: bool,
}

pub fn fields(input: &[u8]) -> Result<Fields<'_>, Error> {
    if input.len() > MAX_FRAME {
        return Err(Error::TooLarge);
    }
    Ok(Fields {
        input,
        pos: 0,
        count: 0,
        failed: false,
    })
}

impl<'a> Fields<'a> {
    fn take(&mut self, size: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(size).ok_or(Error::Overflow)?;
        let result = self.input.get(self.pos..end).ok_or(Error::Truncated)?;
        self.pos = end;
        Ok(result)
    }

    fn read(&mut self) -> Result<Field<'a>, Error> {
        self.count += 1;
        if self.count > MAX_FIELDS {
            return Err(Error::TooManyFields);
        }
        let tag = varint(self.input, &mut self.pos)?;
        let number = tag >> 3;
        if number == 0 || number >= (1 << 29) {
            return Err(Error::InvalidTag);
        }
        let value = match tag & 7 {
            0 => Value::Varint(varint(self.input, &mut self.pos)?),
            1 => Value::Fixed64(self.take(8)?),
            2 => {
                let length = usize::try_from(varint(self.input, &mut self.pos)?)
                    .map_err(|_| Error::Overflow)?;
                Value::Bytes(self.take(length)?)
            }
            5 => Value::Fixed32(self.take(4)?),
            _ => return Err(Error::UnsupportedWireType),
        };
        Ok(Field {
            number: number as u32,
            value,
        })
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = Result<Field<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.pos == self.input.len() {
            return None;
        }
        let value = self.read();
        self.failed = value.is_err();
        Some(value)
    }
}

pub fn find(input: &[u8], number: u32) -> Result<Option<Value<'_>>, Error> {
    let mut result = None;
    for field in fields(input)? {
        let field = field?;
        if field.number == number {
            result = Some(field.value);
        }
    }
    Ok(result)
}

pub fn bytes(input: &[u8], number: u32) -> Result<Option<&[u8]>, Error> {
    match find(input, number)? {
        None => Ok(None),
        Some(Value::Bytes(value)) => Ok(Some(value)),
        _ => Err(Error::WrongType),
    }
}

pub fn integer(input: &[u8], number: u32) -> Result<Option<u64>, Error> {
    match find(input, number)? {
        None => Ok(None),
        Some(Value::Varint(value)) => Ok(Some(value)),
        _ => Err(Error::WrongType),
    }
}

pub fn text(input: &[u8], number: u32) -> Result<Option<&str>, Error> {
    bytes(input, number)?
        .map(|s| std::str::from_utf8(s).map_err(|_| Error::InvalidUtf8))
        .transpose()
}

pub fn put_varint(output: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        output.push((value as u8) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

pub fn put_integer(output: &mut Vec<u8>, field: u32, value: u64) {
    put_varint(output, u64::from(field) << 3);
    put_varint(output, value);
}

pub fn put_bytes(output: &mut Vec<u8>, field: u32, value: &[u8]) {
    put_varint(output, (u64::from(field) << 3) | 2);
    put_varint(output, value.len() as u64);
    output.extend_from_slice(value);
}

pub fn replace_bytes(input: &[u8], number: u32, replacement: &[u8]) -> Result<Vec<u8>, Error> {
    let mut output = Vec::with_capacity(input.len() + replacement.len());
    let mut found = false;
    for field in fields(input)? {
        let field = field?;
        if field.number == number {
            if !matches!(field.value, Value::Bytes(_)) {
                return Err(Error::WrongType);
            }
            put_bytes(&mut output, number, replacement);
            found = true;
            continue;
        }
        match field.value {
            Value::Varint(value) => put_integer(&mut output, field.number, value),
            Value::Bytes(value) => put_bytes(&mut output, field.number, value),
            Value::Fixed32(value) => {
                put_varint(&mut output, (u64::from(field.number) << 3) | 5);
                output.extend_from_slice(value);
            }
            Value::Fixed64(value) => {
                put_varint(&mut output, (u64::from(field.number) << 3) | 1);
                output.extend_from_slice(value);
            }
        }
    }
    if !found {
        return Err(Error::MissingField);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_varint_and_unknown_fixed_field() {
        let input = [0x08, 0x96, 0x01, 0x15, 1, 2, 3, 4, 0x1a, 2, b'o', b'k'];
        assert_eq!(integer(&input, 1), Ok(Some(150)));
        assert_eq!(text(&input, 3), Ok(Some("ok")));
    }

    #[test]
    fn rejects_truncation_and_overflow() {
        assert_eq!(bytes(&[0x0a, 0x05, 1], 1), Err(Error::Truncated));
        assert_eq!(
            integer(
                &[
                    0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 2
                ],
                1
            ),
            Err(Error::Overflow)
        );
        assert_eq!(integer(&[0], 1), Err(Error::InvalidTag));
    }

    #[test]
    fn validates_the_tail_after_a_matching_field() {
        assert_eq!(integer(&[0x08, 1, 0x12, 20], 1), Err(Error::Truncated));
    }
}
