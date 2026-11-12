# Deviant Audio Engine

Standalone GPL-3.0-only Rust `cdylib`/`rlib` with a C ABI for Windows WASAPI
exclusive output and native local-file decode. No UI, installer, network calls,
telemetry, accounts, or Flow-Tube code. A future Tauri host can load this library;
no host integration is included here.

## Build and test

Install stable Rust (minimum 1.87). On Windows install Visual Studio Build Tools
with **Desktop development with C++**, then use an x64 developer terminal:

```sh
cargo build --locked --release
cargo test --locked
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
python examples/ffi_smoke.py
```

Outputs: `target/release/deviant_audio_engine.dll` and its import library on
Windows, `libdeviant_audio_engine.so` on Linux. The public header is
[include/deviant_audio_engine.h](include/deviant_audio_engine.h). Use matching
host/DLL architectures. Linux builds support decoding and mock tests; hardware
output reports unavailable. Cargo downloads dependencies at build time; the
engine itself performs no network activity. After fetching dependencies, use
`cargo build --locked --offline --release` for an offline build.

CI in [.github/workflows/ci.yml](.github/workflows/ci.yml) runs format, lint,
unit/mock tests, release builds and an exported C ABI smoke test on both Windows
and Linux. Tests never need a real audio endpoint. `examples/ffi_smoke.py` uses
only Python's standard library and does not open a hardware stream.

For the interactive Windows USB DAC playback check, see [TESTING.md](TESTING.md).
It lists devices, opens exclusive output and plays a generated tone plus the
committed FLAC fixture through the existing engine APIs.

## Output behavior

* Active render endpoints are enumerated by stable endpoint ID and friendly
  name. A null/empty ID selects the current default render endpoint.
* A dedicated MTA worker owns all COM objects. WASAPI is initialized with
  `EventsExclusive`, with driver-period alignment and an aligned-buffer retry.
  Each event fills the full exclusive buffer. Startup primes silence; an empty
  queue also emits silence and increments the underrun counter.
* Negotiation tries the requested PCM representation, then float32, 24-in-32,
  packed 24, PCM16 and PCM32 as needed. Only successful exclusive initialization
  marks the stream exclusive. Rate and channel count never silently change.
* Exclusive denied, busy, unsupported format, unplug, driver failure and event
  timeout return an error or mark the stream inactive with an error. The engine
  never opens shared mode. The caller may choose a different device/rate or a
  different output engine after seeing the failure.
* Supported engine layouts are mono/stereo, 8–384 kHz, PCM16/24/32 and float32.
  These bounds are not a claim about any particular device. Capabilities expose
  only the actual negotiated stream format, not an exhaustive device format
  matrix. The OS driver is authoritative at open time.

PCM enters a bounded two-second interleaved float32 queue. Integer output is
rounded, clipped and packed with valid bits aligned to the most significant
container bits. There is no volume control, dither, DSP, automatic remixing or
implicit sample-rate conversion. Float conversion and integer quantization can
change samples; end-to-end delivery is reported as **unverified**.

## Decode and sample rates

[Symphonia](https://github.com/pdeljanov/Symphonia) handles AAC-LC (MP4/M4A and
ADTS), FLAC and PCM. `opus-decoder` plus `ogg` handles Ogg Opus version 1,
mapping family 0 mono/stereo. Opus output is 48 kHz, honors pre-skip, header gain
and final granule trim, and rejects chained/multiplexed streams. Its float API
internally uses 16-bit PCM, which is explicitly reported. Decoder support is
not a promise to support every profile/container. Failed packets are errors.

Prefer opening output at the track rate. Mismatched rate/channels are rejected
by feed. `dae_resample` provides optional **offline linear interpolation** for
complete clips, limited to 16M input/output samples. It has no antialias filter;
downsampling can alias. Do not run it independently on streaming chunks. A
production quality streaming resampler is outside this initial implementation.
AAC priming/padding may remain; seamless AAC transitions are not promised.

