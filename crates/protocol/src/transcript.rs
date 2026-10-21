//! The service returns UTF-8 edits, including append operations after VAD commits.
use crate::wire::Error;

pub struct Edit {
    pub offset: usize,
    pub committed: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechEvent {
    pub final_result: bool,
    pub text: String,
}

#[derive(Default)]
pub struct Transcript {
    text: String,
    committed: usize,
}

impl Transcript {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn apply(&mut self, edit: &Edit) -> Result<Vec<SpeechEvent>, Error> {
        if edit.offset > self.text.len() || !self.text.is_char_boundary(edit.offset) {
            return Err(Error::InvalidEdit);
        }
        let length = edit
            .offset
            .checked_add(edit.text.len())
            .ok_or(Error::Overflow)?;
        if length > (1 << 20) || edit.committed > length {
            return Err(Error::InvalidEdit);
        }
        if edit.offset < self.committed {
            let prefix = self.committed - edit.offset;
            if edit.text.get(..prefix) != self.text.get(edit.offset..self.committed) {
                return Err(Error::InvalidEdit);
            }
        }
        // Validate the future stable boundary before changing user text.
        if edit.committed > edit.offset && !edit.text.is_char_boundary(edit.committed - edit.offset)
        {
            return Err(Error::InvalidEdit);
        }
        let old_committed = self.committed;
        let changed =
            self.text.len() != length || self.text.get(edit.offset..) != Some(edit.text.as_str());
        self.text.truncate(edit.offset);
        self.text.push_str(&edit.text);
        self.committed = edit.committed.max(old_committed);
        let mut events = Vec::with_capacity(2);
        if self.committed > old_committed {
            events.push(SpeechEvent {
                final_result: true,
                text: self.text[old_committed..self.committed].to_owned(),
            });
        }
        if changed && self.committed < self.text.len() {
            events.push(SpeechEvent {
                final_result: false,
                text: self.text[self.committed..].to_owned(),
            });
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> Option<SpeechEvent> {
        if self.committed >= self.text.len() {
            return None;
        }
        let text = self.text[self.committed..].to_owned();
        self.committed = self.text.len();
        Some(SpeechEvent {
            final_result: true,
            text,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_after_a_committed_chinese_sentence() {
        let mut state = Transcript::default();
        state
            .apply(&Edit {
                offset: 0,
                committed: 0,
                text: "你好。".into(),
            })
            .unwrap();
        let events = state
            .apply(&Edit {
                offset: 0,
                committed: 9,
                text: "你好。".into(),
            })
            .unwrap();
        assert_eq!(
            events,
            vec![SpeechEvent {
                final_result: true,
                text: "你好。".into()
            }]
        );
        state
            .apply(&Edit {
                offset: 9,
                committed: 9,
                text: "继续".into(),
            })
            .unwrap();
        state
            .apply(&Edit {
                offset: 9,
                committed: 9,
                text: "继续输入".into(),
            })
            .unwrap();
        assert_eq!(state.text(), "你好。继续输入");
        assert_eq!(state.finish().unwrap().text, "继续输入");
        assert!(state.finish().is_none());
    }

    #[test]
    fn invalid_utf8_edit_preserves_existing_text() {
        let mut state = Transcript::default();
        state
            .apply(&Edit {
                offset: 0,
                committed: 0,
                text: "测试".into(),
            })
            .unwrap();
        assert_eq!(
            state.apply(&Edit {
                offset: 1,
                committed: 0,
                text: "x".into()
            }),
            Err(Error::InvalidEdit)
        );
        assert_eq!(state.text(), "测试");
    }

    #[test]
    fn repeated_final_snapshot_does_not_commit_twice() {
        let mut state = Transcript::default();
        let edit = Edit {
            offset: 0,
            committed: 6,
            text: "测试".into(),
        };
        assert_eq!(state.apply(&edit).unwrap().len(), 1);
        assert!(state.apply(&edit).unwrap().is_empty());
        assert!(state.finish().is_none());
    }
}
