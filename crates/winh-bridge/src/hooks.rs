//! Functions reached solely through CarbonDictation's replaced import slots.
#![allow(dead_code)] // Non-Windows builds exercise the portable state/PE tests.
use crate::abi::*;
use crate::state::{self, CallbackKind, Engine, SavedCallback};
use core::ffi::{c_char, c_int, c_void};
use std::collections::HashMap;
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

static DIAGNOSTICS: AtomicU32 = AtomicU32::new(0);
pub(crate) fn diagnostic_or(bits: u32) {
    DIAGNOSTICS.fetch_or(bits, Ordering::Relaxed);
}
pub(crate) fn reset_diagnostics() {
    DIAGNOSTICS.store(0, Ordering::Relaxed);
}
pub(crate) fn diagnostics() -> u32 {
    let ready = runtime().is_some_and(|r| r.ready.load(Ordering::Acquire));
    DIAGNOSTICS.load(Ordering::Relaxed) | u32::from(ready)
}

pub(crate) struct Runtime {
    pub core_base: usize,
    pub engine: Arc<Engine>,
    pub originals: HashMap<String, usize>,
    pub ready: AtomicBool,
}
impl Runtime {
    unsafe fn original<T: Copy>(&self, name: &str) -> Option<T> {
        self.originals.get(name).map(|p| {
            debug_assert_eq!(core::mem::size_of::<T>(), core::mem::size_of::<usize>());
            // SAFETY: callers choose the function pointer type from the verified C header.
            unsafe { core::mem::transmute_copy::<usize, T>(p) }
        })
    }
    unsafe fn language(&self, reco: Handle) -> String {
        // Every returned SDK bag/string is released by the same SDK allocator.
        unsafe {
            let Some(get_bag) = self.original::<HandleOut>("recognizer_get_property_bag") else {
                return String::new();
            };
            let Some(get) = self.original::<BagGet>("property_bag_get_string") else {
                return String::new();
            };
            let Some(free) = self.original::<FreeString>("property_bag_free_string") else {
                return String::new();
            };
            let Some(release) = self.original::<Release>("property_bag_release") else {
                return String::new();
            };
            let mut bag = INVALID;
            if get_bag(reco, &mut bag) != OK {
                return String::new();
            }
            let text = get(bag, 3001, core::ptr::null(), c"".as_ptr());
            let value = if text.is_null() {
                String::new()
            } else {
                let mut n = 0;
                while n < 128 && *text.add(n) != 0 {
                    n += 1;
                }
                if n == 128 {
                    String::new()
                } else {
                    String::from_utf8_lossy(core::slice::from_raw_parts(text.cast(), n))
                        .into_owned()
                }
            };
            if !text.is_null() {
                free(text);
            }
            release(bag);
            value
        }
    }
}

static RUNTIME: OnceLock<Mutex<Option<Arc<Runtime>>>> = OnceLock::new();
pub(crate) fn runtime() -> Option<Arc<Runtime>> {
    state::lock(RUNTIME.get_or_init(|| Mutex::new(None))).clone()
}
pub(crate) fn publish(runtime: Option<Arc<Runtime>>) {
    *state::lock(RUNTIME.get_or_init(|| Mutex::new(None))) = runtime;
}

macro_rules! rt {
    () => {
        match runtime() {
            Some(r) => r,
            None => return NOT_IMPL,
        }
    };
}
macro_rules! forward {
    ($rt:expr, $name:expr, $ty:ty, $($arg:expr),*) => {
        match unsafe { $rt.original::<$ty>($name) } {
            Some(f) => unsafe { f($($arg),*) }, None => NOT_IMPL,
        }
    };
}
macro_rules! setter {
    ($function:ident, $kind:ident) => {
        unsafe extern "system" fn $function(
            reco: Handle,
            callback: Callback,
            context: *mut c_void,
        ) -> Status {
            if reco.is_null() || reco == INVALID || state::is_fake(reco as usize) {
                return INVALID_HANDLE;
            }
            let rt = rt!();
            let status = forward!(rt, stringify!($function), Setter, reco, callback, context);
            if status == OK {
                rt.engine.save_callback(
                    reco as usize,
                    CallbackKind::$kind,
                    callback.map(|f| SavedCallback {
                        function: f as usize,
                        context: context as usize,
                    }),
                );
                if callback.is_some() {
                    match CallbackKind::$kind {
                        CallbackKind::Recognized => diagnostic_or(1 << 1),
                        CallbackKind::Recognizing => diagnostic_or(1 << 2),
                        _ => (),
                    }
                }
            }
            status
        }
    };
}
setter!(recognizer_recognized_set_callback, Recognized);
setter!(recognizer_recognizing_set_callback, Recognizing);
setter!(recognizer_session_started_set_callback, Started);
setter!(recognizer_session_stopped_set_callback, Stopped);
setter!(recognizer_canceled_set_callback, Canceled);
setter!(recognizer_speech_start_detected_set_callback, SpeechStart);
setter!(recognizer_speech_end_detected_set_callback, SpeechEnd);

