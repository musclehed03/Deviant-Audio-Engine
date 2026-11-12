//! C ABI v1. Handles are validated integer IDs, never Rust pointers.
//! Calls are serialized. Caller buffers must remain valid for each call and may
//! not alias other pointer arguments. No Rust allocation crosses the ABI.
use crate::{
    decode::{self, NativeDecoder},
    output::{Format, SharedQueue, StreamStatus},
};
use serde_json::json;
use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::{c_char, CStr},
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
    sync::{Mutex, OnceLock},
    thread::JoinHandle,
};

#[derive(Default)]
struct Engine {
    queue: Option<SharedQueue>,
    worker: Option<JoinHandle<()>>,
    decoder: Option<NativeDecoder>,
    last_stream: StreamStatus,
}
impl Engine {
    fn close(&mut self) {
        if let Some(q) = &self.queue {
            q.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        }
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        if let Some(q) = self.queue.take() {
            let mut q = q.lock().unwrap_or_else(|e| e.into_inner());
            q.status.active = false;
            q.status.exclusive = false;
            q.status.pending_device_frames = 0;
            q.samples.clear();
            self.last_stream = q.snapshot();
        }
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        self.close();
    }
}
#[derive(Default)]
struct Registry {
    next: u64,
    engines: HashMap<u64, Engine>,
}
static ENGINES: OnceLock<Mutex<Registry>> = OnceLock::new();
thread_local! { static ERROR: RefCell<String> = const { RefCell::new(String::new()) }; }
fn set_error(e: &str) {
    ERROR.with(|s| *s.borrow_mut() = e.replace('\0', "?"));
}
fn guard<T: Default>(f: impl FnOnce() -> Result<T, String>) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            set_error(&e);
            T::default()
        }
        Err(_) => {
            set_error("internal panic caught at C ABI boundary");
            T::default()
        }
    }
}
fn status(f: impl FnOnce() -> Result<(), String>) -> i32 {
    if guard(|| {
        f()?;
        Ok(true)
    }) {
        0
    } else {
        -1
    }
}
fn with_engine<T>(id: u64, f: impl FnOnce(&mut Engine) -> Result<T, String>) -> Result<T, String> {
    let mut r = ENGINES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    f(r.engines.get_mut(&id).ok_or("invalid engine handle")?)
}
unsafe fn text<'a>(p: *const c_char) -> Result<&'a str, String> {
    if p.is_null() {
        return Err("null string".into());
    }
    CStr::from_ptr(p)
        .to_str()
        .map_err(|_| "string must be UTF-8".into())
}
unsafe fn copy_text(s: &str, out: *mut c_char, capacity: usize) -> usize {
    let needed = s.len() + 1;
    if !out.is_null() && capacity >= needed {
        std::ptr::copy_nonoverlapping(s.as_ptr(), out.cast(), s.len());
        *out.add(s.len()) = 0;
    }
    needed
}

