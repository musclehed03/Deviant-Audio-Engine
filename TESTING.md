# Windows 11 USB DAC playback test

Use a terminal in this repository's root with a working Windows Rust + Cargo
toolchain. No Python, FFmpeg, test framework, or separate DLL setup is needed.
If Rust reports a missing `link.exe`, install the Visual Studio C++ Build Tools
prerequisite described by the Windows Rust installer, then reopen the terminal.

Connect the USB DAC through your USB-C to USB-A adapter and let Windows recognize
it. Start with the DAC/amplifier volume low: exclusive output can bypass the
Windows app-volume mixer. The test signal peaks at about 5% of full scale.

Build, list devices, then run interactively (PowerShell or Command Prompt):

```text
cargo build --locked --release --example hardware_playback
cargo run --locked --release --example hardware_playback -- --list
cargo run --locked --release --example hardware_playback
```

Enter a displayed **1-based number** or the **exact device name** at the prompt.
Choose the USB DAC rather than the laptop's internal speakers. To skip the prompt:

```text
cargo run --locked --release --example hardware_playback -- --device 2
cargo run --locked --release --example hardware_playback -- --device "Your DAC's exact displayed name"
```

Replace `2` or the quoted name with your device. Duplicate names require a number;
numbers can change when devices are connected/disconnected. The chosen endpoint
ID is printed so you can identify what was opened. To run the built executable:

```text
.\target\release\examples\hardware_playback.exe --list
.\target\release\examples\hardware_playback.exe --device 2
```

The executable embeds the existing synthetic FLAC fixture and can be moved away
from the checkout. It briefly writes that fixture into the Windows temporary
folder for the engine's local-file decoder, then removes the temporary copy.

## What to expect

The test requests **48000 Hz, stereo, 24 valid bits in a 32-bit integer container**.
The engine may negotiate a different PCM representation at the same rate and
channel count. The test prints the **actual** rate, valid bit depth, container
width/type, and channels after opening. It prints exclusive confirmation only
when the engine reports an active exclusive stream; shared fallback is never
used. If exclusive opening fails, it prints the engine's error and exits with
failure. No unsupported format or permission failure is reported as success.

Listen for a quiet **one-second 440 Hz tone**, a **200 ms gap**, then the existing
**50 ms 1 kHz FLAC sample**. The final 100 ms is queued silence. The complete
roughly 1.35-second clip is prepared before opening and enqueued in one atomic
call, within the engine's two-second queue. The short FLAC sample sounds like a
brief higher-pitched beep. The program waits for both the queue and last device
buffer to drain, checking active/exclusive state and errors, with a 10-second
timeout. A successful run exits with code 0; failure exits with code 1.

The completion report distinguishes:

* **Playback completed**: all submitted frames drained and the stream was still
  active/exclusive at the final observation. This cannot prove sound reached
  the DAC's analog output; confirm audibly.
* **Engine queue starvation during the clip**: none, because the entire clip was
  enqueued atomically; no streaming producer can run out of data mid-clip.
* **Raw empty-queue silence frames**: the engine's existing `underrun_frames`
  counter, including startup silence and padding/idle output after the clip.
  A nonzero value is expected and does not by itself demonstrate an audible gap.
* **Hardware/driver underrun-free delivery**: unknown. The engine exposes no
  driver glitch counter, so the test does not claim the USB link, driver or
  physical output was free from glitches. Note any gaps, clicks or distortion
  you hear. This short preloaded test is not a sustained streaming stress test.

To request another rate explicitly:

```text
cargo run --locked --release --example hardware_playback -- --device 2 --rate 44100
cargo run --locked --release --example hardware_playback -- --device 2 --rate 96000
```

The tone is generated at that rate. The FLAC sample is explicitly resampled
using the engine's existing offline linear interpolator; this is printed and
is not a fidelity test. Unsupported rates fail without a shared-mode fallback.
No device-rate capability is assumed from the DAC's marketing specifications.

## If it fails

Copy the device name/ID, requested rate, negotiated format if present, and full
error text. Check the adapter/cable connection and DAC selection. Close other
audio apps if the endpoint is busy. In Windows Sound settings, open the DAC's
additional device properties and allow applications to take exclusive control
(the exact navigation varies by driver). Re-run at 48000 or 44100 Hz if your
DAC rejects a requested rate. If it disconnects or stops signaling buffer events,
the test must print a failure rather than a successful completion.

For a deliberate failure check, turn off exclusive control in the DAC's Windows
properties and rerun. Restore that setting afterward. A driver that rejects
exclusive access should produce `EXCLUSIVE OPEN FAILED` and no shared playback.

For software-only checks, without touching a hardware device:

```text
cargo test --locked --all-targets
```

CI builds this example and runs its device-selection, clip-generation and status
validation tests on Windows and Linux. CI does not run the interactive playback
command and cannot validate your Toshiba laptop, adapter or DAC.
