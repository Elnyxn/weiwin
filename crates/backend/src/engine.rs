//! One dictation session. The only client access is the bounded read-only account reader.
use crate::{Connection, Result, Session, check::new_id, runtime_account, settings::Settings};
#[path = "engine_wetype.rs"]
mod wetype;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use wechat_voice_audio::{
    activity::{Activity, ActivityEvent},
    encoder::{CHUNK_SAMPLES, FRAME_SAMPLES, OpusStream},
    microphone::Microphone,
};
use wechat_voice_protocol::{
    receiver::Receiver as SpeechReceiver, rpc, stream, transcript::SpeechEvent, voice::Profile,
};
pub use wetype::run as run_wetype;

pub enum Source {
    Microphone(Option<String>),
    Wave(PathBuf),
    WaveHold(PathBuf),
}
pub fn validate_wave(path: &std::path::Path) -> Result<()> {
    wechat_voice_audio::wave::read(path).map(|_| ())
}
pub enum Event {
    Started,
    Activity(&'static str),
    Speech(SpeechEvent),
    Input {
        name: String,
        muted: Option<bool>,
        volume: Option<f32>,
    },
    Progress {
        samples: u64,
        peak: u32,
        rms: f64,
        pieces: u32,
        responses: usize,
        partials: usize,
        finals: usize,
    },
}
enum Audio {
    Ready(Option<wechat_voice_audio::microphone::Device>),
    Start,
    Chunk(Vec<u8>),
    End,
}

#[derive(Default)]
struct Signal {
    samples: AtomicU64,
    peak: AtomicU32,
    square: AtomicU64,
}
impl Signal {
    fn accept(&self, pcm: &[i16]) {
        let mut peak = 0;
        let mut square = 0_u64;
        for &sample in pcm {
            peak = peak.max(u32::from(sample.unsigned_abs()));
            square += (i64::from(sample) * i64::from(sample)) as u64;
        }
        self.samples.fetch_add(pcm.len() as u64, Ordering::Relaxed);
        self.peak.fetch_max(peak, Ordering::Relaxed);
        self.square.fetch_add(square, Ordering::Relaxed);
    }
    fn event(&self, pieces: u32, responses: usize, partials: usize, finals: usize) -> Event {
        let samples = self.samples.load(Ordering::Relaxed);
        Event::Progress {
            samples,
            peak: self.peak.swap(0, Ordering::Relaxed),
            rms: (self.square.load(Ordering::Relaxed) as f64 / samples.max(1) as f64).sqrt(),
            pieces,
            responses,
            partials,
            finals,
        }
    }
}

struct Capture {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<()>>>,
    input: Receiver<Audio>,
    signal: Arc<Signal>,
}

impl Capture {
    fn start(source: Source, stop: Arc<AtomicBool>) -> Self {
        // Bounded audio while DNS/TLS/init is in flight; idle PCM is never queued.
        let (sender, input) = mpsc::sync_channel(128);
        let thread_stop = stop.clone();
        let signal = Arc::new(Signal::default());
        let producer_signal = signal.clone();
        let worker = thread::spawn(move || produce(source, &thread_stop, sender, &producer_signal));
        Self {
            stop,
            worker: Some(worker),
            input,
            signal,
        }
    }