#[no_mangle]
pub extern "C" fn dae_init() -> u64 {
    guard(|| {
        let mut r = ENGINES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        r.next = r.next.checked_add(1).ok_or("handle IDs exhausted")?;
        let id = r.next;
        r.engines.insert(id, Engine::default());
        Ok(id)
    })
}
#[no_mangle]
pub extern "C" fn dae_shutdown(id: u64) -> i32 {
    status(|| {
        let engine = ENGINES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .engines
            .remove(&id)
            .ok_or("invalid engine handle")?;
        drop(engine);
        Ok(())
    })
}
/// # Safety
/// `out` must be null or writable for `capacity` bytes.
#[no_mangle]
pub unsafe extern "C" fn dae_last_error(out: *mut c_char, capacity: usize) -> usize {
    guard(|| Ok(ERROR.with(|s| copy_text(&s.borrow(), out, capacity))))
}
/// # Safety
/// `out` must be null or writable for `capacity` bytes.
#[no_mangle]
pub unsafe extern "C" fn dae_devices_json(out: *mut c_char, capacity: usize) -> usize {
    guard(|| {
        #[cfg(windows)]
        let devices = crate::windows::devices()?;
        #[cfg(not(windows))]
        let devices: Vec<crate::output::DeviceInfo> = Vec::new();
        let s = serde_json::to_string(&devices).map_err(|e| e.to_string())?;
        Ok(copy_text(&s, out, capacity))
    })
}
/// # Safety
/// `device_id` must be null (default endpoint) or a valid NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn dae_open_exclusive(
    id: u64,
    device_id: *const c_char,
    sample_rate: u32,
    channels: u16,
    container_bits: u16,
    valid_bits: u16,
    float_pcm: u8,
) -> i32 {
    status(|| {
        if float_pcm > 1 {
            return Err("float_pcm must be 0 or 1".into());
        }
        let requested = Format {
            sample_rate,
            channels,
            container_bits,
            valid_bits,
            float: float_pcm == 1,
        }
        .validate()?;
        let device_id = if device_id.is_null() {
            String::new()
        } else {
            text(device_id)?.to_owned()
        };
        with_engine(id, |engine| {
            engine.close();
            engine.last_stream = StreamStatus::default();
            #[cfg(windows)]
            let opened = crate::windows::start(device_id, requested);
            #[cfg(not(windows))]
            let opened: Result<(SharedQueue, JoinHandle<()>), String> = {
                let _ = (device_id, requested);
                Err(
                    "WASAPI exclusive output is only available on Windows; no shared stream opened"
                        .into(),
                )
            };
            match opened {
                Ok((q, w)) => {
                    engine.queue = Some(q);
                    engine.worker = Some(w);
                    Ok(())
                }
                Err(e) => {
                    engine.last_stream.error = Some(e.clone());
                    Err(e)
                }
            }
        })
    })
}
#[no_mangle]
pub extern "C" fn dae_close_stream(id: u64) -> i32 {
    status(|| {
        with_engine(id, |e| {
            e.close();
            Ok(())
        })
    })
}
/// # Safety
/// `samples` must address `sample_count` readable f32 values, including for zero
/// length. The memory must not be mutated concurrently.
#[no_mangle]
pub unsafe extern "C" fn dae_feed_frames(
    id: u64,
    samples: *const f32,
    sample_count: usize,
    rate: u32,
    channels: u16,
) -> i32 {
    status(|| {
        if samples.is_null() || sample_count > 16_777_216 {
            return Err("null/oversize PCM buffer".into());
        }
        let pcm = std::slice::from_raw_parts(samples, sample_count);
        with_engine(id, |e| {
            e.queue
                .as_ref()
                .ok_or("no output stream")?
                .lock()
                .map_err(|_| "output queue poisoned".to_string())?
                .feed(pcm, rate, channels)
        })
    })
}
/// # Safety
/// `path` must address a NUL-terminated UTF-8 local filesystem path.
#[no_mangle]
pub unsafe extern "C" fn dae_decode_open(id: u64, path: *const c_char) -> i32 {
    status(|| {
        let path = text(path)?;
        with_engine(id, |e| {
            let d = NativeDecoder::open(Path::new(path))?;
            e.decoder = Some(d);
            Ok(())
        })
    })
}
/// # Safety
/// `out` must hold `capacity_samples` writable f32 values; `written_frames` must
/// be writable. Buffers must not overlap. Reads at most 65536 frames.
#[no_mangle]
pub unsafe extern "C" fn dae_decode_read(
    id: u64,
    out: *mut f32,
    capacity_samples: usize,
    written_frames: *mut usize,
) -> i32 {
    status(|| {
        if out.is_null() || written_frames.is_null() {
            return Err("null decode buffer".into());
        }
        *written_frames = 0;
        with_engine(id, |e| {
            let d = e.decoder.as_mut().ok_or("no decoder")?;
            let channels = d.info.channels as usize;
            let frames = (capacity_samples / channels).min(65536);
            let samples = d.read(frames)?;
            std::ptr::copy_nonoverlapping(samples.as_ptr(), out, samples.len());
            *written_frames = samples.len() / channels;
            Ok(())
        })
    })
}
#[no_mangle]
pub extern "C" fn dae_decode_close(id: u64) -> i32 {
    status(|| {
        with_engine(id, |e| {
            e.decoder = None;
            Ok(())
        })
    })
}
/// # Safety
/// Input and output buffers must address their declared numbers of f32 values
/// and must not overlap; `written_samples` must be writable and separate.
#[no_mangle]
pub unsafe extern "C" fn dae_resample(
    input: *const f32,
    sample_count: usize,
    source_rate: u32,
    target_rate: u32,
    channels: u16,
    out: *mut f32,
    capacity_samples: usize,
    written_samples: *mut usize,
) -> i32 {
    status(|| {
        if input.is_null()
            || out.is_null()
            || written_samples.is_null()
            || sample_count > 16_777_216
        {
            return Err("invalid resample buffers".into());
        }
        *written_samples = 0;
        let samples = decode::resample(
            std::slice::from_raw_parts(input, sample_count),
            source_rate,
            target_rate,
            channels,
        )?;
        if samples.len() > capacity_samples {
            return Err("resample destination too small".into());
        }
        std::ptr::copy_nonoverlapping(samples.as_ptr(), out, samples.len());
        *written_samples = samples.len();
        Ok(())
    })
}
/// # Safety
/// `out` must be null or writable for `capacity` bytes.
#[no_mangle]
pub unsafe extern "C" fn dae_capabilities_json(
    id: u64,
    out: *mut c_char,
    capacity: usize,
) -> usize {
    guard(|| {
        with_engine(id, |e| {
            let stream = e
                .queue
                .as_ref()
                .map(|q| q.lock().unwrap_or_else(|p| p.into_inner()).snapshot())
                .unwrap_or_else(|| e.last_stream.clone());
            let value = json!({
                "abi_version":1,"hardware_output_available":cfg!(windows),
                "supported_decoders":["aac-lc","flac","ogg-opus-family-0","pcm"],
                "channel_limit":2,"stream":stream,"track":e.decoder.as_ref().map(|d|&d.info),
                "output_capability_scope":"only the initialized stream format is confirmed; no exhaustive device format list",
                "sample_conversion":"interleaved f32 to negotiated PCM; integer output is rounded and clipped",
                "rate_policy":"source rate required; caller can explicitly resample offline",
                "delivery_verification":"unverified",
                "resampler":"offline linear interpolation; no antialias filter"
            });
            Ok(copy_text(&value.to_string(), out, capacity))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    #[test]
    fn handles_buffers_decode_and_capabilities() {
        unsafe {
            let id = dae_init();
            assert_ne!(id, 0);
            let size = dae_capabilities_json(id, std::ptr::null_mut(), 0);
            assert!(size > 1);
            let mut tiny = [55i8; 1];
            assert_eq!(dae_capabilities_json(id, tiny.as_mut_ptr(), 1), size);
            assert_eq!(tiny, [55]);
            let mut bytes = vec![0u8; size];
            assert_eq!(
                dae_capabilities_json(id, bytes.as_mut_ptr().cast(), size),
                size
            );
            let v: serde_json::Value = serde_json::from_slice(&bytes[..size - 1]).unwrap();
            assert_eq!(v["stream"]["exclusive"], false);
            assert!(v["track"].is_null());
            let path = CString::new(format!(
                "{}/tests/fixtures/tone.flac",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap();
            assert_eq!(dae_decode_open(id, path.as_ptr()), 0);
            let mut pcm = [0f32; 512];
            let mut frames = 0;
            assert_eq!(
                dae_decode_read(id, pcm.as_mut_ptr(), pcm.len(), &mut frames),
                0
            );
            assert_eq!(frames, 256);
            assert_eq!(dae_feed_frames(id, pcm.as_ptr(), 512, 48000, 2), -1);
            assert_eq!(
                dae_decode_read(id, std::ptr::null_mut(), 512, &mut frames),
                -1
            );
            assert_eq!(dae_shutdown(id), 0);
            assert_eq!(dae_shutdown(id), -1);
            assert_eq!(dae_capabilities_json(id, std::ptr::null_mut(), 0), 0);
            assert!(dae_last_error(std::ptr::null_mut(), 0) > 1);
        }
    }
    #[cfg(not(windows))]
    #[test]
    fn unsupported_platform_is_honest() {
        let id = dae_init();
        unsafe {
            assert_eq!(
                dae_open_exclusive(id, std::ptr::null(), 48000, 2, 16, 16, 0),
                -1
            );
        }
        with_engine(id, |e| {
            assert!(!e.last_stream.active);
            assert!(!e.last_stream.exclusive);
            assert!(e.last_stream.error.is_some());
            Ok(())
        })
        .unwrap();
        assert_eq!(dae_shutdown(id), 0);
    }
    #[test]
    fn panic_is_contained() {
        assert_eq!(guard::<usize>(|| panic!("test boundary")), 0);
    }
    #[test]
    fn mock_stream_feed_failure_and_shutdown() {
        let id = dae_init();
        let format = Format {
            sample_rate: 48000,
            channels: 2,
            container_bits: 16,
            valid_bits: 16,
            float: false,
        };
        let queue = crate::output::Queue::new(format);
        {
            let mut q = queue.lock().unwrap();
            q.status.active = true;
            q.status.exclusive = true;
        }
        with_engine(id, |e| {
            e.queue = Some(queue.clone());
            Ok(())
        })
        .unwrap();
        let samples = [0.5, -0.5];
        unsafe {
            assert_eq!(dae_feed_frames(id, samples.as_ptr(), 2, 48000, 2), 0);
        }
        let mut bytes = [0u8; 4];
        queue.lock().unwrap().render(&mut bytes);
        assert_eq!(bytes, [0, 64, 0, 192]);
        assert_eq!(dae_close_stream(id), 0);
        {
            let q = queue.lock().unwrap();
            assert!(q.stop);
            assert!(!q.status.active);
            assert!(!q.status.exclusive);
        }
        assert_eq!(dae_shutdown(id), 0);
    }
}