**Licensing question:** Symphonia is MPL-2.0, not permissive. It is retained as
the requested decoder choice, with its GPL combination and redistribution
obligations flagged in [THIRD_PARTY.md](THIRD_PARTY.md), along with AAC patent
questions and the Opus implementation's maturity/precision limits. Review that
file before binary distribution. The engine's license is [GPLv3](LICENSE).

## C ABI v1 usage

1. Call `dae_init()`; retain the nonzero integer handle. Query
   `dae_devices_json` with null/0, allocate the returned byte count, then call
   again to copy UTF-8 JSON. All text sizes include the terminating NUL.
2. `dae_decode_open(handle, local_path)` opens a local UTF-8 path and decodes the
   first packet to establish actual source format. Query
   `dae_capabilities_json` for track rate/channels before opening output.
3. `dae_open_exclusive` requests a device, rate, channel count, container bits,
   valid bits and float flag. Query capabilities again: output representation
   may differ from the request. A failure opens no shared stream.
4. Repeatedly call `dae_decode_read` into a caller-owned float buffer, then
   `dae_feed_frames`. Read capacity and feed counts are **samples**;
   `written_frames` is **frames per channel**. A successful read of zero frames
   means EOF. Keep the block and retry if feed reports queue-full; never decode
   the next block until the current one is accepted.
5. To finish playback, poll capabilities until both `queued_frames` and
   `pending_device_frames` are zero while `active` is true. The latter counts
   queued-audio frames in the last submitted device buffer and is cleared at the
   next buffer event. Check `stream.error` throughout playback.
6. `dae_close_stream` stops immediately and discards queued audio;
   `dae_decode_close` closes only the decoder. `dae_shutdown` closes both and
   invalidates the handle. Reopening a stream first closes the old stream.

Status functions return 0 on success and -1 on failure. Call `dae_last_error`
on the **same thread** after failure. Errors remain until the next failure;
success need not clear them. JSON functions return 0 on error, otherwise the
required buffer size. Undersized buffers are untouched; retry if required size
grows. The header specifies pointer validity and ownership rules. Invalid IDs,
nulls and bounded lengths are checked; a C caller must still supply valid,
properly aligned memory. Rust panics are caught at exported boundaries; process
allocation failure and invalid foreign pointers cannot be recovered this way.

Calls serialize access and may block (file I/O, stream startup/shutdown). Use a
host control/decode thread, never a real-time audio callback. Output owns its
own event thread; feed backpressure prevents unlimited buffering.

## Honest capabilities

The JSON contains ABI version, compiled decoder availability, platform output
availability, live stream status, actual output format (rate, channels,
container bits, valid bits, float/integer), underruns and queue state. Track
metadata is separate: source codec/rate/channels, source bit depth when known,
source bitrate when known, and decoder precision where constrained.
`hardware_output_available` means the backend is compiled for this platform,
not that a working endpoint exists or exclusive permission is granted.

Unknown source fields are JSON `null`. Lossy codecs have no asserted source PCM
bit depth. `source_bitrate_bps` stays null when not reliably supplied;
`measured_payload_bitrate_bps` is a running compressed-packet-byte average over
decoded packet duration, excluding container overhead. It is not the encoder's
nominal bitrate. No field asserts verified end-to-end sample identity.

## Validation limits

Co-located unit tests cover format negotiation, busy/unsupported mock devices,
PCM packing, finite/frame validation, rate rejection, backpressure, event fill,
underrun accounting, shutdown, stale handles, buffer sizing, panic containment,
resampling and real AAC/FLAC/Opus fixture decoding. Synthetic fixtures and codec
limits are described in THIRD_PARTY.md.

Before release, manually test on Windows: default and explicit endpoints,
multiple supported source rates, exclusive disabled in Sound settings, a busy
endpoint, unsupported rates, unplug during playback, long playback with queue
backpressure, drain, reopen and shutdown. CI and cross-compilation cannot prove
hardware behavior, driver correctness or physical output identity.
