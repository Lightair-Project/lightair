# 11 · Outputs: Streaming, Recording, Clips & Virtual Camera

Everything that leaves Lightair is an **output**: a stream destination, a recording, the clip buffer, the virtual camera, an NDI feed. The Outputs panel lists them all with one consistent row design.

## Outputs panel

```text
OUTPUTS                                        +
 ▶  My Channel · YouTube        [16:9]  ◯ ──   (toggle: included when you go live)
 ◆  studio · Twitch             [16:9 + 9:16 Dual]  ●
 ♪  TikTok                      [9:16]  ◯
 ⦿  Recording                   [16:9] MKV · HEVC
 ✂  Clips                       [16:9 + 9:16] 60 s
 ◎  Virtual camera              [16:9]  Install
 ⟁  NDI · "Lightair Program"    [16:9]  ◯
```

- Each row is an `OutputRow`: icon, name (renameable), format badge(s), state (idle, armed, connecting, live, reconnecting, error), a toggle that decides whether it starts with **Go Live**, and a health dot.
- Selecting a row shows its settings in the Inspector. While live, the Inspector shows bitrate graph, dropped frames, congestion, encoder load, uptime and viewers (where the platform API provides them).
- **Add (+)**: platform sign-in, custom destination (RTMP/RTMPS, SRT, WHIP, RIST), recording, clips, virtual camera, NDI.
- **Output presets** ("Twitch + YouTube", "Recording only") switch which outputs are enabled.

## Streaming

### Destinations

| Destination | Connection | Notes |
|---|---|---|
| **Twitch** | OAuth sign-in; ingest auto-selection | **Enhanced Broadcasting** (client-side multi-rendition encoding; automatic max bitrate and renditions), **Dual Format** (landscape + vertical in one Twitch stream), VOD track, stream markers |
| **YouTube** | OAuth; pick or create a broadcast; stream key selection | Auto-start/auto-stop broadcast, scheduled broadcasts, RTMPS or HLS (for HEVC/AV1), vertical Shorts live via a separate destination on the vertical format |
| **Kick** | OAuth | |
| **TikTok** | OAuth where available, or server + key | Vertical format by default |
| **X** | Sign in; create source (broadcast name, region); generate key | |
| **Facebook** | OAuth (pages, profiles, groups where API allows) | |
| **LinkedIn**, **Trovo** | OAuth or key | |
| **Custom RTMP / RTMPS** | URL + key | |
| **SRT** | Caller/listener, latency, passphrase, stream ID | |
| **WHIP** | Endpoint + bearer token | Sub-second WebRTC ingest |
| **RIST** | URL, profile, encryption | |
| **Relay** | Any relay service (one upload, fanned out in the cloud) | Useful on limited upload; configured like a custom destination with a relay preset |

