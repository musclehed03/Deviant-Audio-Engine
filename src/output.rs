//! Output policy and bounded PCM queue, shared by WASAPI and mock devices.
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Format {
    pub sample_rate: u32,
    pub channels: u16,
    pub container_bits: u16,
    pub valid_bits: u16,
    pub float: bool,
}
impl Format {
    pub fn validate(self) -> Result<Self, String> {
        if !(8000..=384000).contains(&self.sample_rate)
            || !(1..=2).contains(&self.channels)
            || !matches!((self.container_bits, self.valid_bits, self.float),
                (16,16,false) | (24,24,false) | (32,24,false) | (32,32,false) | (32,32,true))
        {
            return Err("unsupported format: mono/stereo PCM16/24/32 or float32, 8–384 kHz required".into());
        }
        Ok(self)
    }
    pub fn bytes_per_frame(self) -> usize {
        self.channels as usize * (self.container_bits as usize / 8)
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct StreamStatus {
    pub active: bool,
    pub exclusive: bool,
    pub format: Option<Format>,
    pub error: Option<String>,
    pub underrun_frames: u64,
    pub queued_frames: usize,
}
pub struct Queue {
    pub samples: VecDeque<f32>,
    pub status: StreamStatus,
    pub stop: bool,
    capacity: usize,
}
pub type SharedQueue = Arc<Mutex<Queue>>;
impl Queue {
    pub fn new(format: Format) -> SharedQueue {
        let capacity = format.sample_rate as usize * format.channels as usize * 2;
        Arc::new(Mutex::new(Self {
            samples: VecDeque::with_capacity(capacity), capacity,
            status: StreamStatus { format: Some(format), ..Default::default() }, stop: false,
        }))
    }
    pub fn feed(&mut self, samples: &[f32], rate: u32, channels: u16) -> Result<(), String> {
        let f = self.status.format.ok_or("no stream")?;
        if !self.status.active { return Err(self.status.error.clone().unwrap_or("stream is inactive".into())); }
        if f.sample_rate != rate || f.channels != channels { return Err("PCM rate/channels must match negotiated output; resample explicitly first".into()); }
        if !samples.len().is_multiple_of(channels as usize) || samples.iter().any(|v| !v.is_finite()) { return Err("invalid interleaved PCM".into()); }
        if samples.len() > self.capacity - self.samples.len() { return Err("queue full; retry after output consumes frames".into()); }
        self.samples.extend(samples);
        Ok(())
    }
    pub fn render(&mut self, bytes: &mut [u8]) {
        let f = self.status.format.expect("queue always has format");
        let width = f.container_bits as usize / 8;
        let mut missing = 0usize;
        for out in bytes.chunks_exact_mut(width) {
            let sample = self.samples.pop_front().unwrap_or_else(|| { missing += 1; 0.0 });
            if f.float { out.copy_from_slice(&sample.to_le_bytes()); }
            else {
                let scale = (1u64 << (f.valid_bits - 1)) as f64;
                let v = ((sample as f64).clamp(-1.0, 1.0) * scale).round().clamp(-scale, scale - 1.0) as i64;
                let aligned = (v << (f.container_bits - f.valid_bits)) as i32;
                out.copy_from_slice(&aligned.to_le_bytes()[..width]);
            }
        }
        self.status.underrun_frames += (missing / f.channels as usize) as u64;
    }
    pub fn snapshot(&self) -> StreamStatus {
        let mut s = self.status.clone();
        s.queued_frames = self.samples.len() / s.format.map_or(1, |f| f.channels as usize);
        s
    }
}

pub trait ExclusiveDevice {
    fn supports(&mut self, format: Format) -> Result<bool, String>;
    fn open(&mut self, format: Format) -> Result<(), String>;
}
/// Never substitute a different rate or channel layout. Try sample representations only.
pub fn negotiate(device: &mut impl ExclusiveDevice, requested: Format) -> Result<Format, String> {
    requested.validate()?;
    let mut candidates = vec![requested];
    for (container_bits, valid_bits, float) in [(32,32,true),(32,24,false),(24,24,false),(16,16,false),(32,32,false)] {
        let f = Format { container_bits, valid_bits, float, ..requested };
        if !candidates.contains(&f) { candidates.push(f); }
    }
    let mut failures = Vec::new();
    for f in candidates {
        match device.supports(f) {
            Ok(true) => match device.open(f) { Ok(()) => return Ok(f), Err(e) => failures.push(e) },
            Ok(false) => (), Err(e) => failures.push(e),
        }
    }
    Err(format!("exclusive unavailable at {} Hz / {} channels; no shared stream opened: {}", requested.sample_rate, requested.channels, failures.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    pub fn format() -> Format { Format { sample_rate:48000, channels:2, container_bits:16, valid_bits:16, float:false } }
    struct Mock { available: bool, opened: Vec<Format> }
    impl ExclusiveDevice for Mock {
        fn supports(&mut self, f: Format) -> Result<bool,String> { Ok(f.container_bits == 16) }
        fn open(&mut self, f: Format) -> Result<(),String> { if !self.available { return Err("device busy".into()); } self.opened.push(f); Ok(()) }
    }
    #[test] fn mock_negotiation_and_busy() {
        let mut d = Mock { available:true, opened:vec![] };
        assert_eq!(negotiate(&mut d, Format { float:true, container_bits:32, valid_bits:32, ..format() }).unwrap(), format());
        assert_eq!(d.opened, vec![format()]);
        d.available = false;
        assert!(negotiate(&mut d, format()).unwrap_err().contains("no shared stream"));
    }
    #[test] fn mock_event_fill_backpressure_and_underrun() {
        let q = Queue::new(format()); let mut q = q.lock().unwrap(); q.status.active = true;
        assert!(q.feed(&[0.0,0.0],44100,2).is_err());
        assert!(q.feed(&[f32::NAN,0.0],48000,2).is_err());
        assert!(q.feed(&vec![0.0;192002],48000,2).is_err());
        q.feed(&[-1.0,1.0],48000,2).unwrap();
        let mut bytes = [0;8]; q.render(&mut bytes);
        assert_eq!(bytes, [0,128,255,127,0,0,0,0]);
        assert_eq!(q.snapshot().underrun_frames,1);
        assert_eq!(q.snapshot().queued_frames,0);
    }
}
