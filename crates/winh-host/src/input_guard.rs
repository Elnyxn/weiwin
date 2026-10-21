//! Correlate the two mouse input streams without granting a blanket drag exemption.
#[derive(Clone, Copy, Debug)]
pub enum InputEnd {
    ManualInput,
    InputChanged,
    FocusChanged,
    Modifier,
}
impl std::fmt::Display for InputEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ManualInput => "检测到手动输入，听写已停止",
            Self::InputChanged => "输入状态已变化，听写已停止",
            Self::FocusChanged => "输入焦点已改变，听写已停止",
            Self::Modifier => "组合键按下，听写已停止",
        })
    }
}
impl std::error::Error for InputEnd {}
pub fn expected_end(error: &(dyn std::error::Error + 'static)) -> bool {
    error.is::<InputEnd>()
}

pub struct MousePermits {
    entries: [Option<(u32, u16)>; 32],
    next: usize,
}
impl MousePermits {
    pub const fn new() -> Self {
        Self {
            entries: [None; 32],
            next: 0,
        }
    }
    pub fn record(&mut self, time: u32, buttons: u16) {
        self.entries[self.next] = Some((time, buttons));
        self.next = (self.next + 1) % self.entries.len();
    }
    pub fn consume(&mut self, time: u32, buttons: u16) -> bool {
        let Some(index) = self.entries.iter().position(|entry| {
            entry.is_some_and(|(stamp, kind)| kind == buttons && time.wrapping_sub(stamp) <= 8)
        }) else {
            return false;
        };
        self.entries[index] = None;
        true
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_interruption_is_normal_but_same_message_transport_errors_are_not() {
        fn through_callback() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Err(InputEnd::ManualInput.into())
        }
        let error = through_callback().unwrap_err();
        assert!(expected_end(error.as_ref()));
        let error = std::io::Error::other(InputEnd::ManualInput.to_string());
        assert!(!expected_end(&error));
        assert!(!expected_end(&std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "network timeout"
        )));
    }
    #[test]
    fn raw_exemption_requires_a_recent_confirmed_native_event_with_the_same_button() {
        let mut permits = MousePermits::new();
        permits.record(100, 1);
        assert!(!permits.consume(99, 1));
        assert!(!permits.consume(100, 4));
        assert!(!permits.consume(109, 1));
        assert!(permits.consume(102, 1));
        assert!(!permits.consume(102, 1));
        permits.record(u32::MAX - 2, 1);
        assert!(permits.consume(2, 1));
    }
}