unsafe fn continuous(reco: Handle, out: *mut Handle, start: bool) -> Status {
    diagnostic_or(if start { 1 << 3 } else { 1 << 10 });
    if out.is_null() {
        return INVALID_ARG;
    }
    unsafe {
        out.write(INVALID);
    }
    if reco.is_null() || reco == INVALID || state::is_fake(reco as usize) {
        return INVALID_HANDLE;
    }
    let rt = rt!();
    if !start && !rt.engine.owns_recognizer(reco as usize) {
        // A real session may predate installation. Let the SDK STOP that session;
        // its real async/event/result handles continue through the forwarding path.
        diagnostic_or(1 << 13);
        return forward!(
            rt,
            "recognizer_stop_continuous_recognition_async",
            AsyncStart,
            reco,
            out
        );
    }
    if !rt.ready.load(Ordering::Acquire) {
        diagnostic_or(1 << 9);
        return match rt.engine.decline_start(reco as usize) {
            Ok(handle) => {
                unsafe {
                    out.write(handle as Handle);
                }
                OK
            }
            Err(error) => error,
        };
    }
    #[cfg(windows)]
    if start && rt.engine.needs_callback_seed(reco as usize) {
        if let Ok(callbacks) = crate::recover::callbacks(rt.core_base, reco as usize) {
            rt.engine.seed_callbacks(reco as usize, callbacks);
            diagnostic_or(1 << 11);
        } else {
            diagnostic_or(1 << 12);
        }
    }
    if start && !rt.engine.has_final_callback(reco as usize) {
        diagnostic_or(1 << 4);
    }
    let handle = if start {
        rt.engine
            .start(reco as usize, unsafe { rt.language(reco) }, String::new())
    } else {
        rt.engine.stop(reco as usize)
    };
    match handle {
        Ok(handle) => {
            unsafe {
                out.write(handle as Handle);
            }
            OK
        }
        Err(status) => {
            diagnostic_or(1 << 6);
            if start && status == NOT_IMPL {
                match rt.engine.decline_start(reco as usize) {
                    Ok(handle) => {
                        unsafe {
                            out.write(handle as Handle);
                        }
                        OK
                    }
                    Err(error) => error,
                }
            } else {
                status
            }
        }
    }
}
unsafe extern "system" fn recognizer_start_continuous_recognition_async(
    reco: Handle,
    out: *mut Handle,
) -> Status {
    unsafe { continuous(reco, out, true) }
}
unsafe extern "system" fn recognizer_stop_continuous_recognition_async(
    reco: Handle,
    out: *mut Handle,
) -> Status {
    unsafe { continuous(reco, out, false) }
}
unsafe fn synchronous(reco: Handle, start: bool) -> Status {
    let mut handle = INVALID;
    let status = unsafe { continuous(reco, &mut handle, start) };
    if status != OK {
        return status;
    }
    let rt = rt!();
    let status = rt
        .engine
        .completion(handle as usize)
        .map_or_else(|s| s, |c| c.wait(u32::MAX));
    rt.engine.release_async(handle as usize);
    status
}
unsafe extern "system" fn recognizer_start_continuous_recognition(reco: Handle) -> Status {
    unsafe { synchronous(reco, true) }
}
unsafe extern "system" fn recognizer_stop_continuous_recognition(reco: Handle) -> Status {
    unsafe { synchronous(reco, false) }
}

