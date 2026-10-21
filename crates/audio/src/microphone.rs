//! WASAPI shared capture. Silent packets are handled without reading null buffers.
use std::{error::Error, slice};
use windows::{
    Win32::{
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
        Foundation::{CloseHandle, HANDLE, WAIT_FAILED},
        Media::Audio::{
            AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE_ACTIVE,
            Endpoints::IAudioEndpointVolume, IAudioCaptureClient, IAudioClient, IMMDevice,
            IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, eCapture, eCommunications,
            eConsole, eMultimedia,
        },
        System::{
            Com::{
                CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
                CoUninitialize, STGM_READ,
                StructuredStorage::{PropVariantClear, PropVariantToStringAlloc},
            },
            Threading::{CreateEventW, WaitForSingleObject},
        },
    },
    core::HSTRING,
};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}
struct Event(HANDLE);
impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub struct Microphone {
    pub device: Device,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Event,
    silence: Vec<i16>,
    _apartment: Apartment,
}

#[derive(Debug, Clone)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub muted: Option<bool>,
    pub volume: Option<f32>,
}

fn describe(device: &IMMDevice) -> Result<Device> {
    let id = unsafe { device.GetId()? };
    let id_text = unsafe { id.to_string() };
    unsafe {
        CoTaskMemFree(Some(id.0.cast()));
    }
    let property = unsafe { device.OpenPropertyStore(STGM_READ)? };
    let mut value = unsafe { property.GetValue(&PKEY_Device_FriendlyName)? };
    let name = unsafe { PropVariantToStringAlloc(&value) };
    unsafe {
        PropVariantClear(&mut value)?;
    }
    let name = name?;
    let text = unsafe { name.to_string() };
    unsafe {
        CoTaskMemFree(Some(name.0.cast()));
    }
    let volume: Option<IAudioEndpointVolume> = unsafe { device.Activate(CLSCTX_ALL, None) }.ok();
    Ok(Device {
        id: id_text?,
        name: text?,
        muted: volume
            .as_ref()
            .and_then(|v| unsafe { v.GetMute().ok() })
            .map(|v| v.as_bool()),
        volume: volume
            .as_ref()
            .and_then(|v| unsafe { v.GetMasterVolumeLevelScalar().ok() }),
    })
}

pub fn devices() -> Result<Vec<(String, Device)>> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    }
    let _apartment = Apartment;
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let mut output = Vec::new();
    for (name, role) in [
        ("console", eConsole),
        ("communications", eCommunications),
        ("multimedia", eMultimedia),
    ] {
        if let Ok(device) = unsafe { enumerator.GetDefaultAudioEndpoint(eCapture, role) } {
            output.push((name.to_owned(), describe(&device)?));
        }
    }
    let all = unsafe { enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)? };
    for index in 0..unsafe { all.GetCount()? }.min(32) {
        output.push((
            "available".to_owned(),
            describe(&unsafe { all.Item(index)? })?,
        ));
    }
    Ok(output)
}

impl Microphone {
    pub fn open(device_id: Option<&str>) -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        }
        let apartment = Apartment;
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let device: IMMDevice = match device_id.filter(|s| !s.is_empty() && *s != "default") {
            Some(id) => unsafe { enumerator.GetDevice(&HSTRING::from(id))? },
            None => unsafe { enumerator.GetDefaultAudioEndpoint(eCapture, eConsole)? },
        };
        let description = describe(&device)?;
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
        let format = WAVEFORMATEX {
            wFormatTag: 1,
            nChannels: 1,
            nSamplesPerSec: 16_000,
            nAvgBytesPerSec: 32_000,
            nBlockAlign: 2,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                2_000_000,
                0,
                &format,
                None,
            )?;
        }
        let event = Event(unsafe { CreateEventW(None, false, false, None)? });
        unsafe {
            client.SetEventHandle(event.0)?;
        }
        let capture: IAudioCaptureClient = unsafe { client.GetService()? };
        unsafe {
            client.Start()?;
        }
        Ok(Self {
            device: description,
            client,
            capture,
            event,
            silence: Vec::with_capacity(3200),
            _apartment: apartment,
        })
    }

    pub fn poll(&mut self, mut consume: impl FnMut(&[i16]) -> Result<()>) -> Result<()> {
        let wait = unsafe { WaitForSingleObject(self.event.0, 50) };
        if wait == WAIT_FAILED {
            return Err(windows::core::Error::from_thread().into());
        }
        loop {
            if unsafe { self.capture.GetNextPacketSize()? } == 0 {
                break;
            }
            let mut pointer = std::ptr::null_mut();
            let mut frames = 0;
            let mut flags = 0;
            unsafe {
                self.capture
                    .GetBuffer(&mut pointer, &mut frames, &mut flags, None, None)?;
            }
            let result = if frames > 16_000 {
                Err("microphone packet exceeds one second".into())
            } else if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                self.silence.resize(frames as usize, 0);
                consume(&self.silence)
            } else if pointer.is_null() && frames != 0 {
                Err("microphone returned an empty data pointer".into())
            } else if frames == 0 {
                Ok(())
            } else {
                consume(unsafe { slice::from_raw_parts(pointer.cast::<i16>(), frames as usize) })
            };
            let release = unsafe { self.capture.ReleaseBuffer(frames) };
            result?;
            release?;
        }
        Ok(())
    }
}

impl Drop for Microphone {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}
