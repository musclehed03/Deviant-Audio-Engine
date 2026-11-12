# Deviant Audio Engine

Standalone GPL-3.0-only Rust library and C ABI for Windows native audio. No UI,
network access, telemetry, accounts, or Flow-Tube integration.

Build with stable Rust and the Visual Studio C++ build tools on Windows:

```sh
cargo build --locked --release
cargo test --locked
```

The DLL is written to `target/release/deviant_audio_engine.dll`.
Linux builds support decoding and mock tests; hardware output is Windows-only.
Windows CI builds and tests without requiring an audio device.