macro_rules! wait {
    ($name:ident) => {
        unsafe extern "system" fn $name(handle: Handle, milliseconds: u32) -> Status {
            let rt = rt!();
            if state::is_fake(handle as usize) {
                rt.engine
                    .completion(handle as usize)
                    .map_or_else(|s| s, |c| c.wait(milliseconds))
            } else {
                forward!(rt, stringify!($name), Wait, handle, milliseconds)
            }
        }
    };
}
wait!(recognizer_start_continuous_recognition_async_wait_for);
wait!(recognizer_stop_continuous_recognition_async_wait_for);
wait!(recognizer_start_keyword_recognition_async_wait_for);
wait!(recognizer_stop_keyword_recognition_async_wait_for);

macro_rules! release {
    ($name:ident, $method:ident) => {
        unsafe extern "system" fn $name(handle: Handle) -> Status {
            let rt = rt!();
            if state::is_fake(handle as usize) {
                rt.engine.$method(handle as usize)
            } else {
                forward!(rt, stringify!($name), Release, handle)
            }
        }
    };
}
release!(recognizer_async_handle_release, release_async);
release!(recognizer_event_handle_release, release_event);
release!(recognizer_result_handle_release, release_result);

macro_rules! valid {
    ($name:ident, $method:ident) => {
        unsafe extern "system" fn $name(handle: Handle) -> bool {
            let Some(rt) = runtime() else {
                return false;
            };
            if state::is_fake(handle as usize) {
                rt.engine.$method(handle as usize).is_ok()
            } else {
                unsafe {
                    rt.original::<Valid>(stringify!($name))
                        .is_some_and(|f| f(handle))
                }
            }
        }
    };
}
valid!(recognizer_async_handle_is_valid, completion);
valid!(recognizer_event_handle_is_valid, event);
valid!(recognizer_result_handle_is_valid, result);

unsafe extern "system" fn recognizer_handle_release(handle: Handle) -> Status {
    if state::is_fake(handle as usize) {
        return INVALID_HANDLE;
    }
    let rt = rt!();
    rt.engine.forget_recognizer(handle as usize);
    forward!(rt, "recognizer_handle_release", Release, handle)
}
unsafe extern "system" fn recognizer_recognition_event_get_result(
    handle: Handle,
    out: *mut Handle,
) -> Status {
    if out.is_null() {
        return INVALID_ARG;
    }
    let rt = rt!();
    if !state::is_fake(handle as usize) {
        return forward!(
            rt,
            "recognizer_recognition_event_get_result",
            HandleOut,
            handle,
            out
        );
    }
    unsafe {
        out.write(INVALID);
    }
    match rt.engine.event_result(handle as usize) {
        Ok(h) => {
            unsafe {
                out.write(h as Handle);
            }
            OK
        }
        Err(s) => s,
    }
}

macro_rules! scalar {
    ($name:ident, $method:ident, $ty:ty, $abi:ty, $get:expr) => {
        unsafe extern "system" fn $name(handle: Handle, out: *mut $ty) -> Status {
            if out.is_null() {
                return INVALID_ARG;
            }
            let rt = rt!();
            if !state::is_fake(handle as usize) {
                return forward!(rt, stringify!($name), $abi, handle, out);
            }
            match rt.engine.$method(handle as usize) {
                Ok(p) => {
                    unsafe {
                        out.write(($get)(&*p));
                    }
                    OK
                }
                Err(s) => s,
            }
        }
    };
}
scalar!(
    recognizer_recognition_event_get_offset,
    event,
    u64,
    U64Out,
    |p: &state::Payload| p.offset
);
scalar!(
    result_get_reason,
    result,
    c_int,
    IntOut,
    |p: &state::Payload| p.reason
);
scalar!(
    result_get_offset,
    result,
    u64,
    U64Out,
    |p: &state::Payload| p.offset
);
scalar!(
    result_get_duration,
    result,
    u64,
    U64Out,
    |p: &state::Payload| p.duration
);
scalar!(
    result_get_channel,
    result,
    u32,
    U32Out,
    |_: &state::Payload| 0
);
scalar!(
    result_get_reason_canceled,
    result,
    c_int,
    IntOut,
    |p: &state::Payload| if p.error_code == 0 {
        CANCEL_USER
    } else {
        CANCEL_ERROR
    }
);
scalar!(
    result_get_canceled_error_code,
    result,
    c_int,
    IntOut,
    |p: &state::Payload| if p.error_code == 0 { 0 } else { 9 }
); // CancellationErrorCode_RuntimeError