Platform sign-in uses OAuth with PKCE in the system browser; tokens go to the OS credential store ([14](14-data-persistence-settings.md#security-of-stored-data)).

### Multistreaming

- **Unlimited destinations**, each with its own on/off toggle, format and quality.
- **Encoder sharing**: destinations with identical video settings (format, resolution, FPS, codec, bitrate, preset) share one encoder automatically; the Encoder view shows the sharing graph. Changing one destination's bitrate splits it onto its own encoder, with a note about the extra cost.
- **Add or stop a destination mid-stream** without affecting the others.
- **Per-destination audio mix** (e.g. a music-free mix for one platform, the VOD mix as main audio elsewhere).
- **Change bitrate while live** for encoders that support it.
- Upload estimate in the Go Live sheet sums all distinct encoders against the measured bandwidth.

### Stream manager

Per-platform metadata in one place (Outputs → selected platform, or the Go Live sheet):

| Platform | Fields |
|---|---|
| Twitch | Title, category (search), tags, content classification labels, language, branded content |
| YouTube | Title, description, privacy, category, thumbnail upload, made for kids, latency mode, DVR, broadcast selection |
| Kick | Title, category, tags, mature flag |
| TikTok / X / Facebook | Title/description fields their APIs expose |

**Edit several at once**: a shared title field fans out to all selected platforms, with per-platform overrides. Saved **stream info presets** ("Tuesday dev stream") fill everything with one click.

### Go Live review

The **Go Live** button opens a review sheet (can be turned off in Settings):

1. **Destinations** with toggles, format badges and account names.
2. **Metadata** per destination, with presets.
3. **Pre-flight checks** with plain-language results:
   - Encoder available and not overloaded in a 2-second dry run.
   - Upload estimate vs measured bandwidth.
   - Mic levels in the last 10 s (warns on silence or clipping).
   - Camera present and not frozen.
   - Disk space for recording/clips.
   - Missing sources in the starting scene.
4. **Options**: also start recording, start on scene ("Starting Soon"), countdown.
5. **Go live** starts every enabled output together.

While live, the primary button turns red with elapsed time; its menu lists each destination with stop/restart and status.

### Reliability

- **Automatic reconnect** per destination with exponential backoff, a visible countdown and **Cancel**, plus a "Retry now" action. An orange banner explains failures in plain language.
- **Dynamic bitrate** lowers bitrate under congestion and recovers gradually.
- **Low-latency mode**, **bind to network interface** (e.g. force Ethernet), and **TCP tuning** options in Advanced.
- **Stream delay** (seconds) with "preserve cutoff point" on reconnect.
- **Bandwidth test mode**: stream to a platform's test ingest without going public.
- **Stats**: bitrate, dropped frames (network vs encoder vs render, separated), congestion, RTT where the protocol provides it.

### Encoding

| Setting | Options |
|---|---|
| Video encoder | VideoToolbox H.264/HEVC (macOS), NVENC H.264/HEVC/AV1, AMF, QSV, x264, SVT-AV1, AOM AV1; only installed/usable encoders are offered |
| Rate control | CBR (default for streaming), VBR, CQP/CRF (recording), with sensible defaults |
| Bitrate | Per destination; platform maximums shown |
| Preset | Encoder-specific quality/speed presets normalised to a 1–7 scale (Fastest → Best quality), with encoder-native names in Advanced |
| Keyframe interval | 2 s default (platform requirements enforced) |
| B-frames | Auto, or off for platforms/encoders that need it |
| Profile / tuning | Advanced |
| Output resolution | Format resolution or downscaled per encoder, scale filter choice |
| Audio | AAC 96–320 kbps, Opus where supported |

## Recording

| Setting | Options |
|---|---|
| Format | Any format (landscape, vertical) or **both** as separate files |
| Container | **Fragmented MP4/MOV** (default: crash-safe and editor-friendly), MKV, MP4, MOV, FLV, hybrid MP4 |
| Video codec | H.264, HEVC, AV1, **ProRes 422/4444** (macOS), lossless (FFV1/UT) |
| Encoder | "Same as stream" (shares the stream encoder, zero extra cost) or independent settings |
| Quality | CQP/CRF quality presets (High, Indistinguishable, Lossless) or bitrate |
| Audio tracks | Up to 6 named tracks + master; each track = a mix or individual tracks ([10](10-audio.md#output-routing)) |
| Folder & naming | Folder per project; templates with date, time, project, format, counter |
| Split | By time, by size, manual split hotkey |
| Pause | Pause/resume into the same file |
| Auto-record | Start when streaming starts; stop when streaming stops |
| Markers | Chapter markers via hotkey/API/Stream Deck, written into MP4/MOV/MKV chapters and a sidecar JSON (with scene changes, clips and alert events as optional automatic markers) |
| Remux | Automatic MKV → MP4/MOV after recording (optional) |

### ISO recording

Records **each selected input** (cameras, display capture, phone camera, NDI input) as its own clean file, **pre-effects or post-effects**, plus each audio track as a separate file or track. All files share timecode and start together, with an edit-ready folder structure and an XML/FCPXML/EDL describing scene switches, so editors can re-cut the show.

### Test recording

**Record → Test recording** captures 15–60 s using the exact stream encoder settings and opens an in-app **review player**: video, meters, waveform, A/V sync indicator, loudness. "Looks good, go live" and "Change settings" buttons.

## Clips

- **Enable clips** runs a background replay buffer (RAM, or disk for long buffers) per enabled format. Buffer length 10–300 s (default 60 s).
- **Clip** button, hotkey, Stream Deck, chat command (mods) or API saves the buffer for every enabled format (landscape + vertical) at once. The API accepts a custom duration up to the buffer length.
- A **toast** appears with a thumbnail and actions: **Replay** (adds a Replay layer to the canvas), **Trim**, **Reveal**, **Share**.
- **Clip library** (Activity → Clips): browse by session, trim in/out (lossless where possible), rename, tag, delete, export, and share to platforms (YouTube Shorts, TikTok, X) through connected accounts.
- **Replay layer**: plays a clip on canvas with scrubbing, A/B loop, playback speed (slow-motion), its own audio track, and auto-dismiss after it ends or after N seconds.
- Clip container, codec and folder are configurable separately from recordings.

## Virtual camera

- Any **format** (landscape or vertical), program or a chosen scene, at format resolution.
- **macOS**: a CoreMediaIO Camera Extension ("Lightair Virtual Camera"), approved once in System Settings; frames are passed through IOSurface with no CPU copy.
- **Windows**: virtual camera registered by the installer.
- **Virtual microphone** (optional): the Stream mix exposed as a system audio input, so calls get the processed voice (Voice Clarity included).
- Turning the virtual camera on and off never restarts the engine. Changing its format is allowed while off.

## NDI output

- Send program, preview, a scene, or a format as named NDI sources, with audio (chosen mix).
- Optional NDI tally from Lightair's program/preview state.
- NDI runtime is detected; Lightair explains how to install it if missing (licensing prevents bundling in some cases).

## Screenshots

**Take screenshot** (F2 by default) saves the selected format, or all formats, as PNG/JPEG at canvas resolution; optional copy to clipboard.

## Engine mapping

| Lightair | libobs |
|---|---|
| Stream destination | `obs_output_t` (`rtmp_output`, `ffmpeg_mpegts_muxer` for SRT/RIST, `whip_output`) + `obs_service_t` |
| Shared encoder | One `obs_encoder_t` attached to several outputs |
| Format | The encoder's video comes from that format's canvas (`obs_canvas_get_video`) |
| Recording | `ffmpeg_muxer` / `mp4_output` (hybrid/fragmented) |
| Clips | `replay_buffer` per format |
| ISO | Per-input private views/encoders feeding separate muxers (Lightair `lightair-outputs` helpers) |
| Enhanced Broadcasting | libobs multitrack video output support with the Twitch configuration service |
| Virtual camera | Lightair output writing to the camera extension's frame queue |
| NDI | Lightair output using the NDI SDK at runtime |
