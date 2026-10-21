//! Ordered stream acknowledgement and transcript updates, usable offline too.
use crate::{asr, stream, transcript, wire::Error};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Default)]
pub struct Receiver {
    pub updates: usize,
    pub changes: usize,
    pub text: String,
    pub down_ack: u32,
    pub events: VecDeque<transcript::SpeechEvent>,
    transcript: transcript::Transcript,
    pending_downstream: BTreeSet<u32>,
    pending_text: BTreeMap<u64, transcript::Edit>,
    applied_data: u32,
}

impl Receiver {
    /// For saved captures that begin after earlier non-text protocol messages.
    pub fn starting_at(sequence: u32) -> Self {
        let ack = sequence.saturating_sub(1);
        Self {
            down_ack: ack,
            applied_data: ack,
            ..Self::default()
        }
    }

    pub fn ingest(&mut self, frame: &[u8]) -> Result<(), Error> {
        let previous_ack = self.down_ack;
        stream::visit_downstream(frame, |sequence| {
            if sequence > previous_ack {
                self.pending_downstream.insert(sequence);
            }
        })?;
        while let Some(next) = self.down_ack.checked_add(1) {
            if !self.pending_downstream.remove(&next) {
                break;
            }
            self.down_ack = next;
        }
        if self.pending_downstream.len() > 512 {
            return Err(Error::WindowExceeded);
        }
        for update in asr::decode_all(frame)? {
            if update.sequence > u64::from(self.applied_data) {
                self.pending_text
                    .entry(update.sequence)
                    .or_insert_with(|| transcript::Edit {
                        offset: update.edit_offset,
                        committed: update.committed_bytes,
                        text: update.replacement.unwrap_or(update.text).to_owned(),
                    });
            }
        }
        while self
            .pending_text
            .first_key_value()
            .is_some_and(|(&sequence, _)| sequence <= u64::from(self.down_ack))
        {
            let (_, edit) = self.pending_text.pop_first().unwrap();
            self.events.extend(self.transcript.apply(&edit)?);
            self.updates += 1;
            if self.text != self.transcript.text() {
                self.text.clear();
                self.text.push_str(self.transcript.text());
                self.changes += 1;
            }
        }
        self.applied_data = self.down_ack;
        if self.pending_text.len() > 512 || self.events.len() > 512 {
            return Err(Error::WindowExceeded);
        }
        Ok(())
    }

    pub fn finish(&mut self) {
        if let Some(event) = self.transcript.finish() {
            self.events.push_back(event);
        }
    }
}