macro_rules! string {
    ($name:ident, $method:ident, $field:ident) => {
        unsafe extern "system" fn $name(handle: Handle, out: *mut c_char, capacity: u32) -> Status {
            let rt = rt!();
            if !state::is_fake(handle as usize) {
                return forward!(rt, stringify!($name), StringOut, handle, out, capacity);
            }
            match rt.engine.$method(handle as usize) {
                Ok(p) => unsafe { crate::abi::copy_string(&p.$field, out, capacity) },
                Err(s) => s,
            }
        }
    };
}
string!(recognizer_session_event_get_session_id, event, session);
string!(result_get_text, result, text);
string!(result_get_result_id, result, result_id);

unsafe extern "system" fn result_get_property_bag(handle: Handle, out: *mut Handle) -> Status {
    if out.is_null() {
        return INVALID_ARG;
    }
    let rt = rt!();
    if !state::is_fake(handle as usize) {
        return forward!(rt, "result_get_property_bag", HandleOut, handle, out);
    }
    unsafe {
        out.write(INVALID);
    }
    let payload = match rt.engine.result(handle as usize) {
        Ok(p) => p,
        Err(s) => return s,
    };
    // Fresh real bags give each SDK wrapper independent ownership. Never return a
    // Rust CString or the same SDK owning handle twice to CarbonDictation.
    unsafe {
        let Some(create) = rt.original::<BagCreate>("property_bag_create") else {
            return NOT_IMPL;
        };
        let Some(set) = rt.original::<BagSet>("property_bag_set_string") else {
            return NOT_IMPL;
        };
        let Some(release) = rt.original::<Release>("property_bag_release") else {
            return NOT_IMPL;
        };
        let mut bag = INVALID;
        let status = create(&mut bag);
        if status != OK {
            return status;
        }
        for (id, value) in [(3002, payload.session.clone()), (5000, payload.json())] {
            let Ok(value) = CString::new(value) else {
                release(bag);
                return INVALID_ARG;
            };
            let status = set(bag, id, core::ptr::null(), value.as_ptr());
            if status != OK {
                release(bag);
                return status;
            }
        }
        out.write(bag);
    }
    OK
}

// These start branches are intentionally unsupported. They NEVER call the SDK,
// even for a real recognizer, so a new branch cannot silently use Microsoft ASR.
unsafe fn blocked_output(out: *mut Handle) -> Status {
    if out.is_null() {
        return INVALID_ARG;
    }
    unsafe {
        out.write(INVALID);
    }
    NOT_IMPL
}
unsafe extern "system" fn recognizer_recognize_once(_: Handle, out: *mut Handle) -> Status {
    diagnostic_or(1 << 5);
    unsafe { blocked_output(out) }
}
unsafe extern "system" fn recognizer_recognize_once_async(h: Handle, out: *mut Handle) -> Status {
    unsafe { recognizer_recognize_once(h, out) }
}
unsafe extern "system" fn recognizer_recognize_once_async_wait_for(
    handle: Handle,
    milliseconds: u32,
    out: *mut Handle,
) -> Status {
    if out.is_null() {
        return INVALID_ARG;
    }
    let rt = rt!();
    if state::is_fake(handle as usize) {
        unsafe {
            out.write(INVALID);
        }
        return INVALID_HANDLE;
    }
    type OnceWait = unsafe extern "system" fn(Handle, u32, *mut Handle) -> Status;
    forward!(
        rt,
        "recognizer_recognize_once_async_wait_for",
        OnceWait,
        handle,
        milliseconds,
        out
    )
}
unsafe extern "system" fn recognizer_recognize_text_once_async(
    _: Handle,
    _: *const c_char,
    out: *mut Handle,
) -> Status {
    unsafe { recognizer_recognize_once(core::ptr::null_mut(), out) }
}
unsafe extern "system" fn recognizer_start_keyword_recognition_async(
    _: Handle,
    _: Handle,
    out: *mut Handle,
) -> Status {
    diagnostic_or(1 << 8);
    unsafe { blocked_output(out) }
}
unsafe extern "system" fn recognizer_start_keyword_recognition(_: Handle, _: Handle) -> Status {
    diagnostic_or(1 << 8);
    NOT_IMPL
}
unsafe extern "system" fn recognizer_stop_keyword_recognition_async(
    _: Handle,
    out: *mut Handle,
) -> Status {
    diagnostic_or(1 << 8);
    unsafe { blocked_output(out) }
}
unsafe extern "system" fn recognizer_stop_keyword_recognition(_: Handle) -> Status {
    diagnostic_or(1 << 8);
    NOT_IMPL
}
unsafe extern "system" fn recognizer_recognize_keyword_once(
    _: Handle,
    _: Handle,
    out: *mut Handle,
) -> Status {
    diagnostic_or(1 << 8);
    unsafe { blocked_output(out) }
}
unsafe extern "system" fn recognizer_recognize_keyword_once_async(
    _: Handle,
    _: Handle,
    out: *mut Handle,
) -> Status {
    diagnostic_or(1 << 8);
    unsafe { blocked_output(out) }
}

