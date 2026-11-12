//! All COM objects stay on one dedicated MTA thread.
use crate::output::*;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use wasapi::*;

struct Com;
impl Com { fn new() -> Result<Self,String> { initialize_mta().ok().map_err(|e| e.to_string())?; Ok(Self) } }
impl Drop for Com { fn drop(&mut self) { deinitialize(); } }
fn err(e: impl std::fmt::Display) -> String { e.to_string() }
fn wave(f: Format) -> WaveFormat {
    WaveFormat::new(f.container_bits as usize, f.valid_bits as usize, &if f.float { SampleType::Float } else { SampleType::Int }, f.sample_rate as usize, f.channels as usize, None)
}
pub fn devices() -> Result<Vec<DeviceInfo>, String> {
    thread::spawn(|| {
        let _com = Com::new()?;
        let enumerator = DeviceEnumerator::new().map_err(err)?;
        let collection = enumerator.get_device_collection(&Direction::Render).map_err(err)?;
        let mut result = Vec::new();
        for d in &collection {
            let d = d.map_err(err)?;
            result.push(DeviceInfo { id:d.get_id().map_err(err)?, name:d.get_friendlyname().map_err(err)? });
        }
        Ok(result)
    }).join().map_err(|_| "device enumeration thread panicked".to_string())?
}
struct Hardware { device: Device, client: AudioClient }
impl ExclusiveDevice for Hardware {
    fn supports(&mut self, f: Format) -> Result<bool,String> {
        Ok(self.client.is_supported(&wave(f), &ShareMode::Exclusive).is_ok())
    }
    fn open(&mut self, f: Format) -> Result<(),String> {
        self.client = self.device.get_iaudioclient().map_err(err)?;
        let wf = wave(f);
        let (_, min) = self.client.get_device_period().map_err(err)?;
        let period = self.client.calculate_aligned_period_near(min.max(100_000), Some(128), &wf).map_err(err)?;
        let mode = StreamMode::EventsExclusive { period_hns: period };
        if let Err(first) = self.client.initialize_client(&wf, &Direction::Render, &mode) {
            // Retry with the driver's aligned frame count on a fresh client.
            let frames = self.client.get_buffer_size().map_err(|_| first.to_string())?;
            let aligned = calculate_period_100ns(frames as i64, f.sample_rate as i64);
            self.client = self.device.get_iaudioclient().map_err(err)?;
            self.client.initialize_client(&wf, &Direction::Render, &StreamMode::EventsExclusive { period_hns:aligned }).map_err(err)?;
        }
        if !matches!(self.client.get_sharemode(), Some(ShareMode::Exclusive)) { return Err("backend did not confirm exclusive share mode".into()); }
        Ok(())
    }
}
pub fn start(id: String, requested: Format) -> Result<(SharedQueue, JoinHandle<()>),String> {
    let (tx,rx) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let mut published: Option<SharedQueue> = None;
        let result = (|| -> Result<(),String> {
            let _com = Com::new()?;
            let enumerator = DeviceEnumerator::new().map_err(err)?;
            let device = if id.is_empty() { enumerator.get_default_device(&Direction::Render) } else { enumerator.get_device(&id) }.map_err(err)?;
            if !matches!(device.get_direction(), Direction::Render) { return Err("device is not a render endpoint".into()); }
            let client = device.get_iaudioclient().map_err(err)?;
            let mut hardware = Hardware { device, client };
            let f = negotiate(&mut hardware, requested)?;
            let client = hardware.client;
            let event = client.set_get_eventhandle().map_err(err)?;
            let render = client.get_audiorenderclient().map_err(err)?;
            let frames = client.get_buffer_size().map_err(err)? as usize;
            let mut bytes = vec![0; frames * f.bytes_per_frame()];
            // Exclusive event-driven streams require full buffers, including the initial silence.
            render.write_to_device(frames, &bytes, None).map_err(err)?;
            client.start_stream().map_err(err)?;
            let q = Queue::new(f);
            { let mut q = q.lock().unwrap(); q.status.active = true; q.status.exclusive = true; }
            published = Some(q.clone());
            if tx.send(Ok(q.clone())).is_err() { client.stop_stream().map_err(err)?; return Ok(()); }
            let run = (|| -> Result<(),String> {
                loop {
                    if q.lock().unwrap().stop { break; }
                    event.wait_for_event(250).map_err(err)?;
                    { let mut queue = q.lock().unwrap(); if queue.stop { break; } queue.render(&mut bytes); }
                    render.write_to_device(frames, &bytes, None).map_err(err)?;
                }
                Ok(())
            })();
            let stop = client.stop_stream().map_err(err);
            run.and(stop)
        })();
        if let Some(q) = published {
            let mut q = q.lock().unwrap(); q.status.active = false; q.status.exclusive = false;
            if let Err(e) = result { q.status.error = Some(e); }
        } else if let Err(e) = result { let _ = tx.send(Err(e)); }
    });
    match rx.recv() {
        Ok(Ok(q)) => Ok((q,worker)),
        other => { let _ = worker.join(); Err(match other { Ok(Err(e)) => e, _ => "output worker failed to initialize".into() }) }
    }
}
