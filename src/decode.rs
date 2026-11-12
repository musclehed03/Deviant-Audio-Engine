//! Local-file streaming decode. Licensing question: Symphonia is MPL-2.0,
//! not permissively licensed; see THIRD_PARTY.md before redistribution.
use serde::Serialize;
use std::{
    collections::VecDeque,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};
use symphonia::core::{
    audio::SampleBuffer,
    codecs::{Decoder, DecoderOptions, CODEC_TYPE_AAC},
    errors::Error,
    formats::{FormatOptions, FormatReader},
    io::MediaSourceStream,
    meta::MetadataOptions,
    probe::Hint,
};

#[derive(Clone, Debug, Serialize)]
pub struct TrackInfo {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub source_bit_depth: Option<u32>,
    pub source_bitrate_bps: Option<u64>,
    pub measured_payload_bitrate_bps: Option<u64>,
    pub decoder_precision_bits: Option<u32>,
}
struct Symphonia {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track: u32,
}
struct Opus {
    reader: ogg::PacketReader<File>,
    decoder: opus_decoder::OpusDecoder,
    serial: u32,
    pre_skip: u64,
    decoded: u64,
    gain: f32,
    ended: bool,
}
enum Backend {
    Symphonia(Symphonia),
    Opus(Box<Opus>),
}
pub struct NativeDecoder {
    backend: Backend,
    pub info: TrackInfo,
    pending: VecDeque<f32>,
    eof: bool,
    payload_bytes: u64,
    payload_frames: u64,
}
fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}
impl NativeDecoder {
    pub fn open(path: &Path) -> Result<Self, String> {
        let mut file = File::open(path).map_err(error)?;
        let mut magic = [0; 4];
        file.read_exact(&mut magic).map_err(error)?;
        file.seek(SeekFrom::Start(0)).map_err(error)?;
        if &magic == b"OggS" {
            let mut reader = ogg::PacketReader::new(file.try_clone().map_err(error)?);
            if let Some(head) = reader.read_packet().map_err(error)? {
                if head.data.starts_with(b"OpusHead") {
                    let h = &head.data;
                    if h.len() != 19 || h[8] != 1 || !(1..=2).contains(&h[9]) || h[18] != 0 {
                        return Err(
                            "only Opus version 1 mapping-family 0 mono/stereo is supported".into(),
                        );
                    }
                    let channels = h[9] as u16;
                    let pre_skip = u16::from_le_bytes([h[10], h[11]]) as u64;
                    let gain_db = i16::from_le_bytes([h[16], h[17]]) as f32 / 256.0;
                    let serial = head.stream_serial();
                    let tags = reader
                        .read_packet()
                        .map_err(error)?
                        .ok_or("missing OpusTags")?;
                    if tags.stream_serial() != serial || !tags.data.starts_with(b"OpusTags") {
                        return Err("invalid OpusTags".into());
                    }
                    return Ok(Self {
                        backend: Backend::Opus(Box::new(Opus {
                            reader,
                            decoder: opus_decoder::OpusDecoder::new(48000, channels as usize)
                                .map_err(error)?,
                            serial,
                            pre_skip,
                            decoded: 0,
                            gain: 10f32.powf(gain_db / 20.0),
                            ended: false,
                        })),
                        info: TrackInfo {
                            codec: "opus".into(),
                            sample_rate: 48000,
                            channels,
                            source_bit_depth: None,
                            source_bitrate_bps: None,
                            measured_payload_bitrate_bps: None,
                            decoder_precision_bits: Some(16),
                        },
                        pending: VecDeque::new(),
                        eof: false,
                        payload_bytes: 0,
                        payload_frames: 0,
                    });
                }
            }
            file.seek(SeekFrom::Start(0)).map_err(error)?;
        }
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|x| x.to_str()) {
            hint.with_extension(ext);
        }
        let probed = symphonia::default::get_probe()
            .format(
                &hint,
                mss,
                &FormatOptions {
                    enable_gapless: true,
                    ..Default::default()
                },
                &MetadataOptions::default(),
            )
            .map_err(error)?;
        let mut format = probed.format;
        let track = format.default_track().ok_or("no default audio track")?;
        let track_id = track.id;
        let p = track.codec_params.clone();
        let mut decoder = symphonia::default::get_codecs()
            .make(&p, &DecoderOptions::default())
            .map_err(error)?;
        let first = loop {
            let packet = format.next_packet().map_err(error)?;
            if packet.track_id() == track_id {
                break packet;
            }
        };
        let decoded = decoder.decode(&first).map_err(error)?;
        let spec = *decoded.spec();
        let mut initial = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        initial.copy_interleaved_ref(decoded);
        let initial = initial.samples().to_vec();
        let codec = symphonia::default::get_codecs()
            .get_codec(p.codec)
            .map(|descriptor| descriptor.short_name)
            .unwrap_or("unknown")
            .to_string();
        let info = TrackInfo {
            codec,
            sample_rate: spec.rate,
            channels: spec.channels.count() as u16,
            source_bit_depth: if p.codec == CODEC_TYPE_AAC {
                None
            } else {
                p.bits_per_sample
            },
            source_bitrate_bps: None,
            measured_payload_bitrate_bps: None,
            decoder_precision_bits: None,
        };
        if !(1..=2).contains(&info.channels) {
            return Err("only mono/stereo decoding is supported".into());
        }
        let track = track_id;
        let initial_frames = initial.len() / info.channels as usize;
        Ok(Self {
            backend: Backend::Symphonia(Symphonia {
                format,
                decoder,
                track,
            }),
            info,
            pending: initial.into(),
            eof: false,
            payload_bytes: first.data.len() as u64,
            payload_frames: initial_frames as u64,
        })
    }
    /// Returns interleaved float32 PCM; empty means EOF. Packet failures are errors.
    pub fn read(&mut self, max_frames: usize) -> Result<Vec<f32>, String> {
        if max_frames == 0 || max_frames > 65536 {
            return Err("read size must be 1..65536 frames".into());
        }
        let wanted = max_frames * self.info.channels as usize;
        while self.pending.len() < wanted && !self.eof {
            let (samples, bytes, frames) = match &mut self.backend {
                Backend::Symphonia(s) => {
                    let packet = match s.format.next_packet() {
                        Ok(p) => p,
                        Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                            self.eof = true;
                            break;
                        }
                        Err(e) => return Err(error(e)),
                    };
                    if packet.track_id() != s.track {
                        continue;
                    }
                    let bytes = packet.data.len();
                    let decoded = s.decoder.decode(&packet).map_err(error)?;
                    if decoded.spec().rate != self.info.sample_rate
                        || decoded.spec().channels.count() != self.info.channels as usize
                    {
                        return Err("mid-stream format change requires reopening output".into());
                    }
                    let mut buf =
                        SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
                    buf.copy_interleaved_ref(decoded);
                    let samples = buf.samples().to_vec();
                    let frames = samples.len() / self.info.channels as usize;
                    (samples, bytes, frames)
                }
                Backend::Opus(o) => {
                    if o.ended {
                        if o.reader.read_packet().map_err(error)?.is_some() {
                            return Err("chained/multiplexed Ogg Opus is unsupported".into());
                        }
                        self.eof = true;
                        break;
                    }
                    let packet = o
                        .reader
                        .read_packet()
                        .map_err(error)?
                        .ok_or("truncated Opus stream: no EOS page")?;
                    if packet.stream_serial() != o.serial {
                        return Err("multiplexed Ogg is unsupported".into());
                    }
                    if packet.data.is_empty() {
                        return Err("empty Opus packet".into());
                    }
                    let mut samples = vec![
                        0.0;
                        opus_decoder::OpusDecoder::MAX_FRAME_SIZE_48K
                            * self.info.channels as usize
                    ];
                    let frames = o
                        .decoder
                        .decode_float(&packet.data, &mut samples, false)
                        .map_err(error)?;
                    let start = o.decoded;
                    o.decoded += frames as u64;
                    let mut end = o.decoded;
                    if packet.last_in_stream() {
                        end = packet.absgp_page();
                        if end < start || end > o.decoded || end < o.pre_skip {
                            return Err("invalid final Opus granule".into());
                        }
                        o.ended = true;
                    }
                    let skip = o.pre_skip.saturating_sub(start).min(frames as u64) as usize;
                    let keep_end = (end - start) as usize;
                    let channels = self.info.channels as usize;
                    let samples = samples[skip.min(keep_end) * channels..keep_end * channels]
                        .iter()
                        .map(|v| v * o.gain)
                        .collect();
                    (samples, packet.data.len(), frames)
                }
            };
            self.payload_bytes += bytes as u64;
            self.payload_frames += frames as u64;
            self.pending.extend(samples);
        }
        self.info.measured_payload_bitrate_bps =
            (self.payload_bytes * 8 * self.info.sample_rate as u64)
                .checked_div(self.payload_frames);
        let count = wanted.min(self.pending.len());
        Ok(self.pending.drain(..count).collect())
    }
}