macro_rules! addresses {
    ($($name:ident),* $(,)?) => {
        pub(crate) fn address(name: &str) -> Option<usize> {
            match name { $(stringify!($name) => Some($name as *const () as usize),)* _ => None }
        }
    };
}
addresses!(
    recognizer_recognized_set_callback,
    recognizer_recognizing_set_callback,
    recognizer_session_started_set_callback,
    recognizer_session_stopped_set_callback,
    recognizer_canceled_set_callback,
    recognizer_speech_start_detected_set_callback,
    recognizer_speech_end_detected_set_callback,
    recognizer_start_continuous_recognition_async,
    recognizer_stop_continuous_recognition_async,
    recognizer_start_continuous_recognition_async_wait_for,
    recognizer_stop_continuous_recognition_async_wait_for,
    recognizer_start_continuous_recognition,
    recognizer_stop_continuous_recognition,
    recognizer_async_handle_is_valid,
    recognizer_async_handle_release,
    recognizer_event_handle_is_valid,
    recognizer_event_handle_release,
    recognizer_result_handle_is_valid,
    recognizer_result_handle_release,
    recognizer_handle_release,
    recognizer_recognition_event_get_result,
    recognizer_recognition_event_get_offset,
    recognizer_session_event_get_session_id,
    result_get_reason,
    result_get_offset,
    result_get_duration,
    result_get_channel,
    result_get_reason_canceled,
    result_get_canceled_error_code,
    result_get_text,
    result_get_result_id,
    result_get_property_bag,
    recognizer_recognize_once,
    recognizer_recognize_once_async,
    recognizer_recognize_once_async_wait_for,
    recognizer_recognize_text_once_async,
    recognizer_start_keyword_recognition,
    recognizer_start_keyword_recognition_async,
    recognizer_start_keyword_recognition_async_wait_for,
    recognizer_stop_keyword_recognition,
    recognizer_stop_keyword_recognition_async,
    recognizer_stop_keyword_recognition_async_wait_for,
    recognizer_recognize_keyword_once,
    recognizer_recognize_keyword_once_async,
);

pub(crate) const REQUIRED: &[&str] = &[
    "recognizer_recognized_set_callback",
    "recognizer_recognizing_set_callback",
    "recognizer_session_started_set_callback",
    "recognizer_session_stopped_set_callback",
    "recognizer_canceled_set_callback",
    "recognizer_speech_start_detected_set_callback",
    "recognizer_speech_end_detected_set_callback",
    "recognizer_start_continuous_recognition_async",
    "recognizer_stop_continuous_recognition_async",
    "recognizer_start_continuous_recognition_async_wait_for",
    "recognizer_stop_continuous_recognition_async_wait_for",
    "recognizer_async_handle_is_valid",
    "recognizer_async_handle_release",
    "recognizer_handle_release",
    "recognizer_event_handle_is_valid",
    "recognizer_event_handle_release",
    "recognizer_result_handle_release",
    "recognizer_recognition_event_get_result",
    "recognizer_recognition_event_get_offset",
    "recognizer_session_event_get_session_id",
    "result_get_reason",
    "result_get_text",
    "result_get_result_id",
    "result_get_offset",
    "result_get_duration",
    "result_get_channel",
    "result_get_reason_canceled",
    "result_get_canceled_error_code",
    "result_get_property_bag",
    "recognizer_recognize_once",
    "recognizer_start_keyword_recognition_async",
    "recognizer_start_keyword_recognition_async_wait_for",
    "recognizer_stop_keyword_recognition_async",
    "recognizer_stop_keyword_recognition_async_wait_for",
];

pub(crate) fn is_start_or_stop(name: &str) -> bool {
    name.starts_with("recognizer_start_")
        || name.starts_with("recognizer_recognize_")
        || name.starts_with("recognizer_stop_")
}
