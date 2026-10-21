use crate::abi::{self, Status};
use crate::ipc::{BridgeEvent, BridgeRequest};
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

const TAG: usize = 0x5758_0000_0000_0000;
const TAG_MASK: usize = 0xffff_0000_0000_0000;
const MAX_HANDLES: usize = 512;
const MAX_FALLBACK_JOBS: usize = MAX_HANDLES * 4;
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
thread_local! { static IN_CALLBACK: Cell<bool> = const { Cell::new(false) }; }

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}
pub(crate) fn is_fake(h: usize) -> bool {
    h & TAG_MASK == TAG
}

#[derive(Clone, Copy, Debug)]
#[repr(usize)]
pub(crate) enum CallbackKind {
    Started,
    Stopped,
    Recognizing,
    Recognized,
    Canceled,
    SpeechStart,
    SpeechEnd,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SavedCallback {
    pub function: usize,
    pub context: usize,
}

#[derive(Default)]
struct CompletionValues {
    ready: Option<Status>,
    drained: Option<Status>,
}
#[derive(Default)]
pub(crate) struct Completion {
    values: Mutex<CompletionValues>,
    changed: Condvar,
}
impl Completion {
    fn broker_ready(&self, status: Status) {
        let mut v = lock(&self.values);
        if v.ready.is_none() {
            v.ready = Some(status);
        }
        self.changed.notify_all();
    }
    fn finish(&self, status: Status) {
        let mut v = lock(&self.values);
        if v.ready.is_none() {
            v.ready = Some(status);
        }
        if v.drained.is_none() {
            v.drained = Some(status);
        }
        self.changed.notify_all();
    }
    pub(crate) fn wait(&self, milliseconds: u32) -> Status {
        let reentrant = IN_CALLBACK.with(Cell::get);
        let deadline = (milliseconds != u32::MAX)
            .then(|| Instant::now() + Duration::from_millis(milliseconds as u64));
        let mut values = lock(&self.values);
        loop {
            // A callback cannot wait for its own dispatcher to return. For this case
            // only, the broker's stop acknowledgement breaks that self-deadlock.
            if let Some(s) = if reentrant {
                values.ready
            } else {
                values.drained
            } {
                return s;
            }
            if let Some(deadline) = deadline {
                let now = Instant::now();
                if now >= deadline {
                    return abi::TIMEOUT;
                }
                let (next, _) = self
                    .changed
                    .wait_timeout(values, deadline - now)
                    .unwrap_or_else(|p| p.into_inner());
                values = next;
            } else {
                values = self.changed.wait(values).unwrap_or_else(|p| p.into_inner());
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Payload {
    pub session: String,
    pub text: String,
    pub result_id: String,
    pub reason: i32,
    pub offset: u64,
    pub duration: u64,
    pub error_code: u32,
    pub error_message: String,
}
impl Payload {
    fn empty(reco: usize, generation: u64, session: String) -> Self {
        Self {
            session,
            text: String::new(),
            result_id: format!("{reco:x}-{generation}"),
            reason: abi::RECOGNIZED,
            offset: 0,
            duration: 0,
            error_code: 0,
            error_message: String::new(),
        }
    }
    pub(crate) fn json(&self) -> String {
        if self.reason == abi::CANCELED {
            serde_json::json!({"RecognitionStatus":"Error","ErrorDetails":self.error_message})
                .to_string()
        } else if self.reason == abi::RECOGNIZING {
            serde_json::json!({"Id":self.result_id,"Text":self.text,
                "Offset":self.offset,"Duration":self.duration})
            .to_string()
        } else {
            serde_json::json!({"Id":self.result_id,"RecognitionStatus":"Success",
                "DisplayText":self.text,"Offset":self.offset,"Duration":self.duration,
                "NBest":[{"Lexical":self.text,"ITN":self.text,"MaskedITN":self.text,"Display":self.text}]}).to_string()
        }
    }
}

#[derive(Clone)]
enum Entry {
    Event(Arc<Payload>),
    Result(Arc<Payload>),
    Async(Arc<Completion>),
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Starting,
    Running,
    Stopping,
    Draining,
    Failed,
}
struct Recognizer {
    callbacks: [Option<SavedCallback>; 7],
    observed: u8,
    generation: u64,
    phase: Phase,
    session: String,
    start: Option<Arc<Completion>>,
    stop: Option<Arc<Completion>>,
    language: String,
    device: String,
    utterance: u64,
    in_speech: bool,
}
impl Default for Recognizer {
    fn default() -> Self {
        Self {
            callbacks: [None; 7],
            observed: 0,
            generation: 0,
            phase: Phase::Idle,
            session: String::new(),
            start: None,
            stop: None,
            language: String::new(),
            device: String::new(),
            utterance: 0,
            in_speech: false,
        }
    }
}
#[derive(Default)]
struct State {
    recos: HashMap<usize, Recognizer>,
    handles: HashMap<usize, Entry>,
    disconnected: bool,
    suspended: bool,
}
impl State {
    fn allocate(&mut self, entry: Entry) -> Result<usize, Status> {
        if self.handles.len() >= MAX_HANDLES {
            return Err(abi::NOT_IMPL);
        }
        let id = NEXT_HANDLE
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                (v < 0x0000_ffff_ffff_ffff).then_some(v + 1)
            })
            .map_err(|_| abi::NOT_IMPL)?;
        let handle = TAG | id as usize;
        self.handles.insert(handle, entry);
        Ok(handle)
    }
}

pub(crate) struct Job {
    reco: usize,
    generation: u64,
    kind: CallbackKind,
    payload: Arc<Payload>,
    finish_stop: Option<Arc<Completion>>,
    marks_idle: bool,
}

pub(crate) struct Engine {
    state: Mutex<State>,
    requests: mpsc::SyncSender<BridgeRequest>,
    jobs: mpsc::SyncSender<Job>,
    pending: AtomicUsize,
    fallback: Mutex<VecDeque<Job>>,
    flushing: AtomicBool,
}
impl Engine {
    pub(crate) fn new() -> (
        Arc<Self>,
        mpsc::Receiver<BridgeRequest>,
        mpsc::Receiver<Job>,
    ) {
        let (requests, request_rx) = mpsc::sync_channel(64);
        let (jobs, job_rx) = mpsc::sync_channel(128);
        (
            Arc::new(Self {
                state: Mutex::new(State::default()),
                requests,
                jobs,
                pending: AtomicUsize::new(0),
                fallback: Mutex::new(VecDeque::new()),
                flushing: AtomicBool::new(false),
            }),
            request_rx,
            job_rx,
        )
    }
    pub(crate) fn save_callback(
        &self,
        reco: usize,
        kind: CallbackKind,
        callback: Option<SavedCallback>,
    ) {
        let mut state = lock(&self.state);
        let recognizer = state.recos.entry(reco).or_default();
        recognizer.callbacks[kind as usize] = callback;
        recognizer.observed |= 1 << kind as usize;
    }
    pub(crate) fn seed_callbacks(&self, reco: usize, callbacks: [Option<SavedCallback>; 7]) {
        let mut state = lock(&self.state);
        let recognizer = state.recos.entry(reco).or_default();
        for (index, callback) in callbacks.into_iter().enumerate() {
            if recognizer.observed & (1 << index) == 0 {
                recognizer.callbacks[index] = callback;
                recognizer.observed |= 1 << index;
            }
        }
    }
    pub(crate) fn has_final_callback(&self, reco: usize) -> bool {
        lock(&self.state)
            .recos
            .get(&reco)
            .is_some_and(|r| r.callbacks[CallbackKind::Recognized as usize].is_some())
    }
    pub(crate) fn needs_callback_seed(&self, reco: usize) -> bool {
        lock(&self.state)
            .recos
            .get(&reco)
            .is_none_or(|r| r.observed != 0x7f)
    }
    pub(crate) fn owns_recognizer(&self, reco: usize) -> bool {
        lock(&self.state)
            .recos
            .get(&reco)
            .is_some_and(|r| r.generation != 0)
    }
    pub(crate) fn start(
        &self,
        reco: usize,
        language: String,
        device: String,
    ) -> Result<usize, Status> {
        let done = Arc::new(Completion::default());
        let mut state = lock(&self.state);
        if state.suspended {
            return Err(abi::NOT_IMPL);
        }
        if state.disconnected {
            drop(state);
            return self.decline_start(reco);
        }
        let r = state.recos.get(&reco).ok_or(abi::NOT_IMPL)?;
        if r.callbacks[CallbackKind::Recognized as usize].is_none() {
            return Err(abi::NOT_IMPL);
        }
        if !matches!(r.phase, Phase::Idle | Phase::Failed) {
            return Err(abi::INVALID_ARG);
        }
        let generation = NEXT_GENERATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1))
            .map_err(|_| abi::NOT_IMPL)?;
        let handle = state.allocate(Entry::Async(done.clone()))?;
        let r = state.recos.get_mut(&reco).ok_or(abi::INVALID_HANDLE)?;
        r.generation = generation;
        r.phase = Phase::Starting;
        r.session.clear();
        r.start = Some(done);
        r.stop = None;
        r.language = language.clone();
        r.device = device.clone();
        r.utterance = 0;
        r.in_speech = false;
        drop(state);
        if self
            .requests
            .try_send(BridgeRequest::Start {
                reco: reco as u64,
                generation,
                language,
                device,
            })
            .is_err()
        {
            self.disconnected("bridge request pipe unavailable");
        }
        Ok(handle)
    }
    /// A checked recovery/broker refusal is reported through events, not an SDK
    /// exception. Never sends a request or starts Microsoft recognition.
    pub(crate) fn decline_start(&self, reco: usize) -> Result<usize, Status> {
        let mut state = lock(&self.state);
        if state.suspended {
            return Err(abi::NOT_IMPL);
        }
        let phase = state.recos.get(&reco).map_or(Phase::Idle, |r| r.phase);
        if !matches!(phase, Phase::Idle | Phase::Failed) {
            if state.disconnected {
                // Its cancellation/Stopped is already draining. A retry must not
                // replace that generation or expose another synchronous SDK error.
                let completion = Arc::new(Completion::default());
                completion.finish(abi::OK);
                return state.allocate(Entry::Async(completion));
            }
            return Err(abi::INVALID_ARG);
        }
        let generation = NEXT_GENERATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1))
            .map_err(|_| abi::NOT_IMPL)?;
        let completion = Arc::new(Completion::default());
        completion.finish(abi::OK);
        let handle = state.allocate(Entry::Async(completion))?;
        let r = state.recos.entry(reco).or_default();
        r.generation = generation;
        r.phase = Phase::Failed;
        r.session = format!("bridge-declined-{generation}");
        r.start = None;
        r.stop = None;
        r.in_speech = false;
        let mut payload = Payload::empty(reco, generation, r.session.clone());
        payload.reason = abi::CANCELED;
        payload.error_code = abi::NOT_IMPL as u32;
        payload.error_message = "bridge callback recovery or broker unavailable".into();
        let jobs = [CallbackKind::Canceled, CallbackKind::Stopped]
            .into_iter()
            .filter(|kind| r.callbacks[*kind as usize].is_some())
            .map(|kind| Job {
                reco,
                generation,
                kind,
                payload: Arc::new(payload.clone()),
                finish_stop: None,
                marks_idle: false,
            })
            .collect::<Vec<_>>();
        self.pending.fetch_add(jobs.len(), Ordering::SeqCst);
        drop(state);
        if self.enqueue_jobs(jobs) {
            self.disconnected("bridge callback dispatcher unavailable or full");
        }
        Ok(handle)
    }
    pub(crate) fn stop(&self, reco: usize) -> Result<usize, Status> {
        let mut state = lock(&self.state);
        let r = state.recos.get(&reco).ok_or(abi::NOT_IMPL)?;
        let idle = matches!(r.phase, Phase::Idle | Phase::Failed);
        let done = r
            .stop
            .clone()
            .unwrap_or_else(|| Arc::new(Completion::default()));
        let handle = state.allocate(Entry::Async(done.clone()))?;
        let r = state.recos.get_mut(&reco).ok_or(abi::INVALID_HANDLE)?;
        if idle {
            done.finish(abi::OK);
            return Ok(handle);
        }
        if r.stop.is_some() {
            return Ok(handle);
        }
        r.stop = Some(done);
        if r.phase == Phase::Draining {
            // The existing Stopped job owns termination; a repeated stop must not
            // create a second backend request or a completion nobody can finish.
            if let Some(c) = &r.stop {
                c.broker_ready(abi::OK);
            }
            return Ok(handle);
        }
        r.phase = Phase::Stopping;
        let request = BridgeRequest::Stop {
            reco: reco as u64,
            generation: r.generation,
            language: r.language.clone(),
            device: r.device.clone(),
        };
        drop(state);
        if self.requests.try_send(request).is_err() {
            self.disconnected("bridge request pipe unavailable or full");
        }
        Ok(handle)
    }
    pub(crate) fn forget_recognizer(&self, reco: usize) {
        let r = lock(&self.state).recos.remove(&reco);
        if let Some(r) = r {
            if let Some(c) = r.start {
                c.finish(abi::INVALID_HANDLE);
            }
            if let Some(c) = r.stop {
                c.finish(abi::INVALID_HANDLE);
            }
            if !matches!(r.phase, Phase::Idle | Phase::Failed) {
                let _ = self.requests.try_send(BridgeRequest::Stop {
                    reco: reco as u64,
                    generation: r.generation,
                    language: r.language,
                    device: r.device,
                });
            }
        }
    }
    pub(crate) fn completion(&self, handle: usize) -> Result<Arc<Completion>, Status> {
        match lock(&self.state).handles.get(&handle) {
            Some(Entry::Async(c)) => Ok(c.clone()),
            _ => Err(abi::INVALID_HANDLE),
        }
    }
    pub(crate) fn event(&self, handle: usize) -> Result<Arc<Payload>, Status> {
        match lock(&self.state).handles.get(&handle) {
            Some(Entry::Event(p)) => Ok(p.clone()),
            _ => Err(abi::INVALID_HANDLE),
        }
    }
    pub(crate) fn result(&self, handle: usize) -> Result<Arc<Payload>, Status> {
        match lock(&self.state).handles.get(&handle) {
            Some(Entry::Result(p)) => Ok(p.clone()),
            _ => Err(abi::INVALID_HANDLE),
        }
    }
    pub(crate) fn event_result(&self, handle: usize) -> Result<usize, Status> {
        let payload = self.event(handle)?;
        lock(&self.state).allocate(Entry::Result(payload))
    }
    fn release_kind(&self, h: usize, kind: u8) -> Status {
        let mut s = lock(&self.state);
        let valid = matches!(
            (s.handles.get(&h), kind),
            (Some(Entry::Event(_)), 0) | (Some(Entry::Result(_)), 1) | (Some(Entry::Async(_)), 2)
        );
        if valid {
            s.handles.remove(&h);
            abi::OK
        } else {
            abi::INVALID_HANDLE
        }
    }
    pub(crate) fn release_event(&self, h: usize) -> Status {
        self.release_kind(h, 0)
    }
    pub(crate) fn release_result(&self, h: usize) -> Status {
        self.release_kind(h, 1)
    }
    pub(crate) fn release_async(&self, h: usize) -> Status {
        self.release_kind(h, 2)
    }

    pub(crate) fn receive(&self, event: BridgeEvent) {
        if matches!(&event, BridgeEvent::Started { session, .. } if session.is_empty() || session.contains('\0'))
            || matches!(&event, BridgeEvent::Partial { text, .. } | BridgeEvent::Final { text, .. } if text.contains('\0'))
        {
            self.disconnected("invalid bridge session or result text");
            return;
        }
        let (reco, generation) = event.identity();
        let reco = reco as usize;
        let mut state = lock(&self.state);
        let Some(r) = state.recos.get_mut(&reco) else {
            return;
        };
        if r.generation != generation
            || matches!(r.phase, Phase::Idle | Phase::Failed | Phase::Draining)
        {
            return;
        }
        let mut payload = Payload::empty(reco, generation, r.session.clone());
        let terminal_error = matches!(&event, BridgeEvent::Error { .. });
        let final_result = matches!(&event, BridgeEvent::Final { .. });
        let mut before = Vec::new();
        let mut after = Vec::new();
        let (kind, finish_stop, marks_idle) = match event {
            BridgeEvent::Started { session, .. } => {
                if r.phase != Phase::Starting && r.phase != Phase::Stopping {
                    return;
                }
                if !r.session.is_empty() {
                    return;
                }
                r.session = session.clone();
                payload.session = session;
                if r.phase == Phase::Starting {
                    r.phase = Phase::Running;
                }
                if let Some(c) = r.start.take() {
                    c.finish(abi::OK);
                }
                (CallbackKind::Started, None, false)
            }
            BridgeEvent::SpeechActivity {
                speaking, offset, ..
            } => {
                if r.session.is_empty()
                    || !matches!(r.phase, Phase::Running | Phase::Stopping)
                    || (speaking && r.phase != Phase::Running)
                    || speaking == r.in_speech
                {
                    return;
                }
                r.in_speech = speaking;
                payload.offset = offset;
                (
                    if speaking {
                        CallbackKind::SpeechStart
                    } else {
                        CallbackKind::SpeechEnd
                    },
                    None,
                    false,
                )
            }
            BridgeEvent::Partial {
                text,
                offset,
                duration,
                ..
            }
            | BridgeEvent::Final {
                text,
                offset,
                duration,
                ..
            } => {
                if r.session.is_empty() || !matches!(r.phase, Phase::Running | Phase::Stopping) {
                    return;
                }
                payload.text = text;
                payload.offset = offset;
                payload.duration = duration;
                payload.reason = if final_result {
                    abi::RECOGNIZED
                } else {
                    abi::RECOGNIZING
                };
                payload.result_id = format!("{reco:x}-{generation}-{}", r.utterance);
                if !r.in_speech {
                    before.push((CallbackKind::SpeechStart, Arc::new(payload.clone())));
                    r.in_speech = true;
                }
                if final_result {
                    r.utterance = r.utterance.saturating_add(1);
                    r.in_speech = false;
                    after.push((CallbackKind::SpeechEnd, Arc::new(payload.clone())));
                }
                (
                    if final_result {
                        CallbackKind::Recognized
                    } else {
                        CallbackKind::Recognizing
                    },
                    None,
                    false,
                )
            }
            BridgeEvent::Stopped { .. } => {
                if let Some(c) = r.start.take() {
                    c.finish(abi::OK);
                }
                if r.in_speech {
                    before.push((CallbackKind::SpeechEnd, Arc::new(payload.clone())));
                    r.in_speech = false;
                }
                r.phase = Phase::Draining;
                if let Some(c) = &r.stop {
                    c.broker_ready(abi::OK);
                }
                (CallbackKind::Stopped, r.stop.clone(), true)
            }
            BridgeEvent::Error { error, .. } => {
                if let Some(c) = r.start.take() {
                    c.finish(abi::OK);
                }
                // Operational failure belongs to Canceled/Stopped events. Returning
                // an SDK start/wait error makes Carbon throw across its WinRT boundary.
                r.phase = Phase::Stopping;
                payload.reason = abi::CANCELED;
                payload.error_code = if error.code == 0 {
                    abi::NOT_IMPL as u32
                } else {
                    error.code
                };
                payload.error_message = error.message;
                (CallbackKind::Canceled, None, false)
            }
        };
        let mut jobs = before
            .into_iter()
            .map(|(kind, payload)| Job {
                reco,
                generation,
                kind,
                payload,
                finish_stop: None,
                marks_idle: false,
            })
            .collect::<Vec<_>>();
        jobs.push(Job {
            reco,
            generation,
            kind,
            payload: Arc::new(payload),
            finish_stop,
            marks_idle,
        });
        jobs.extend(after.into_iter().map(|(kind, payload)| Job {
            reco,
            generation,
            kind,
            payload,
            finish_stop: None,
            marks_idle: false,
        }));
        self.pending.fetch_add(jobs.len(), Ordering::SeqCst);
        drop(state);
        let congested = self.enqueue_jobs(jobs);
        if terminal_error {
            self.receive(BridgeEvent::Stopped {
                reco: reco as u64,
                generation,
            });
        }
        if congested {
            self.disconnected("bridge callback dispatcher unavailable or full");
        }
    }
    pub(crate) fn disconnected(&self, message: &str) {
        crate::hooks::diagnostic_or(1 << 7);
        let jobs = {
            let mut s = lock(&self.state);
            s.disconnected = true;
            let mut jobs = Vec::new();
            for (reco, r) in &mut s.recos {
                if matches!(r.phase, Phase::Idle | Phase::Failed | Phase::Draining) {
                    continue;
                }
                r.phase = Phase::Draining;
                if let Some(c) = r.start.take() {
                    c.finish(abi::OK);
                }
                if let Some(c) = &r.stop {
                    c.broker_ready(abi::OK);
                }
                let mut payload = Payload::empty(*reco, r.generation, r.session.clone());
                payload.reason = abi::CANCELED;
                payload.error_code = abi::NOT_IMPL as u32;
                payload.error_message = message.to_owned();
                jobs.push(Job {
                    reco: *reco,
                    generation: r.generation,
                    kind: CallbackKind::Canceled,
                    payload: Arc::new(payload),
                    finish_stop: None,
                    marks_idle: false,
                });
                jobs.push(Job {
                    reco: *reco,
                    generation: r.generation,
                    kind: CallbackKind::Stopped,
                    payload: Arc::new(Payload::empty(*reco, r.generation, r.session.clone())),
                    finish_stop: r.stop.clone(),
                    marks_idle: true,
                });
            }
            self.pending.fetch_add(jobs.len(), Ordering::SeqCst);
            jobs
        };
        self.enqueue_jobs(jobs);
    }
    pub(crate) fn dispatch(&self, job: Job) {
        self.dispatch_one(job);
        self.flush_fallback();
    }
    fn dispatch_one(&self, job: Job) {
        struct Pending<'a>(&'a AtomicUsize);
        impl Drop for Pending<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _pending = Pending(&self.pending);
        let (callback, handle, stopped) = {
            let mut s = lock(&self.state);
            let Some(r) = s.recos.get_mut(&job.reco) else {
                if let Some(c) = job.finish_stop {
                    c.finish(abi::INVALID_HANDLE);
                }
                return;
            };
            if r.generation != job.generation {
                return;
            }
            let cb = r.callbacks[job.kind as usize];
            // All preceding results have drained. Native SessionStopped may
            // synchronously start again, so publish Idle before calling it.
            // Detach the old stop completion so reentry cannot finish it early
            // or let old cleanup overwrite the next generation.
            let stopped = if job.marks_idle {
                r.phase = Phase::Idle;
                r.stop.take()
            } else {
                None
            };
            let handle = if cb.is_some() {
                s.allocate(Entry::Event(job.payload.clone())).ok()
            } else {
                None
            };
            (cb, handle, stopped)
        };
        if let (Some(callback), Some(handle)) = (callback, handle) {
            // No registry/session/pipe mutex is held across application code.
            let previous = IN_CALLBACK.replace(true);
            // SAFETY: saved from a real SDK callback setter with the same ABI.
            unsafe {
                let f: unsafe extern "system" fn(abi::Handle, abi::Handle, *mut core::ffi::c_void) =
                    core::mem::transmute(callback.function);
                f(
                    job.reco as abi::Handle,
                    handle as abi::Handle,
                    callback.context as *mut _,
                );
            }
            IN_CALLBACK.set(previous);
            // No event-AddRef API exists in the intercepted C ABI. The event is
            // borrowed for the callback; result handles returned from it own an
            // independent Arc. SDK wrappers normally release the event themselves.
            self.release_event(handle);
        } else if callback.is_some() {
            self.disconnected("bridge handle limit reached");
        }
        if let Some(completion) = stopped {
            completion.finish(abi::OK);
        }
        if let Some(c) = job.finish_stop {
            c.finish(abi::OK);
        }
    }
    /// Overflow is held in a bounded emergency tail, not silently dropped. It
    /// drains only after every already queued callback, preserving final/terminal
    /// ordering. A tiny wake job keeps the existing dispatcher thread responsible.
    fn enqueue_jobs(&self, jobs: Vec<Job>) -> bool {
        let mut congested = false;
        for job in jobs {
            let mut tail = lock(&self.fallback);
            if !tail.is_empty() || self.flushing.load(Ordering::Acquire) {
                if tail.len() < MAX_FALLBACK_JOBS {
                    tail.push_back(job);
                    congested = true;
                    continue;
                }
                drop(tail);
                self.abandon_job(job);
                congested = true;
                continue;
            }
            drop(tail);
            match self.jobs.try_send(job) {
                Ok(()) => (),
                Err(mpsc::TrySendError::Full(job)) => {
                    lock(&self.fallback).push_back(job);
                    congested = true;
                }
                Err(mpsc::TrySendError::Disconnected(job)) => {
                    self.abandon_job(job);
                    congested = true;
                }
            }
        }
        if !lock(&self.fallback).is_empty() {
            self.pending.fetch_add(1, Ordering::SeqCst);
            let wake = Job {
                reco: 0,
                generation: 0,
                kind: CallbackKind::Stopped,
                payload: Arc::new(Payload::empty(0, 0, String::new())),
                finish_stop: None,
                marks_idle: false,
            };
            if self.jobs.try_send(wake).is_err() {
                self.pending.fetch_sub(1, Ordering::SeqCst);
            }
        }
        congested
    }
    fn abandon_job(&self, job: Job) {
        self.pending.fetch_sub(1, Ordering::SeqCst);
        if job.marks_idle {
            let completion = {
                let mut s = lock(&self.state);
                s.recos.get_mut(&job.reco).and_then(|r| {
                    if r.generation == job.generation {
                        r.phase = Phase::Failed;
                        r.stop.take()
                    } else {
                        None
                    }
                })
            };
            if let Some(c) = completion {
                c.finish(abi::OK);
            }
        }
        if let Some(c) = job.finish_stop {
            c.finish(abi::OK);
        }
    }
    fn flush_fallback(&self) {
        if self
            .flushing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        loop {
            let job = {
                let mut tail = lock(&self.fallback);
                if self.pending.load(Ordering::SeqCst) == tail.len() {
                    tail.pop_front()
                } else {
                    None
                }
            };
            let Some(job) = job else {
                break;
            };
            self.dispatch_one(job);
        }
        self.flushing.store(false, Ordering::Release);
    }
    /// Disabling with an outstanding event/result/async could expose fake handles to
    /// the restored SDK. Refuse until consumers have released every bridge handle.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn suspend_if_idle(&self) -> bool {
        let mut state = lock(&self.state);
        if !state.handles.is_empty()
            || self.pending.load(Ordering::SeqCst) != 0
            || state
                .recos
                .values()
                .any(|r| !matches!(r.phase, Phase::Idle | Phase::Failed))
        {
            return false;
        }
        state.suspended = true;
        true
    }
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn resume(&self) {
        lock(&self.state).suspended = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn active() -> (
        Arc<Engine>,
        mpsc::Receiver<BridgeRequest>,
        mpsc::Receiver<Job>,
        usize,
        u64,
    ) {
        let (engine, requests, jobs) = Engine::new();
        engine.save_callback(
            7,
            CallbackKind::Recognized,
            Some(SavedCallback {
                function: 1,
                context: 0,
            }),
        );
        let h = engine.start(7, "zh-CN".into(), String::new()).unwrap();
        let BridgeRequest::Start { generation, .. } = requests.recv().unwrap() else {
            panic!()
        };
        (engine, requests, jobs, h, generation)
    }
    #[test]
    fn activity_callbacks_are_ordered_deduplicated_without_recognition_jobs() {
        let (e, _, jobs, _, generation) = active();
        e.receive(BridgeEvent::Started {
            reco: 7,
            generation,
            session: "activity".into(),
        });
        assert!(matches!(jobs.recv().unwrap().kind, CallbackKind::Started));
        for speaking in [true, true, false, false, true] {
            e.receive(BridgeEvent::SpeechActivity {
                reco: 7,
                generation,
                speaking,
                offset: 42,
            });
        }
        for expected in [
            CallbackKind::SpeechStart,
            CallbackKind::SpeechEnd,
            CallbackKind::SpeechStart,
        ] {
            let job = jobs.recv().unwrap();
            assert_eq!(job.kind as usize, expected as usize);
            assert_eq!(job.payload.session, "activity");
            assert_eq!(job.payload.offset, 42);
            assert!(job.payload.text.is_empty());
        }
        assert!(jobs.try_recv().is_err());
        e.receive(BridgeEvent::Stopped {
            reco: 7,
            generation,
        });
        assert!(matches!(jobs.recv().unwrap().kind, CallbackKind::SpeechEnd));
        assert!(matches!(jobs.recv().unwrap().kind, CallbackKind::Stopped));
        assert!(jobs.try_recv().is_err());
    }

    #[test]
    fn activity_callbacks_are_recovered_even_when_final_is_already_captured() {
        let (e, _, _, _, _) = active();
        assert!(e.has_final_callback(7));
        assert!(e.needs_callback_seed(7));
        e.save_callback(7, CallbackKind::SpeechEnd, None);
        e.seed_callbacks(
            7,
            [Some(SavedCallback {
                function: 7,
                context: 9,
            }); 7],
        );
        assert!(!e.needs_callback_seed(7));
        let s = lock(&e.state);
        let r = s.recos.get(&7).unwrap();
        assert!(r.callbacks[CallbackKind::SpeechStart as usize].is_some());
        assert!(r.callbacks[CallbackKind::SpeechEnd as usize].is_none());
    }

    #[test]
    fn start_requires_captured_result_callback_and_rejects_old_generation() {
        let (e, _, _) = Engine::new();
        assert_eq!(e.start(7, String::new(), String::new()), Err(abi::NOT_IMPL));
        let (e, _, _, h, generation) = active();
        e.receive(BridgeEvent::Started {
            reco: 7,
            generation: generation + 1,
            session: "stale".into(),
        });
        assert_eq!(e.completion(h).unwrap().wait(0), abi::TIMEOUT);
        e.receive(BridgeEvent::Started {
            reco: 7,
            generation,
            session: "current".into(),
        });
        assert_eq!(e.completion(h).unwrap().wait(0), abi::OK);
        assert_eq!(e.release_async(h), abi::OK);
        assert!(is_fake(h));
        assert!(e.completion(h).is_err());
        assert_eq!(e.release_async(h), abi::INVALID_HANDLE);
    }
    #[test]
    fn stop_completion_waits_for_final_callback_dispatch() {
        let (e, requests, jobs, _, generation) = active();
        e.receive(BridgeEvent::Started {
            reco: 7,
            generation,
            session: "s".into(),
        });
        e.dispatch(jobs.recv().unwrap());
        // Disable callback invocation here; final is still an ordered dispatcher job.
        e.save_callback(7, CallbackKind::Recognized, None);
        let stop = e.stop(7).unwrap();
        assert!(matches!(
            requests.recv().unwrap(),
            BridgeRequest::Stop { .. }
        ));
        e.receive(BridgeEvent::Final {
            reco: 7,
            generation,
            text: "完整结果".into(),
            offset: 9,
            duration: 10,
        });
        e.receive(BridgeEvent::Stopped {
            reco: 7,
            generation,
        });
        assert_eq!(e.completion(stop).unwrap().wait(0), abi::TIMEOUT);
        for _ in 0..3 {
            e.dispatch(jobs.recv().unwrap());
            assert_eq!(e.completion(stop).unwrap().wait(0), abi::TIMEOUT);
        }
        e.dispatch(jobs.recv().unwrap());
        assert_eq!(e.completion(stop).unwrap().wait(0), abi::OK);
    }
    #[test]
    fn event_result_lifetimes_are_independent_and_tombstones_never_look_real() {
        let (e, _, _) = Engine::new();
        let p = Arc::new(Payload::empty(1, 1, "session".into()));
        let event = lock(&e.state).allocate(Entry::Event(p)).unwrap();
        let result = e.event_result(event).unwrap();
        assert_eq!(e.release_event(event), abi::OK);
        assert_eq!(e.result(result).unwrap().session, "session");
        assert_eq!(e.release_result(result), abi::OK);
        assert_eq!(e.release_event(result), abi::INVALID_HANDLE);
        assert!(is_fake(result));
        assert!(e.result(result).is_err());
    }
    #[test]
    fn pipe_failure_unblocks_infinite_waiters() {
        let (e, _, _, h, _) = active();
        e.disconnected("closed");
        assert_eq!(e.completion(h).unwrap().wait(u32::MAX), abi::OK);
    }

    #[test]
    fn operational_failure_reports_cancel_then_stopped_without_an_sdk_exception() {
        let (engine, _requests, jobs, start, generation) = active();
        let stop = engine.stop(7).unwrap();
        engine.receive(BridgeEvent::Error {
            reco: 7,
            generation,
            error: crate::ipc::BridgeError {
                message: "test connection refused".into(),
                code: 42,
            },
        });
        assert_eq!(engine.completion(start).unwrap().wait(0), abi::OK);
        assert_eq!(engine.completion(stop).unwrap().wait(0), abi::TIMEOUT);
        let canceled = jobs.recv().unwrap();
        assert!(matches!(canceled.kind, CallbackKind::Canceled));
        assert_eq!(canceled.payload.error_code, 42);
        assert_eq!(canceled.payload.error_message, "test connection refused");
        engine.dispatch(canceled);
        assert_eq!(engine.completion(stop).unwrap().wait(0), abi::TIMEOUT);
        let stopped = jobs.recv().unwrap();
        assert!(matches!(stopped.kind, CallbackKind::Stopped));
        engine.dispatch(stopped);
        assert_eq!(engine.completion(stop).unwrap().wait(0), abi::OK);
    }
    #[test]
    fn utterance_identity_survives_partial_updates_and_changes_after_final() {
        let (e, _, jobs, _, generation) = active();
        e.receive(BridgeEvent::Started {
            reco: 7,
            generation,
            session: "s".into(),
        });
        jobs.recv().unwrap();
        for text in ["第", "第一句"] {
            e.receive(BridgeEvent::Partial {
                reco: 7,
                generation,
                text: text.into(),
                offset: 0,
                duration: 1,
            });
        }
        e.receive(BridgeEvent::Final {
            reco: 7,
            generation,
            text: "第一句".into(),
            offset: 0,
            duration: 2,
        });
        e.receive(BridgeEvent::Partial {
            reco: 7,
            generation,
            text: "第二句".into(),
            offset: 2,
            duration: 1,
        });
        let ids = jobs
            .try_iter()
            .filter(|j| matches!(j.kind, CallbackKind::Recognizing | CallbackKind::Recognized))
            .map(|j| j.payload.result_id.clone())
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 4);
        assert_eq!(ids[0], ids[1]);
        assert_eq!(ids[1], ids[2]);
        assert_ne!(ids[2], ids[3]);
    }
    #[test]
    fn bounded_dispatch_overflow_cancels_without_blocking_the_pipe_reader() {
        let (e, requests, jobs, _, generation) = active();
        e.receive(BridgeEvent::Started {
            reco: 7,
            generation,
            session: "s".into(),
        });
        e.dispatch(jobs.recv().unwrap());
        let stop = e.stop(7).unwrap();
        requests.recv().unwrap();
        for _ in 0..140 {
            e.receive(BridgeEvent::Partial {
                reco: 7,
                generation,
                text: "x".into(),
                offset: 0,
                duration: 1,
            });
        }
        assert_eq!(e.completion(stop).unwrap().wait(0), abi::TIMEOUT);
        let mut count = 0;
        while let Ok(job) = jobs.try_recv() {
            e.dispatch(job);
            count += 1;
        }
        assert!(count <= 128);
        assert_eq!(e.completion(stop).unwrap().wait(0), abi::OK);
        assert_eq!(e.pending.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn callback_can_read_result_without_registry_lock_and_event_expires_on_return() {
        struct Context {
            engine: Arc<Engine>,
            event: usize,
            result: usize,
        }
        unsafe extern "system" fn capture(
            _: abi::Handle,
            event: abi::Handle,
            context: *mut core::ffi::c_void,
        ) {
            let context = unsafe { &mut *(context as *mut Context) };
            context.event = event as usize;
            context.result = context.engine.event_result(event as usize).unwrap();
        }
        let (e, _, jobs, _, generation) = active();
        let mut context = Context {
            engine: e.clone(),
            event: 0,
            result: 0,
        };
        e.save_callback(
            7,
            CallbackKind::Recognized,
            Some(SavedCallback {
                function: capture as *const () as usize,
                context: &mut context as *mut Context as usize,
            }),
        );
        e.receive(BridgeEvent::Started {
            reco: 7,
            generation,
            session: "s".into(),
        });
        e.dispatch(jobs.recv().unwrap());
        e.receive(BridgeEvent::Final {
            reco: 7,
            generation,
            text: "结果".into(),
            offset: 0,
            duration: 1,
        });
        for _ in 0..3 {
            e.dispatch(jobs.recv().unwrap());
        }
        assert!(e.event(context.event).is_err());
        assert_eq!(e.result(context.result).unwrap().text, "结果");
        assert_eq!(e.release_result(context.result), abi::OK);
    }

    #[test]
    fn seed_only_fills_unobserved_callbacks_and_never_revives_explicit_none() {
        let (e, _, _) = Engine::new();
        let callback = SavedCallback {
            function: 7,
            context: 9,
        };
        e.save_callback(7, CallbackKind::Recognized, None);
        e.seed_callbacks(7, [Some(callback); 7]);
        {
            let s = lock(&e.state);
            let r = s.recos.get(&7).unwrap();
            assert_eq!(r.callbacks[CallbackKind::Recognized as usize], None);
            assert_eq!(r.callbacks[CallbackKind::Started as usize], Some(callback));
            assert_eq!(r.observed, 0x7f);
        }
        e.seed_callbacks(
            7,
            [Some(SavedCallback {
                function: 70,
                context: 90,
            }); 7],
        );
        let s = lock(&e.state);
        let r = s.recos.get(&7).unwrap();
        assert_eq!(r.callbacks[CallbackKind::Recognized as usize], None);
        assert_eq!(r.callbacks[CallbackKind::Started as usize], Some(callback));
    }

    #[test]
    fn decline_is_successful_async_without_sending_any_backend_request() {
        let (e, requests, jobs) = Engine::new();
        let callback = Some(SavedCallback {
            function: 1,
            context: 0,
        });
        e.save_callback(7, CallbackKind::Canceled, callback);
        e.save_callback(7, CallbackKind::Stopped, callback);
        let start = e.decline_start(7).unwrap();
        assert_eq!(e.completion(start).unwrap().wait(0), abi::OK);
        assert!(requests.try_recv().is_err());
        assert!(!e.has_final_callback(7));
        assert!(lock(&e.state).recos.get(&7).unwrap().phase == Phase::Failed);
        let canceled = jobs.recv().unwrap();
        let stopped = jobs.recv().unwrap();
        assert!(matches!(canceled.kind, CallbackKind::Canceled));
        assert!(matches!(stopped.kind, CallbackKind::Stopped));
        e.save_callback(7, CallbackKind::Canceled, None);
        e.save_callback(7, CallbackKind::Stopped, None);
        e.dispatch(canceled);
        e.dispatch(stopped);
        let stop = e.stop(7).unwrap();
        assert_eq!(e.completion(stop).unwrap().wait(0), abi::OK);
        assert!(requests.try_recv().is_err());
        e.release_async(start);
        e.release_async(stop);
        assert!(e.suspend_if_idle());
        assert_eq!(e.decline_start(7), Err(abi::NOT_IMPL));
    }

    #[test]
    fn repeated_stop_while_draining_shares_completion_and_sends_no_second_request() {
        let (e, requests, jobs, start, generation) = active();
        e.receive(BridgeEvent::Started {
            reco: 7,
            generation,
            session: "s".into(),
        });
        e.dispatch(jobs.recv().unwrap());
        let first = e.stop(7).unwrap();
        requests.recv().unwrap();
        e.receive(BridgeEvent::Stopped {
            reco: 7,
            generation,
        });
        let second = e.stop(7).unwrap();
        assert!(Arc::ptr_eq(
            &e.completion(first).unwrap(),
            &e.completion(second).unwrap()
        ));
        assert!(requests.try_recv().is_err());
        assert_eq!(e.completion(second).unwrap().wait(0), abi::TIMEOUT);
        e.dispatch(jobs.recv().unwrap());
        assert_eq!(e.completion(first).unwrap().wait(0), abi::OK);
        assert_eq!(e.completion(second).unwrap().wait(0), abi::OK);
        for h in [start, first, second] {
            e.release_async(h);
        }
        assert!(e.suspend_if_idle());
    }

    #[test]
    fn late_stop_after_broker_stopped_is_completed_by_existing_terminal_job() {
        let (e, requests, jobs, _, generation) = active();
        e.receive(BridgeEvent::Started {
            reco: 7,
            generation,
            session: "s".into(),
        });
        e.dispatch(jobs.recv().unwrap());
        e.receive(BridgeEvent::Stopped {
            reco: 7,
            generation,
        });
        let stop = e.stop(7).unwrap();
        assert!(requests.try_recv().is_err());
        assert_eq!(e.completion(stop).unwrap().wait(0), abi::TIMEOUT);
        e.dispatch(jobs.recv().unwrap());
        assert_eq!(e.completion(stop).unwrap().wait(0), abi::OK);
    }

    #[test]
    fn full_queue_does_not_drop_stopped_or_leave_permanent_draining_state() {
        let (e, requests, jobs, start, generation) = active();
        e.receive(BridgeEvent::Started {
            reco: 7,
            generation,
            session: "s".into(),
        });
        e.dispatch(jobs.recv().unwrap());
        let stop = e.stop(7).unwrap();
        requests.recv().unwrap();
        for _ in 0..127 {
            // first partial adds SpeechStart: exactly 128 queued jobs
            e.receive(BridgeEvent::Partial {
                reco: 7,
                generation,
                text: "x".into(),
                offset: 0,
                duration: 1,
            });
        }
        e.receive(BridgeEvent::Stopped {
            reco: 7,
            generation,
        });
        assert_eq!(e.completion(stop).unwrap().wait(0), abi::TIMEOUT);
        while let Ok(job) = jobs.try_recv() {
            e.dispatch(job);
        }
        assert_eq!(e.completion(stop).unwrap().wait(0), abi::OK);
        assert!(lock(&e.state).recos.get(&7).unwrap().phase == Phase::Idle);
        assert!(lock(&e.fallback).is_empty());
        assert_eq!(e.pending.load(Ordering::SeqCst), 0);
        e.release_async(start);
        e.release_async(stop);
        assert!(e.suspend_if_idle());
    }

    #[test]
    fn stopped_callback_can_restart_without_old_cleanup_clobbering_new_session() {
        struct Context {
            engine: Arc<Engine>,
            previous_stop: Arc<Completion>,
            result: Option<Result<usize, Status>>,
            previous_drained_inside: bool,
        }
        unsafe extern "system" fn restart(
            reco: abi::Handle,
            _: abi::Handle,
            context: *mut core::ffi::c_void,
        ) {
            let context = unsafe { &mut *(context as *mut Context) };
            // A redundant Stop while idle must not finish the previous Stop.
            let redundant = context.engine.stop(reco as usize).unwrap();
            context.engine.release_async(redundant);
            context.result = Some(context.engine.start(
                reco as usize,
                "zh-CN".into(),
                String::new(),
            ));
            context.previous_drained_inside = lock(&context.previous_stop.values).drained.is_some();
        }
        let (e, requests, jobs, _, generation) = active();
        e.receive(BridgeEvent::Started {
            reco: 7,
            generation,
            session: "old".into(),
        });
        e.dispatch(jobs.recv().unwrap());
        let stop = e.stop(7).unwrap();
        requests.recv().unwrap();
        let mut context = Context {
            engine: e.clone(),
            previous_stop: e.completion(stop).unwrap(),
            result: None,
            previous_drained_inside: true,
        };
        e.save_callback(
            7,
            CallbackKind::Stopped,
            Some(SavedCallback {
                function: restart as *const () as usize,
                context: &mut context as *mut Context as usize,
            }),
        );
        e.receive(BridgeEvent::Stopped {
            reco: 7,
            generation,
        });
        assert_eq!(e.completion(stop).unwrap().wait(0), abi::TIMEOUT);
        e.dispatch(jobs.recv().unwrap());
        let restarted = context.result.unwrap().unwrap();
        assert!(!context.previous_drained_inside);
        assert_eq!(e.completion(stop).unwrap().wait(0), abi::OK);
        let BridgeRequest::Start {
            generation: next, ..
        } = requests.recv().unwrap()
        else {
            panic!()
        };
        assert_ne!(next, generation);
        assert!(lock(&e.state).recos.get(&7).unwrap().phase == Phase::Starting);
        e.receive(BridgeEvent::Stopped {
            reco: 7,
            generation,
        });
        e.receive(BridgeEvent::Started {
            reco: 7,
            generation: next,
            session: "new".into(),
        });
        assert_eq!(e.completion(restarted).unwrap().wait(0), abi::OK);
        assert!(lock(&e.state).recos.get(&7).unwrap().phase == Phase::Running);
    }

    #[test]
    fn disconnected_cancel_callback_can_stop_and_wait_reentrantly() {
        struct Context {
            engine: Arc<Engine>,
            status: Status,
        }
        unsafe extern "system" fn canceled(
            reco: abi::Handle,
            _: abi::Handle,
            context: *mut core::ffi::c_void,
        ) {
            let context = unsafe { &mut *(context as *mut Context) };
            let stop = context.engine.stop(reco as usize).unwrap();
            context.status = context.engine.completion(stop).unwrap().wait(0);
            context.engine.release_async(stop);
        }
        let (e, requests, jobs, _, generation) = active();
        e.receive(BridgeEvent::Started {
            reco: 7,
            generation,
            session: "s".into(),
        });
        e.dispatch(jobs.recv().unwrap());
        let mut context = Context {
            engine: e.clone(),
            status: usize::MAX,
        };
        e.save_callback(
            7,
            CallbackKind::Canceled,
            Some(SavedCallback {
                function: canceled as *const () as usize,
                context: &mut context as *mut Context as usize,
            }),
        );
        e.disconnected("pipe closed");
        let retry_while_draining = e.start(7, String::new(), String::new()).unwrap();
        assert_eq!(e.completion(retry_while_draining).unwrap().wait(0), abi::OK);
        e.release_async(retry_while_draining);
        e.dispatch(jobs.recv().unwrap());
        assert_eq!(context.status, abi::OK);
        assert!(requests.try_recv().is_err());
        e.dispatch(jobs.recv().unwrap());
        assert!(lock(&e.state).recos.get(&7).unwrap().phase == Phase::Idle);
        let declined = e.start(7, String::new(), String::new()).unwrap();
        assert_eq!(e.completion(declined).unwrap().wait(0), abi::OK);
        assert!(requests.try_recv().is_err());
    }
}