/// Explicit offline linear interpolation. For playback prefer opening at source rate.
/// This changes samples and is not a mastering-quality bandlimited resampler.
pub fn resample(
    samples: &[f32],
    source_rate: u32,
    target_rate: u32,
    channels: u16,
) -> Result<Vec<f32>, String> {
    if samples.len() > 16_777_216
        || !(8000..=384000).contains(&source_rate)
        || !(8000..=384000).contains(&target_rate)
        || !(1..=2).contains(&channels)
        || !samples.len().is_multiple_of(channels as usize)
        || samples.iter().any(|x| !x.is_finite())
    {
        return Err("invalid resampling input".into());
    }
    if source_rate == target_rate {
        return Ok(samples.to_vec());
    }
    let channels = channels as usize;
    let frames = samples.len() / channels;
    let count = (frames as u64 * target_rate as u64).div_ceil(source_rate as u64) as usize;
    if count > 16_777_216 / channels {
        return Err("resampling output exceeds 16M samples".into());
    }
    let mut out = Vec::with_capacity(count * channels);
    for n in 0..count {
        let position = n as f64 * source_rate as f64 / target_rate as f64;
        let a = position.floor() as usize;
        let b = (a + 1).min(frames - 1);
        let t = (position - a as f64) as f32;
        for ch in 0..channels {
            out.push(samples[a * channels + ch] * (1.0 - t) + samples[b * channels + ch] * t);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resample_rates_and_identity() {
        assert_eq!(
            resample(&[0.0, 1.0], 8000, 16000, 1).unwrap(),
            vec![0.0, 0.5, 1.0, 1.0]
        );
        assert_eq!(
            resample(&[0.0, 1.0], 48000, 48000, 2).unwrap(),
            vec![0.0, 1.0]
        );
        assert!(resample(&[0.0], 0, 48000, 1).is_err());
        assert!(resample(&[0.0], 48000, 48000, 2).is_err());
        assert!(resample(&[], 48000, 44100, 2).unwrap().is_empty());
    }
    #[test]
    fn decode_real_codec_fixtures() {
        for (file, codec) in [
            ("tone.flac", "flac"),
            ("tone.m4a", "aac"),
            ("tone.opus", "opus"),
        ] {
            let mut d = NativeDecoder::open(
                &Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures")
                    .join(file),
            )
            .unwrap();
            assert_eq!(d.info.codec, codec);
            assert_eq!(d.info.sample_rate, 48000);
            assert_eq!(d.info.channels, 2);
            assert_eq!(d.info.source_bitrate_bps, None);
            let mut samples = Vec::new();
            loop {
                let block = d.read(257).unwrap();
                if block.is_empty() {
                    break;
                }
                samples.extend(block);
            }
            assert!(samples.iter().all(|v| v.is_finite()));
            assert!(samples.iter().any(|v| v.abs() > 0.01));
            assert!(
                samples.len() >= 4700 && samples.len() <= 8192,
                "{file}: {}",
                samples.len()
            );
            assert!(d.info.measured_payload_bitrate_bps.unwrap() > 0);
            if codec == "aac" {
                assert_eq!(samples.len(), 8192);
            }
            if codec == "opus" {
                assert_eq!(samples.len(), 4800);
                assert_eq!(d.info.decoder_precision_bits, Some(16));
            }
            if codec == "flac" {
                assert_eq!(samples.len(), 4800);
                assert_eq!(d.info.source_bit_depth, Some(16));
            }
        }
    }
    #[test]
    fn malformed_input_is_error() {
        assert!(NativeDecoder::open(Path::new("Cargo.toml")).is_err());
    }
}