    fn join(&mut self) -> Result<()> {
        self.worker.take().map_or(Ok(()), |thread| {
            thread.join().map_err(|_| "audio worker failed")?
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.join();
    }
}

fn produce(
    source: Source,
    stop: &AtomicBool,
    sender: SyncSender<Audio>,
    signal: &Signal,
) -> Result<()> {
    let hold_wave = matches!(&source, Source::WaveHold(_));
    let finite_wave = matches!(&source, Source::Wave(_));
    let settings = Settings::load_default()?;
    let mut turns = AudioTurns::new(sender, settings.silence_seconds)?;
    let ready_sender = turns.sender.clone();
    let mut accept = |pcm: &[i16]| -> Result<()> {
        signal.accept(pcm);
        turns.accept(pcm)
    };
    match source {
        Source::Microphone(device) => {
            let mut microphone = Microphone::open(device.as_deref())?;
            turns_ready(&ready_sender, Some(microphone.device.clone()))?;
            while !stop.load(Ordering::Acquire) {
                microphone.poll(&mut accept)?;
            }
        }
        Source::Wave(path) | Source::WaveHold(path) => {
            let mut samples = wechat_voice_audio::wave::read(&path)?;
            if hold_wave {
                samples.splice(..0, std::iter::repeat_n(0, 16_000));
            }
            turns_ready(&ready_sender, None)?;
            let start = Instant::now();
            for (index, chunk) in samples.chunks(CHUNK_SAMPLES).enumerate() {
                let due = Duration::from_millis(index as u64 * 120);
                while start.elapsed() < due && !stop.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(10));
                }
                if stop.load(Ordering::Acquire) {
                    break;
                }
                accept(chunk)?;
            }
            let mut index = samples.len().div_ceil(CHUNK_SAMPLES);
            while hold_wave && !stop.load(Ordering::Acquire) {
                while start.elapsed() < Duration::from_millis(index as u64 * 120)
                    && !stop.load(Ordering::Acquire)
                {
                    thread::sleep(Duration::from_millis(10));
                }
                if stop.load(Ordering::Acquire) {
                    break;
                }
                accept(&[0; CHUNK_SAMPLES])?;
                index += 1;
            }
        }
    }
    drop(accept);
    if finite_wave && !stop.load(Ordering::Acquire) {
        // A fixture may end on the last syllable. Supply the same sample-clocked
        // silence tail as live input before its natural EOF, without extending
        // the recorded-input signal counters or waiting for wall-clock silence.
        return turns.finish_wave();
    }
    turns.finish()
}

fn turns_ready(
    sender: &SyncSender<Audio>,
    device: Option<wechat_voice_audio::microphone::Device>,
) -> Result<()> {
    sender
        .try_send(Audio::Ready(device))
        .map_err(|_| "audio queue is full or closed".into())
}

/// Encoders belong to individual turns, so each stream starts with its Opus header.
struct AudioTurns {
    sender: SyncSender<Audio>,
    activity: Activity,
    encoder: Option<OpusStream>,
    pending: Vec<i16>,
}

impl AudioTurns {
    fn new(sender: SyncSender<Audio>, silence_seconds: u32) -> Result<Self> {
        Ok(Self {
            sender,
            activity: Activity::with_silence_seconds(silence_seconds)?,
            encoder: None,
            pending: Vec::with_capacity(CHUNK_SAMPLES * 2),
        })
    }

    fn accept(&mut self, pcm: &[i16]) -> Result<()> {
        for event in self.activity.push(pcm) {
            self.event(event)?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        for event in self.activity.finish() {
            self.event(event)?;
        }
        Ok(())
    }

    fn finish_wave(&mut self) -> Result<()> {
        self.accept(&vec![0; self.activity.silence_samples()])?;
        self.finish()
    }

    fn send(&self, event: Audio) -> Result<()> {
        self.sender
            .try_send(event)
            .map_err(|_| "audio queue is full or closed".into())
    }

    fn event(&mut self, event: ActivityEvent) -> Result<()> {
        match event {
            ActivityEvent::Started(pcm) => {
                self.encoder = Some(OpusStream::new()?);
                self.send(Audio::Start)?;
                self.audio(&pcm)?;
            }
            ActivityEvent::Audio(pcm) => self.audio(&pcm)?,
            ActivityEvent::Ended => {
                if !self.pending.is_empty() {
                    self.pending.resize(
                        self.pending.len().div_ceil(FRAME_SAMPLES) * FRAME_SAMPLES,
                        0,
                    );
                    let encoded = self
                        .encoder
                        .as_mut()
                        .ok_or("missing turn encoder")?
                        .encode(&self.pending)?;
                    self.send(Audio::Chunk(encoded))?;
                    self.pending.clear();
                }
                self.encoder = None;
                self.send(Audio::End)?;
            }
        }
        Ok(())
    }

    fn audio(&mut self, pcm: &[i16]) -> Result<()> {
        self.pending.extend_from_slice(pcm);
        while self.pending.len() >= CHUNK_SAMPLES {
            let encoded = self
                .encoder
                .as_mut()
                .ok_or("missing turn encoder")?
                .encode(&self.pending[..CHUNK_SAMPLES])?;
            self.send(Audio::Chunk(encoded))?;
            self.pending.drain(..CHUNK_SAMPLES);
        }
        Ok(())
    }
}

fn identity(account: &runtime_account::RuntimeAccount) -> rpc::Identity<'_> {
    rpc::Identity {
        cloud_uin: account.app_uin,
        wss_token: &account.wss_token,
        auth_type: account.auth_type(),
        host: "shortcloud.weixin.com",
        app_id: (account.session_fields & 4 != 0).then_some(account.app_id.as_str()),
        client_version: (account.session_fields & 32 != 0).then_some(account.client_version),
        server_group_key: (account.session_fields & 64 != 0).then_some(account.server_group_key),
    }
}

struct Turn {
    session: Session,
    stream_id: String,
    profile: Profile,
    context: Vec<u8>,
    request: u32,
    piece: u32,
    last_send: Instant,
}

impl Turn {
    fn start(account: &runtime_account::RuntimeAccount, stop: &AtomicBool) -> Result<Option<Self>> {
        let stream_id = new_id()?;
        let profile = Profile {
            client_version: account.client_version,
            key: new_id()?,
        };
        let context = profile.context();
        let mut session = Session::connect(&Connection::official())?;
        if stop.load(Ordering::Acquire) {
            let _ = session.close();
            return Ok(None);
        }
        session.send(rpc::request(
            1,
            &identity(account),
            rpc::INIT_PATH,
            &stream::init(account.device_id.as_bytes(), &stream_id, 3),
        )?)?;
        let deadline = Instant::now() + Duration::from_secs(8);
        while session.received == 0 {
            if stop.load(Ordering::Acquire) {
                let _ = session.close();
                return Ok(None);
            }
            if !session.poll()? {
                return Err("speech service closed during initialization".into());
            }
            if Instant::now() >= deadline {
                return Err("speech initialization timed out".into());
            }
        }
        Ok(Some(Self {
            session,
            stream_id,
            profile,
            context,
            request: 1,
            piece: 0,
            last_send: Instant::now(),
        }))
    }

