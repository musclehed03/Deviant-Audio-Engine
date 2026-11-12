// SPDX-License-Identifier: GPL-3.0-only
//! Terminal hardware test using only the engine's existing public APIs.
use deviant_audio_engine::{decode, ffi};
use serde::Deserialize;
use std::{
    ffi::{c_char, CString},
    io::{self, Write},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Deserialize)]
struct Device {
    id: String,
    name: String,
}
#[derive(Clone, Debug, Deserialize)]
struct Format {
    sample_rate: u32,
    channels: u16,
    container_bits: u16,
    valid_bits: u16,
    float: bool,
}
#[derive(Debug, Deserialize)]
struct Stream {
    active: bool,
    exclusive: bool,
    format: Option<Format>,
    error: Option<String>,
    underrun_frames: u64,
    queued_frames: usize,
    pending_device_frames: usize,
}
#[derive(Deserialize)]
struct Capabilities {
    stream: Stream,
}
#[derive(Debug, Default, PartialEq)]
struct Options {
    list: bool,
    help: bool,
    device: Option<String>,
    rate: Option<u32>,
}
fn options(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let mut args = args.into_iter();
    let mut opts = Options::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--list" => opts.list = true,
            "--help" | "-h" => opts.help = true,
            "--device" => {
                if opts.device.is_some() {
                    return Err("--device was supplied twice".into());
                }
                opts.device = Some(args.next().ok_or("--device needs a number or exact name")?);
            }
            "--rate" => {
                if opts.rate.is_some() {
                    return Err("--rate was supplied twice".into());
                }
                let rate = args
                    .next()
                    .ok_or("--rate needs a sample rate")?
                    .parse::<u32>()
                    .map_err(|_| "--rate must be an integer")?;
                if !(8000..=384000).contains(&rate) {
                    return Err("--rate must be 8000..384000 Hz".into());
                }
                opts.rate = Some(rate);
            }
            _ => return Err(format!("unknown argument: {arg}; use --help")),
        }
    }
    Ok(opts)
}
fn select<'a>(devices: &'a [Device], selection: &str) -> Result<&'a Device, String> {
    if let Ok(number) = selection.parse::<usize>() {
        return number
            .checked_sub(1)
            .and_then(|i| devices.get(i))
            .ok_or_else(|| "device number is outside the displayed list".into());
    }
    let mut matches = devices.iter().filter(|d| d.name == selection);
    let first = matches.next().ok_or("no device has that exact name")?;
    if matches.next().is_some() {
        return Err("multiple devices have that name; select by number instead".into());
    }
    Ok(first)
}
// Each ABI function below receives a valid caller-owned buffer. Handle and
// buffers never escape the call, and engine shutdown is guaranteed by Drop.
fn abi_string(mut call: impl FnMut(*mut c_char, usize) -> usize) -> Result<String, String> {
    for _ in 0..8 {
        let size = call(std::ptr::null_mut(), 0);
        if size == 0 {
            return Err("engine could not return text".into());
        }
        if size > 16 * 1024 * 1024 {
            return Err("engine text response exceeds test limit".into());
        }
        let mut buf = vec![0u8; size];
        let required = call(buf.as_mut_ptr().cast(), buf.len());
        if required == 0 {
            return Err("engine text query failed".into());
        }
        if required <= buf.len() {
            if buf[required - 1] != 0 {
                return Err("engine text was not NUL terminated".into());
            }
            return String::from_utf8(buf[..required - 1].to_vec()).map_err(|e| e.to_string());
        }
    }
    Err("engine text size kept changing; retry the test".into())
}
fn last_error() -> String {
    abi_string(|p, n| unsafe { ffi::dae_last_error(p, n) })
        .unwrap_or_else(|_| "engine error unavailable".into())
}
fn check(code: i32) -> Result<(), String> {
    if code == 0 {
        Ok(())
    } else {
        Err(last_error())
    }
}
struct Engine(u64);
impl Engine {
    fn new() -> Result<Self, String> {
        let id = ffi::dae_init();
        if id == 0 {
            Err(last_error())
        } else {
            Ok(Self(id))
        }
    }
    fn stream(&self) -> Result<Stream, String> {
        let text = abi_string(|p, n| unsafe { ffi::dae_capabilities_json(self.0, p, n) })
            .map_err(|e| format!("{e}: {}", last_error()))?;
        serde_json::from_str::<Capabilities>(&text)
            .map(|c| c.stream)
            .map_err(|e| e.to_string())
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        let code = ffi::dae_shutdown(self.0);
        if code != 0 {
            eprintln!("Shutdown failed: {}", last_error());
        }
    }
}
fn live(stream: &Stream) -> Result<(), String> {
    if let Some(e) = &stream.error {
        return Err(format!("output stream error: {e}"));
    }
    if !stream.active {
        return Err("output stream became inactive; playback did not complete".into());
    }
    if !stream.exclusive {
        return Err("exclusive mode is not confirmed; refusing to report success".into());
    }
    Ok(())
}
fn fade(pcm: &mut [f32], rate: u32, channels: usize) {
    let frames = pcm.len() / channels;
    let ramp = (rate as usize / 100).min(frames / 2).max(1);
    for (n, frame) in pcm.chunks_exact_mut(channels).enumerate() {
        let gain = (n.min(frames - 1 - n) as f32 / ramp as f32).min(1.0);
        for sample in frame {
            *sample *= gain;
        }
    }
}
// Embed the existing fixture so the executable can be moved away from the
// checkout. The decoder accepts local files, so materialize a private temporary
// copy, then remove it when decoding ends. create_new avoids following symlinks.
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Result<Self, String> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..100 {
            let path = std::env::temp_dir().join(format!(
                "deviant-hardware-{}-{}.flac",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut file = match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => file,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("cannot create temporary fixture: {e}")),
            };
            let fixture = Self(path);
            let written = file.write_all(include_bytes!("../tests/fixtures/tone.flac"));
            drop(file);
            written.map_err(|e| format!("cannot write temporary fixture: {e}"))?;
            return Ok(fixture);
        }
        Err("could not create a unique temporary fixture".into())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0) {
            eprintln!(
                "Could not remove temporary fixture {}: {e}",
                self.0.display()
            );
        }
    }
}
fn clip(rate: u32) -> Result<Vec<f32>, String> {
    // This is the committed, synthetic 50 ms FLAC fixture; no downloads or
    // external programs are involved. Decode entirely before opening output.
    let fixture = Fixture::new()?;
    let mut decoder = decode::NativeDecoder::open(&fixture.0)
        .map_err(|e| format!("could not decode embedded FLAC fixture: {e}"))?;
    if decoder.info.channels != 2 || decoder.info.sample_rate != 48000 {
        return Err("fixture unexpectedly changed format; expected 48 kHz stereo".into());
    }
    let mut sample = Vec::new();
    loop {
        let block = decoder.read(4096)?;
        if block.is_empty() {
            break;
        }
        sample.extend(block);
        if sample.len() > 48000 * 2 {
            return Err("fixture exceeds one second; refusing unbounded preload".into());
        }
    }
    if sample.is_empty() {
        return Err("fixture decoded to no frames".into());
    }
    if rate != 48000 {
        sample = decode::resample(&sample, 48000, rate, 2)?;
        println!(
            "Fixture explicitly resampled: 48000 -> {rate} Hz (offline linear interpolation)."
        );
    }
    for s in &mut sample {
        *s *= 0.5;
    }
    fade(&mut sample, rate, 2);
    println!(
        "Decoded sample: FLAC, source 48000 Hz / 16-bit / 2 channels; {} output frames.",
        sample.len() / 2
    );
    let mut tone = Vec::with_capacity(rate as usize * 2);
    for n in 0..rate {
        let value = 0.05 * (std::f32::consts::TAU * 440.0 * n as f32 / rate as f32).sin();
        tone.extend([value, value]);
    }
    fade(&mut tone, rate, 2);
    tone.resize(tone.len() + rate as usize / 5 * 2, 0.0); // 200 ms gap.
    tone.extend(sample);
    tone.resize(tone.len() + rate as usize / 10 * 2, 0.0); // 100 ms tail.
    if tone.len() > rate as usize * 2 * 2 {
        return Err("test clip exceeds engine's two-second queue".into());
    }
    Ok(tone)
}
fn run(opts: Options) -> Result<(), String> {
    if opts.help {
        println!(
            "Hardware playback test (Windows WASAPI exclusive only)\n\
Usage: cargo run --release --example hardware_playback -- [OPTIONS]\n\
  --list                 List output devices without playing\n\
  --device NUMBER|NAME   Select a 1-based number or exact name (quote names)\n\
  --rate HZ              Requested output rate; default 48000\n\
  --help                 Show this help\n\
With no --device, prompts for a number or exact name. Plays a quiet 1 s\n\
440 Hz tone, a gap, and the committed 50 ms decoded FLAC sample."
        );
        return Ok(());
    }
    if !cfg!(windows) {
        return Err(
            "this hardware test requires Windows; WASAPI is unavailable on this platform".into(),
        );
    }
    let text = abi_string(|p, n| unsafe { ffi::dae_devices_json(p, n) })
        .map_err(|e| format!("device enumeration failed: {e}: {}", last_error()))?;
    let devices: Vec<Device> = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    println!("Available active output devices:");
    for (i, d) in devices.iter().enumerate() {
        println!("  {}. {}\n     ID: {}", i + 1, d.name, d.id);
    }
    if devices.is_empty() {
        return Err("no active output devices found; check the USB DAC/adapter connection".into());
    }
    if opts.list {
        return Ok(());
    }
    let selection = if let Some(s) = opts.device {
        s
    } else {
        print!("Pick a device by number or exact name: ");
        io::stdout().flush().map_err(|e| e.to_string())?;
        let mut s = String::new();
        if io::stdin().read_line(&mut s).map_err(|e| e.to_string())? == 0 {
            return Err("input closed before selecting a device".into());
        }
        s.trim_end_matches(['\r', '\n']).to_owned()
    };
    let chosen = select(&devices, &selection)?;
    println!("Chosen device: {}\nEndpoint ID: {}", chosen.name, chosen.id);
    let rate = opts.rate.unwrap_or(48000);
    let pcm = clip(rate)?;
    let engine = Engine::new()?;
    let device_id = CString::new(chosen.id.as_str()).map_err(|e| e.to_string())?;
    println!("Request: {rate} Hz / 24 valid bits in 32-bit integer containers / 2 channels.");
    let opened =
        unsafe { ffi::dae_open_exclusive(engine.0, device_id.as_ptr(), rate, 2, 32, 24, 0) };
    if let Err(e) = check(opened) {
        return Err(format!(
            "EXCLUSIVE OPEN FAILED: {e}\nShared fallback: NO; no shared stream was opened."
        ));
    }
    let before = engine.stream()?;
    live(&before)?;
    let f = before
        .format
        .as_ref()
        .ok_or("engine omitted negotiated format")?;
    println!(
        "Actual negotiated format: {} Hz / {} valid bits in {}-bit {} containers / {} channels.",
        f.sample_rate,
        f.valid_bits,
        f.container_bits,
        if f.float { "float" } else { "integer PCM" },
        f.channels
    );
    println!("Exclusive mode: CONFIRMED by successful WASAPI exclusive initialization.\nShared fallback: NO (engine does not open shared mode).");
    if f.sample_rate != rate || f.channels != 2 {
        return Err("negotiated rate/channels differ from the prepared clip".into());
    }
    // One atomic, all-or-nothing enqueue: every frame is available before the
    // worker can start consuming this clip. There is no streaming producer to
    // starve mid-clip. Startup and post-clip idle frames remain counted by engine.
    check(unsafe { ffi::dae_feed_frames(engine.0, pcm.as_ptr(), pcm.len(), rate, 2) })?;
    println!("Playing: quiet 440 Hz tone, 200 ms gap, short 1 kHz decoded FLAC sample.");
    let deadline = Instant::now() + Duration::from_secs(10);
    let end = loop {
        let stream = engine.stream()?;
        live(&stream)?;
        if stream.queued_frames == 0 && stream.pending_device_frames == 0 {
            break stream;
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for playback to drain; not reporting completion".into());
        }
        thread::sleep(Duration::from_millis(5));
    };
    println!("Playback completed: YES (all queued frames drained; stream remained active and exclusive).");
    println!(
        "Engine queue starvation during the clip: NO (complete clip was enqueued in one call)."
    );
    println!(
        "Raw empty-queue silence frames: {} total; {} since the pre-feed snapshot (includes startup/end padding).",
        end.underrun_frames, end.underrun_frames.saturating_sub(before.underrun_frames)
    );
    println!("Hardware/driver underrun-free delivery: UNKNOWN; the engine has no driver glitch counter.\nListen for gaps/distortion; software completion cannot confirm physical DAC output.");
    check(ffi::dae_close_stream(engine.0))?;
    Ok(())
}
fn main() -> std::process::ExitCode {
    match options(std::env::args().skip(1)).and_then(run) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!(
                "TEST FAILED: {e}\nPlayback success / underrun-free delivery: NOT CONFIRMED."
            );
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_rejects_invalid_and_ambiguous_names() {
        let devices = vec![
            Device {
                id: "a".into(),
                name: "USB DAC".into(),
            },
            Device {
                id: "b".into(),
                name: "Speakers".into(),
            },
            Device {
                id: "c".into(),
                name: "USB DAC".into(),
            },
        ];
        assert_eq!(select(&devices, "2").unwrap().id, "b");
        assert_eq!(select(&devices, "Speakers").unwrap().id, "b");
        for invalid in ["0", "4", "USB DAC", "speakers", ""] {
            assert!(select(&devices, invalid).is_err());
        }
    }
    #[test]
    fn cli_rejects_malformed_options() {
        assert!(options(["--rate", "bad"].map(str::to_owned)).is_err());
        assert!(options(["--device"].map(str::to_owned)).is_err());
        assert!(options(["--rate", "1"].map(str::to_owned)).is_err());
        assert!(options(["--wat"].map(str::to_owned)).is_err());
        let opts = options(["--device", "USB DAC", "--rate", "44100"].map(str::to_owned)).unwrap();
        assert_eq!(opts.device.as_deref(), Some("USB DAC"));
        assert_eq!(opts.rate, Some(44100));
    }
    #[test]
    fn preloaded_clip_fits_queue_and_is_quiet() {
        for rate in [8000, 44100, 48000, 96000, 384000] {
            let pcm = clip(rate).unwrap();
            assert!(pcm.len().is_multiple_of(2));
            assert!(pcm.len() <= rate as usize * 4);
            assert!(pcm.len() > rate as usize * 2);
            assert!(pcm.iter().all(|s| s.is_finite() && s.abs() <= 0.06));
            assert_eq!(pcm[0], 0.0);
            assert_eq!(*pcm.last().unwrap(), 0.0);
        }
    }
    #[test]
    fn status_never_accepts_inactive_shared_or_error() {
        let mut s = Stream {
            active: true,
            exclusive: true,
            format: None,
            error: None,
            underrun_frames: 0,
            queued_frames: 0,
            pending_device_frames: 0,
        };
        assert!(live(&s).is_ok());
        s.exclusive = false;
        assert!(live(&s).is_err());
        s.exclusive = true;
        s.active = false;
        assert!(live(&s).is_err());
        s.active = true;
        s.error = Some("device removed".into());
        assert!(live(&s).is_err());
    }
}
