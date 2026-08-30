# Voxtype Meeting Transcriber

An Omarchy plugin for local meeting transcription. It preserves utterance timestamps and conversation order while using the local engine already configured in Voxtype.

## Requirements

- Omarchy Shell 4 or newer
- Voxtype with a supported local engine configured
- PipeWire/PulseAudio compatibility tools
- Rust toolchain (installation only)
- Python 3.11 or newer

## Install

Build and enable the per-user capture service:

```bash
./scripts/install-user.sh
```

Install or link this repository as the Omarchy plugin `io.github.boomdev.voxtype-meeting-transcriber`, then rescan and enable it in the right bar section. The service uses `~/.config/voxtype/config.toml` only as the transcription configuration source; Voxtype's native meeting mode should be disabled.

## Interaction

- Left click opens or closes the meeting panel.
- Right click refreshes service state.
- Start, stop, pause, and resume provide immediate pending feedback.
- The gear configures capture source/devices, audio retention, and which languages appear on the meeting page; engine/model are read-only.
- Recent meetings offer **Export and Open**, then **Open** after a transcript has been exported.
- `S` starts or stops while the main view has focus; `R` refreshes.

Meeting data is stored under the XDG data directory for `voxtype-meeting-service`. See [the complete feature specification](docs/features.md).