    fn sync(
        &mut self,
        account: &runtime_account::RuntimeAccount,
        audio: Option<&[u8]>,
    ) -> Result<()> {
        let business = if let Some(audio) = audio {
            self.piece = self
                .piece
                .checked_add(1)
                .ok_or("speech piece count exceeded")?;
            Some(self.profile.audio(self.piece, audio))
        } else {
            None
        };
        let body = stream::sync(&stream::Sync {
            device_id: account.device_id.as_bytes(),
            stream_id: &self.stream_id,
            business: 3,
            piece_sequence: self.piece,
            piece: business.as_deref(),
            down_ack: self.session.recognition.down_ack,
            context: &self.context,
        });
        self.request = self
            .request
            .checked_add(1)
            .ok_or("speech request count exceeded")?;
        self.session.send(rpc::request(
            self.request,
            &identity(account),
            rpc::SYNC_PATH,
            &body,
        )?)?;
        self.last_send = Instant::now();
        Ok(())
    }

    fn events(
        &mut self,
        counts: &mut Counts,
        output: &mut impl FnMut(Event) -> Result<()>,
    ) -> Result<()> {
        emit_speech(&mut self.session.recognition, counts, output)
    }

    fn finish(
        mut self,
        counts: &mut Counts,
        output: &mut impl FnMut(Event) -> Result<()>,
    ) -> Result<String> {
        // Three seconds of silence already reached the service. Only allow a short
        // downstream drain, while capture independently retains the next onset.
        let deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < deadline {
            if !self.session.poll()? {
                break;
            }
            self.events(counts, output)?;
        }
        // This Receiver is never reused after finish: late edits cannot cross turns.
        let text = finish_transcript(&mut self.session.recognition, counts, output)?;
        counts.responses += self.session.received;
        let _ = self.session.close();
        Ok(text)
    }
}

fn emit_speech(
    receiver: &mut SpeechReceiver,
    counts: &mut Counts,
    output: &mut impl FnMut(Event) -> Result<()>,
) -> Result<()> {
    while let Some(event) = receiver.events.pop_front() {
        if event.final_result {
            counts.finals += 1;
        } else {
            counts.partials += 1;
        }
        output(Event::Speech(event))?;
    }
    Ok(())
}

fn finish_transcript(
    receiver: &mut SpeechReceiver,
    counts: &mut Counts,
    output: &mut impl FnMut(Event) -> Result<()>,
) -> Result<String> {
    receiver.finish();
    emit_speech(receiver, counts, output)?;
    Ok(std::mem::take(&mut receiver.text))
}

#[derive(Default)]
struct Counts {
    pieces: u32,
    responses: usize,
    partials: usize,
    finals: usize,
}

impl Counts {
    fn progress(&self, signal: &Signal, turn: Option<&Turn>) -> Event {
        signal.event(
            self.pieces,
            self.responses + turn.map_or(0, |t| t.session.received),
            self.partials,
            self.finals,
        )
    }
}

pub fn run(
    pid: u32,
    source: Source,
    stop: Arc<AtomicBool>,
    mut output: impl FnMut(Event) -> Result<()>,
) -> Result<String> {
    let mut account = runtime_account::read(pid)?;
    let mut capture = Capture::start(source, stop.clone());
    let mut turn: Option<Turn> = None;
    let mut counts = Counts::default();
    let mut ready = false;
    let mut text = String::new();
    let mut last_report = Instant::now();
    loop {
        match capture.input.try_recv() {
            Ok(Audio::Ready(device)) => {
                ready = true;
                if let Some(device) = device {
                    output(Event::Input {
                        name: device.name,
                        muted: device.muted,
                        volume: device.volume,
                    })?;
                }
                // SDK/local listening starts once, independently of cloud turns.
                output(Event::Started)?;
                output(Event::Activity("armed"))?;
            }
            Ok(Audio::Start) => {
                if turn.is_some() {
                    return Err("overlapping speech turns".into());
                }
                if !stop.load(Ordering::Acquire) {
                    output(Event::Activity("connecting"))?;
                    account = runtime_account::read(pid)?;
                    turn = Turn::start(&account, &stop)?;
                    if turn.is_some() {
                        output(Event::Activity("recognizing"))?;
                    }
                }
            }
            Ok(Audio::Chunk(audio)) => {
                if let Some(turn) = turn.as_mut() {
                    turn.sync(&account, Some(&audio))?;
                    counts.pieces = counts
                        .pieces
                        .checked_add(1)
                        .ok_or("speech piece count exceeded")?;
                }
            }
            Ok(Audio::End) => {
                if let Some(ended) = turn.take() {
                    let part = ended.finish(&mut counts, &mut output)?;
                    if text.len() + part.len() > (1 << 20) {
                        return Err("dictation text exceeds limit".into());
                    }
                    text.push_str(&part);
                    output(Event::Activity("paused"))?;
                    output(counts.progress(&capture.signal, None))?;
                    last_report = Instant::now();
                }
            }
            Err(TryRecvError::Disconnected) => {
                capture.join()?;
                if !ready {
                    return Err("audio capture failed to initialize".into());
                }
                if turn.is_some() {
                    return Err("audio capture ended inside a speech turn".into());
                }
                break;
            }
            Err(TryRecvError::Empty) => {
                if turn.is_none() {
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
        if let Some(turn) = turn.as_mut() {
            if turn.last_send.elapsed() >= Duration::from_millis(120) {
                turn.sync(&account, None)?;
            }
            if !turn.session.poll()? {
                return Err("speech service closed during a speech turn".into());
            }
            turn.events(&mut counts, &mut output)?;
        }
        if last_report.elapsed() >= Duration::from_millis(250) {
            output(counts.progress(&capture.signal, turn.as_ref()))?;
            last_report = Instant::now();
        }
    }
    output(counts.progress(&capture.signal, None))?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wechat_voice_audio::{activity::SILENCE_SAMPLES, encoder::HEADER};
    use wechat_voice_protocol::wire;

    fn voice() -> Vec<i16> {
        let mut phase = 0.0_f64;
        (0..16_000)
            .map(|i| {
                phase +=
                    std::f64::consts::TAU * (145.0 + 25.0 * (i as f64 / 1400.0).sin()) / 16_000.0;
                ((phase.sin() + 0.4 * (phase * 3.0).sin() + 0.2 * (phase * 7.0).sin())
                    * (2400.0 + 900.0 * (i as f64 / 530.0).sin())) as i16
            })
            .collect()
    }

    /// A real protocol envelope with committed=0, reproducing the observed service behavior.
    fn partial(text: &str) -> Vec<u8> {
        let mut result = Vec::new();
        wire::put_bytes(&mut result, 3, text.as_bytes());
        let mut piece = Vec::new();
        wire::put_integer(&mut piece, 1, 1);
        wire::put_bytes(&mut piece, 2, &result);
        let mut body = Vec::new();
        wire::put_bytes(&mut body, 3, &piece);
        let mut http = Vec::new();
        wire::put_integer(&mut http, 2, 200);
        wire::put_bytes(&mut http, 5, &body);
        let mut frame = Vec::new();
        wire::put_bytes(&mut frame, 4, &http);
        frame
    }

    #[test]
    fn two_turns_finalize_without_eof_and_idle_has_no_audio() {
        let (sender, input) = mpsc::sync_channel(128);
        let mut audio = AudioTurns::new(sender, 3).unwrap();
        let mut receiver = None;
        let mut counts = Counts::default();
        let mut speech = Vec::new();
        let mut output = |event| {
            if let Event::Speech(e) = event {
                speech.push(e);
            }
            Ok(())
        };
        for (index, text) in ["第一句", "第二句"].iter().enumerate() {
            audio.accept(&vec![0; 16_000]).unwrap();
            assert!(input.try_recv().is_err());
            audio.accept(&voice()).unwrap();
            audio.accept(&vec![0; SILENCE_SAMPLES - 1]).unwrap();
            let mut first_piece = true;
            while let Ok(event) = input.try_recv() {
                match event {
                    Audio::Start => {
                        assert!(receiver.is_none());
                        receiver = Some(SpeechReceiver::default());
                    }
                    Audio::Chunk(bytes) => {
                        assert_eq!(bytes.starts_with(HEADER), first_piece);
                        if first_piece {
                            receiver.as_mut().unwrap().ingest(&partial(text)).unwrap();
                            emit_speech(receiver.as_mut().unwrap(), &mut counts, &mut output)
                                .unwrap();
                        }
                        first_piece = false;
                    }
                    Audio::End => panic!("turn ended before three seconds"),
                    Audio::Ready(_) => unreachable!(),
                }
            }
            assert_eq!(counts.finals, index);
            audio.accept(&[0]).unwrap();
            while let Ok(event) = input.try_recv() {
                if let Audio::End = event {
                    let mut ended = receiver.take().unwrap();
                    assert_eq!(
                        finish_transcript(&mut ended, &mut counts, &mut output).unwrap(),
                        *text
                    );
                }
            }
            assert_eq!(counts.finals, index + 1);
            audio.accept(&vec![0; 16_000 * 8]).unwrap();
            assert!(input.try_recv().is_err(), "idle PCM reached upload queue");
        }
        drop(output);
        assert_eq!(
            speech
                .iter()
                .filter(|e| e.final_result)
                .map(|e| e.text.as_str())
                .collect::<Vec<_>>(),
            vec!["第一句", "第二句"]
        );
    }

    fn wave_fixture(name: &str, pcm: &[i16]) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("wechat-activity-{name}-{}.wav", std::process::id()));
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&(36 + pcm.len() as u32 * 2).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&16_000_u32.to_le_bytes());
        wav.extend_from_slice(&32_000_u32.to_le_bytes());
        wav.extend_from_slice(&2_u16.to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(pcm.len() as u32 * 2).to_le_bytes());
        for sample in pcm {
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        std::fs::write(&path, wav).unwrap();
        path
    }

    #[test]
    fn finite_wave_eof_finishes_without_losing_its_last_frame() {
        let path = wave_fixture("finite", &voice());
        let (sender, input) = mpsc::sync_channel(128);
        let signal = Signal::default();
        produce(
            Source::Wave(path.clone()),
            &AtomicBool::new(false),
            sender,
            &signal,
        )
        .unwrap();
        let events: Vec<_> = input.try_iter().collect();
        assert_eq!(
            events.iter().filter(|e| matches!(e, Audio::Start)).count(),
            1
        );
        assert!(matches!(events.first(), Some(Audio::Ready(None))));
        assert!(matches!(events.last(), Some(Audio::End)));
        assert_eq!(signal.samples.load(Ordering::Relaxed), 16_000);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn finite_wave_tail_honors_ten_second_threshold() {
        let (sender, input) = mpsc::sync_channel(128);
        let mut audio = AudioTurns::new(sender, 10).unwrap();
        audio.accept(&voice()).unwrap();
        audio.finish_wave().unwrap();
        let events: Vec<_> = input.try_iter().collect();
        assert_eq!(
            events.iter().filter(|e| matches!(e, Audio::Start)).count(),
            1
        );
        assert_eq!(events.iter().filter(|e| matches!(e, Audio::End)).count(), 1);
        assert!(matches!(events.last(), Some(Audio::End)));
    }

    #[test]
    fn wave_hold_worker_stops_and_joins_while_idle() {
        let path = wave_fixture("hold", &[0]);
        let stop = Arc::new(AtomicBool::new(false));
        let mut capture = Capture::start(Source::WaveHold(path.clone()), stop.clone());
        assert!(matches!(
            capture.input.recv_timeout(Duration::from_secs(2)).unwrap(),
            Audio::Ready(None)
        ));
        let start = Instant::now();
        stop.store(true, Ordering::Release);
        capture.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(matches!(
            capture.input.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
        std::fs::remove_file(path).unwrap();
    }
}
