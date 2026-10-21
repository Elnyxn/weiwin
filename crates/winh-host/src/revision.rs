//! Revisions are confined to the current, uncommitted dictation tail.
use std::collections::VecDeque;
use unicode_segmentation::UnicodeSegmentation;

pub struct Pending {
    pub text: String,
    pub final_result: bool,
}
#[derive(Default)]
pub struct Deferred {
    items: VecDeque<Pending>,
    bytes: usize,
}
impl Deferred {
    pub fn push(&mut self, text: &str, final_result: bool) -> Result<(), &'static str> {
        if text.len() > 16_384 || text.chars().any(char::is_control) {
            return Err("终端听写文字无效");
        }
        if !final_result && self.items.back().is_some_and(|item| !item.final_result) {
            self.bytes -= self.items.pop_back().unwrap().text.len();
        }
        if self.items.len() >= 128 || self.bytes + text.len() > 1_048_576 {
            return Err("待输入文字过多，请重新听写");
        }
        self.bytes += text.len();
        self.items.push_back(Pending {
            text: text.to_owned(),
            final_result,
        });
        Ok(())
    }
    pub fn pop(&mut self) -> Option<Pending> {
        let item = self.items.pop_front()?;
        self.bytes -= item.text.len();
        Some(item)
    }
}

#[derive(Default)]
pub struct Revision {
    pending: String,
}
pub struct Update {
    pub erase: usize,
    pub insert: String,
    next: String,
}
impl Revision {
    pub fn prepare(&self, text: &str, final_result: bool) -> Result<Update, &'static str> {
        if text.len() > 16_384 {
            return Err("单句过长，请停顿收句");
        }
        if text.chars().any(char::is_control) {
            return Err("终端听写不支持控制字符");
        }
        // A stable prefix may be committed before the hypothesis tail ends.
        // Its tail is already on screen and must not be inserted a second time.
        if final_result && self.pending.starts_with(text) {
            if text.len() != self.pending.len()
                && !self
                    .pending
                    .grapheme_indices(true)
                    .any(|(offset, _)| offset == text.len())
            {
                return Err("文字边界已变化，听写已停止");
            }
            return Ok(Update {
                erase: 0,
                insert: String::new(),
                next: self.pending[text.len()..].to_owned(),
            });
        }
        let old: Vec<_> = self.pending.graphemes(true).collect();
        let new: Vec<_> = text.graphemes(true).collect();
        let common = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
        Ok(Update {
            erase: old.len() - common,
            insert: new[common..].concat(),
            next: if final_result {
                String::new()
            } else {
                text.to_owned()
            },
        })
    }
    pub fn accept(&mut self, update: Update) {
        self.pending = update.next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn apply(state: &mut Revision, document: &mut String, text: &str, final_result: bool) {
        let update = state.prepare(text, final_result).unwrap();
        let remove = document
            .grapheme_indices(true)
            .rev()
            .nth(update.erase.saturating_sub(1))
            .map(|(i, _)| i);
        if update.erase > 0 {
            document.truncate(remove.unwrap());
        }
        document.push_str(&update.insert);
        state.accept(update);
    }
    #[test]
    fn revises_chinese_number_without_touching_existing_input_or_duplicating_final() {
        let mut r = Revision::default();
        let mut doc = "原有内容> ".to_owned();
        for text in ["今天", "今天编号九八", "今天编号9876"] {
            apply(&mut r, &mut doc, text, false);
        }
        apply(&mut r, &mut doc, "今天编号9876", true);
        assert_eq!(doc, "原有内容> 今天编号9876");
        apply(&mut r, &mut doc, "第二句", false);
        apply(&mut r, &mut doc, "第二句。", true);
        assert_eq!(doc, "原有内容> 今天编号9876第二句。");
    }
    #[test]
    fn stable_prefix_retains_uncommitted_tail_and_graphemes_are_not_split() {
        let mut r = Revision::default();
        let mut doc = "prefix ".to_owned();
        apply(&mut r, &mut doc, "你好👩‍💻今天", false);
        apply(&mut r, &mut doc, "你好", true);
        apply(&mut r, &mut doc, "🧑‍💻明天", false);
        apply(&mut r, &mut doc, "🧑‍💻明天", true);
        assert_eq!(doc, "prefix 你好🧑‍💻明天");
        assert!(r.prepare("\nwhoami", false).is_err());
        assert!(r.prepare("\u{1b}[D", false).is_err());
        let mut combining = Revision::default();
        let update = combining.prepare("e\u{301}X", false).unwrap();
        combining.accept(update);
        assert!(combining.prepare("e", true).is_err());
    }
    #[test]
    fn panel_pause_coalesces_hypotheses_and_preserves_commits_across_turns() {
        let mut queue = Deferred::default();
        for (text, final_result) in [
            ("今天", false),
            ("今天编号九八", false),
            ("今天编号9876", false),
            ("今天编号", true),
            ("9876", false),
            ("9876", true),
            ("第二", false),
            ("第二句", false),
            ("第二句。", true),
        ] {
            queue.push(text, final_result).unwrap();
        }
        let mut revision = Revision::default();
        let mut doc = "原有> ".to_owned();
        while let Some(event) = queue.pop() {
            apply(&mut revision, &mut doc, &event.text, event.final_result);
        }
        assert_eq!(doc, "原有> 今天编号9876第二句。");
    }
}
