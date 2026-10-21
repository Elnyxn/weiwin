//! Local, sample-clocked turn detection. No audio leaves this module while idle.
use crate::encoder::{FRAME_SAMPLES, SAMPLE_RATE};
use std::collections::VecDeque;
use webrtc_vad::{SampleRate, Vad, VadMode};

pub const SILENCE_SAMPLES: usize = SAMPLE_RATE as usize * 3;
pub const PREROLL_SAMPLES: usize = SAMPLE_RATE as usize * 360 / 1000;
const ATTACK_FRAMES: usize = 3; // 60 ms; one click must not open a connection.

#[derive(Debug, PartialEq, Eq)]
pub enum ActivityEvent {
    /// Includes the onset and up to 360 ms of preceding audio.
    Started(Vec<i16>),
    Audio(Vec<i16>),
    /// The preceding Audio includes the configured amount of detected silence.
    Ended,
}

pub struct Activity {
    vad: Vad,
    pending: Vec<i16>,
    preroll: VecDeque<i16>,
    attack: usize,
    silent: usize,
    silence_samples: usize,
    active: bool,
    noise: f64,
}

impl Default for Activity {
    fn default() -> Self {
        Self::with_silence_seconds(3).expect("valid default silence threshold")
    }
}

impl Activity {
    pub fn with_silence_seconds(seconds: u32) -> Result<Self, &'static str> {
        if !matches!(seconds, 1 | 2 | 3 | 5 | 10) {
            return Err("unsupported silence threshold");
        }
        Ok(Self {
            vad: Vad::new_with_rate_and_mode(SampleRate::Rate16kHz, VadMode::Quality),
            pending: Vec::with_capacity(FRAME_SAMPLES),
            preroll: VecDeque::with_capacity(PREROLL_SAMPLES),
            attack: 0,
            silent: 0,
            silence_samples: SAMPLE_RATE as usize * seconds as usize,
            active: false,
            noise: 10.0,
        })
    }

    pub fn silence_samples(&self) -> usize {
        self.silence_samples
    }

    /// WASAPI packet boundaries do not affect the silence deadline.
    pub fn push(&mut self, pcm: &[i16]) -> Vec<ActivityEvent> {
        let mut events = Vec::new();
        for &sample in pcm {
            self.pending.push(sample);
            if self.pending.len() == FRAME_SAMPLES {
                let frame = std::mem::replace(&mut self.pending, Vec::with_capacity(FRAME_SAMPLES));
                // Remove DC bias when measuring energy; WebRTC supplies the actual
                // speech/noise decision, rather than treating all loud sound as voice.
                let mean = frame.iter().map(|&s| f64::from(s)).sum::<f64>() / FRAME_SAMPLES as f64;
                let rms = (frame
                    .iter()
                    .map(|&s| (f64::from(s) - mean).powi(2))
                    .sum::<f64>()
                    / FRAME_SAMPLES as f64)
                    .sqrt();
                let voice = self
                    .vad
                    .is_voice_segment(&frame)
                    .expect("fixed 16 kHz / 20 ms VAD frame")
                    && rms >= (self.noise * 2.5).max(80.0);
                if !self.active && !voice {
                    // Follow slowly changing background, without learning speech as noise.
                    self.noise = self.noise * 0.98 + rms.min(3000.0) * 0.02;
                }
                self.frame(frame, voice, &mut events);
            }
        }
        events
    }

    fn frame(&mut self, frame: Vec<i16>, voice: bool, events: &mut Vec<ActivityEvent>) {
        if self.active {
            self.silent = if voice { 0 } else { self.silent + frame.len() };
            events.push(ActivityEvent::Audio(frame));
            if self.silent >= self.silence_samples {
                self.active = false;
                self.attack = 0;
                self.silent = 0;
                events.push(ActivityEvent::Ended);
            }
        } else {
            self.preroll.extend(frame);
            if self.preroll.len() > PREROLL_SAMPLES {
                self.preroll.drain(..self.preroll.len() - PREROLL_SAMPLES);
            }
            self.attack = if voice { self.attack + 1 } else { 0 };
            if self.attack >= ATTACK_FRAMES {
                self.active = true;
                self.silent = 0;
                events.push(ActivityEvent::Started(self.preroll.drain(..).collect()));
            }
        }
    }

    /// Explicit stop / finite WAV EOF flushes a partial PCM frame and the turn.
    pub fn finish(&mut self) -> Vec<ActivityEvent> {
        let mut events = Vec::new();
        if self.active {
            if !self.pending.is_empty() {
                events.push(ActivityEvent::Audio(std::mem::take(&mut self.pending)));
            }
            self.active = false;
            events.push(ActivityEvent::Ended);
        }
        self.pending.clear();
        self.preroll.clear();
        self.attack = 0;
        self.silent = 0;
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voice(samples: usize) -> Vec<i16> {
        // A voiced harmonic excitation with slowly varying pitch and amplitude.
        let mut phase = 0.0_f64;
        (0..samples)
            .map(|i| {
                phase += std::f64::consts::TAU * (145.0 + 25.0 * (i as f64 / 1400.0).sin())
                    / f64::from(SAMPLE_RATE);
                ((phase.sin() + 0.4 * (phase * 3.0).sin() + 0.2 * (phase * 7.0).sin())
                    * (2400.0 + 900.0 * (i as f64 / 530.0).sin())) as i16
            })
            .collect()
    }

    #[test]
    fn quiet_noise_and_single_click_do_not_activate() {
        let mut gate = Activity::default();
        let mut noise = vec![0; SAMPLE_RATE as usize * 5];
        for (i, s) in noise.iter_mut().enumerate() {
            *s = (i % 5) as i16 - 2;
        }
        noise[16_000] = 21_000;
        assert!(gate.push(&noise).is_empty());
        assert!(gate.finish().is_empty());
        assert!(gate.preroll.len() <= PREROLL_SAMPLES);
    }

    #[test]
    fn three_seconds_are_sample_counted_and_speaking_rearms() {
        let mut gate = Activity::default();
        gate.push(&vec![0; 16_000]);
        let first = gate.push(&voice(16_000));
        let starts: Vec<_> = first
            .iter()
            .filter_map(|e| match e {
                ActivityEvent::Started(pcm) => Some(pcm),
                _ => None,
            })
            .collect();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].len(), PREROLL_SAMPLES);
        // Exactly 2999.9375 ms is insufficient, independent of poll timing.
        let before = gate.push(&vec![0; SILENCE_SAMPLES - 1]);
        assert!(!before.iter().any(|e| matches!(e, ActivityEvent::Ended)));
        assert_eq!(gate.push(&[0]).last(), Some(&ActivityEvent::Ended));
        assert!(gate.push(&vec![0; 160_000]).is_empty());
        let second = gate.push(&voice(16_000));
        assert_eq!(
            second
                .iter()
                .filter(|e| matches!(e, ActivityEvent::Started(_)))
                .count(),
            1
        );
        assert_eq!(
            gate.push(&vec![0; SILENCE_SAMPLES]).last(),
            Some(&ActivityEvent::Ended)
        );
        assert!(gate.finish().is_empty());
    }

    #[test]
    fn short_pause_preserves_turn_and_eof_flushes_partial_frame() {
        let mut gate = Activity::default();
        assert!(
            gate.push(&voice(16_000))
                .iter()
                .any(|e| matches!(e, ActivityEvent::Started(_)))
        );
        assert!(
            !gate
                .push(&vec![0; 32_000])
                .iter()
                .any(|e| matches!(e, ActivityEvent::Ended))
        );
        assert!(
            !gate
                .push(&voice(16_003))
                .iter()
                .any(|e| matches!(e, ActivityEvent::Started(_)))
        );
        let tail = gate.finish();
        assert!(matches!(&tail[0], ActivityEvent::Audio(pcm) if pcm.len() == 3));
        assert_eq!(tail.last(), Some(&ActivityEvent::Ended));
        assert!(gate.finish().is_empty());
    }

    #[test]
    fn every_configured_threshold_ends_at_its_exact_pcm_sample_and_rearms() {
        for seconds in [1, 2, 3, 5, 10] {
            let mut gate = Activity::with_silence_seconds(seconds).unwrap();
            assert!(
                gate.push(&voice(16_000))
                    .iter()
                    .any(|e| matches!(e, ActivityEvent::Started(_)))
            );
            let samples = SAMPLE_RATE as usize * seconds as usize;
            assert_eq!(gate.silence_samples(), samples);
            assert!(
                !gate
                    .push(&vec![0; samples - 1])
                    .iter()
                    .any(|e| matches!(e, ActivityEvent::Ended))
            );
            assert_eq!(gate.push(&[0]).last(), Some(&ActivityEvent::Ended));
            assert!(gate.push(&vec![0; 16_000]).is_empty());
            assert!(
                gate.push(&voice(16_000))
                    .iter()
                    .any(|e| matches!(e, ActivityEvent::Started(_)))
            );
        }
        for invalid in [0, 4, 6, 11, u32::MAX] {
            assert!(Activity::with_silence_seconds(invalid).is_err());
        }
    }
}
