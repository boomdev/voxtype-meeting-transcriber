# Voxtype Meeting Transcriber — Features

## Identity and scope

- Display name: **Voxtype Meeting Transcriber**.
- Canonical identifier: `io.github.boomdev.voxtype-meeting-transcriber`.
- Native Omarchy Shell bar widget plus a per-user background capture service.
- Voxtype remains the local speech-to-text engine used for each utterance; its configured engine, model, and language are snapshotted when a meeting starts.
- Voxtype's native meeting mode is not used. F9 dictation and `voxtype transcribe` remain available.

## Bar and popup

- Persistent, theme-aware icon with idle, recording, paused, busy, unavailable, and error states.
- No tooltip; left click opens the popup and right click refreshes state.
- Immediate **Please wait** feedback for start, stop, pause, and resume.
- Active meeting title, elapsed time, capture source, and utterance count.
- Click-to-select language chips on the main popup, filtered to languages enabled in settings from Voxtype's Whisper language picker.
- Closing or reloading the popup never interrupts a recording; the popup reconnects to the service.
- Optional desktop notifications, off by default.

## Recording controls

- Start with an optional meeting title, stop, truly pause, and resume.
- Paused audio is drained and discarded, including buffered partial speech, so it cannot appear later in the transcript.
- Capture microphone, system audio, or both.
- Select microphone and system-output devices.
- Split speech into utterances after 700 ms of silence, with a 30-second safety maximum.
- Preserve each utterance's capture timestamp and merge both sources chronologically rather than grouping speakers.
- Process transcription through one worker to keep resource use and ordering predictable.

## Transcription and storage

- Use the local transcription engine already configured in Voxtype.
- Reject remote or unsupported Voxtype engines for meeting capture.
- Freeze the Voxtype configuration per meeting so a mid-meeting configuration change cannot mix engines or models.
- Persist session state, audio chunks, jobs, events, and a canonical Markdown transcript below the service data directory.
- Regenerate the transcript in chronological utterance order with source labels and timestamps.
- Optionally retain utterance audio; when disabled, delete each audio chunk only after its transcript is durably stored.
- Recover recording state and pending transcription work independently of the Omarchy Shell process.

## Meeting options

- Configure source, microphone device, system-output device, and audio retention in the popup.
- Choose which of Voxtype's Whisper language options (`auto`, `en`, `fr`, `de`, `it`, `es`, `pt`, `nl`, `pl`, `zh`, `ja`, `ko`, `ru`, `ar`) appear as chips on the meeting page. The list matches Engine → Whisper · language and does not shrink for `.en` models.
- Show the active Voxtype engine and model as read-only values.
- Prevent capture-option edits while a meeting is active.

## Recent meetings and exports

- Show the five most recent meetings with title, date, duration, status, and utterance count.
- Export and open a transcript that has not been exported yet; later opens reuse that exported file in the default text editor.
- Copy canonical Markdown transcripts to `~/Documents/Meetings` by default when export/open needs it.
- Use filesystem-safe, collision-resistant filenames and never silently overwrite an export.
- Preserve existing exported Markdown during migration from Voxtype's native meeting mode.

## Availability and lifecycle

- Detect a missing or unreachable service and surface actionable errors.
- A user systemd service owns capture independently of the bar widget.
- The installer builds and installs the service, enables it for the user, and leaves system Omarchy files untouched. No sudo or pkexec is required.
- The matching uninstaller stops the user unit and removes the installed binary and unit file. It does not delete meeting transcripts or Voxtype configuration.
- The plugin is installed and removed with `omarchy plugin add` / `omarchy plugin remove` as user-owned configuration under `~/.config/omarchy/plugins/`.

## Explicitly excluded

- Voxtype's native meeting capture/database.
- Remote transcription services.
- Tooltip behavior.
- AI summarization, speaker-label editing, automatic recording, or automatic deletion of exports.
