//! Newline-delimited UTF-8 JSON over inherited anonymous pipes. No named pipe or socket.
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead};

pub const MAX_EVENT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum BridgeRequest {
    Start {
        reco: u64,
        generation: u64,
        language: String,
        device: String,
    },
    Stop {
        reco: u64,
        generation: u64,
        language: String,
        device: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BridgeError {
    pub message: String,
    pub code: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BridgeEvent {
    Started {
        reco: u64,
        generation: u64,
        session: String,
    },
    Partial {
        reco: u64,
        generation: u64,
        text: String,
        offset: u64,
        duration: u64,
    },
    SpeechActivity {
        reco: u64,
        generation: u64,
        speaking: bool,
        offset: u64,
    },
    Final {
        reco: u64,
        generation: u64,
        text: String,
        offset: u64,
        duration: u64,
    },
    Stopped {
        reco: u64,
        generation: u64,
    },
    Error {
        reco: u64,
        generation: u64,
        error: BridgeError,
    },
}

impl BridgeEvent {
    pub fn identity(&self) -> (u64, u64) {
        match self {
            Self::Started {
                reco, generation, ..
            }
            | Self::Partial {
                reco, generation, ..
            }
            | Self::SpeechActivity {
                reco, generation, ..
            }
            | Self::Final {
                reco, generation, ..
            }
            | Self::Stopped { reco, generation }
            | Self::Error {
                reco, generation, ..
            } => (*reco, *generation),
        }
    }
}

/// The cap applies before appending bytes, including when a peer omits the newline.
pub(crate) fn read_event(reader: &mut impl BufRead) -> io::Result<Option<BridgeEvent>> {
    let mut line = Vec::new();
    loop {
        let input = reader.fill_buf()?;
        if input.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete bridge event",
                ))
            };
        }
        let n = input
            .iter()
            .position(|v| *v == b'\n')
            .map_or(input.len(), |i| i + 1);
        if line.len() + n > MAX_EVENT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bridge event too large",
            ));
        }
        let ended = input[n - 1] == b'\n';
        line.extend_from_slice(&input[..n]);
        reader.consume(n);
        if ended {
            return serde_json::from_slice(&line)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};
    #[test]
    fn broker_wire_contract_and_bounded_frames() {
        let mut r = BufReader::with_capacity(3, Cursor::new(
            b"{\"event\":\"final\",\"reco\":9,\"generation\":4,\"text\":\"hello\",\"offset\":1,\"duration\":2}\n"));
        assert!(matches!(
            read_event(&mut r).unwrap(),
            Some(BridgeEvent::Final { generation: 4, .. })
        ));
        assert!(read_event(&mut r).unwrap().is_none());
        assert!(read_event(&mut Cursor::new(vec![b'x'; MAX_EVENT_BYTES + 1])).is_err());
        assert!(read_event(&mut Cursor::new(b"{}".as_slice())).is_err());
    }
}
