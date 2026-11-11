# 10 · Audio

Audio is where streams are won or lost: viewers forgive soft video, not bad sound. Lightair treats audio like a real mixer: **tracks** with effect chains, **mixes** that feed outputs, honest meters, and monitoring that works.

## Model

```text
Inputs ──► Tracks (effect chain, gain, mute, pan) ──► Mixes ──► Outputs
 mic        "Mic"   [Voice Clarity][Gain]          Stream     ► Stream destinations
 app audio  "Game"  [Gain]                         Recording  ► Recording (multitrack)
 desktop    "Music" [Ducking ← Mic][Limiter]        Cue        ► Monitoring device
 layer      "Video" [Gain]                         Clip/ISO   ► Clips, ISO files
```

| Concept | Definition |
|---|---|
| **Input** | Where sound comes from: microphone/interface channel, system audio, an application's audio, a media or browser layer's audio, a phone camera's mic, NDI/SRT input audio |
| **Track** | One channel strip in the mixer. Has one input, an effect chain, gain, pan/balance, mute, solo-in-cue, sync offset, and mix routing. Users name tracks ("Mic", "Game") |
| **Mix** | A summed bus that feeds outputs: **Stream**, **Recording**, optional **Recording 2…n** (multitrack), **Cue** (monitoring). Implemented on libobs audio mixes (up to 6) |
| **Master** | Each mix has a master section with its own effect chain (typically a limiter) and meter |

### Track sources

| Track input | Notes |
|---|---|
| **Microphone / interface input** | Selects device and **channel(s)** (for multi-channel interfaces, pick input 3 or 3+4 as stereo) |
| **System audio** | Everything the computer plays (macOS via ScreenCaptureKit; Windows WASAPI loopback) |
| **Application audio** | One or more specific apps (game, Spotify, Discord), via ScreenCaptureKit app filtering (macOS) and WASAPI process loopback (Windows). Includes "everything except these apps" |
| **Layer audio** | Media files, browser layers, captured windows, video inputs with embedded audio (capture cards, NDI, SRT, phone camera). Created automatically when such a layer is added, linked to the layer |
| **Virtual / loopback inputs** | Third-party virtual devices still work as microphone inputs |

Tracks created from layers are **attached** to their layer: renaming or deleting the layer offers to do the same to the track, the strip shows the layer's icon, and **Reveal source layer** jumps to it (switching scenes if needed).

## Scene-aware tracks

| Track kind | Behaviour |
|---|---|
| **Global track** | Exists in every scene (mic, system audio, music app). Added with **+** in the mixer |
| **Scene track** | Belongs to a layer in a scene (a media file's audio, a browser layer). Shown only while that scene is active, unless "Show all tracks" is on. **Promote to global** turns it into a global track |

- **Link / unlink (🔗)**: a linked track has one volume and mute everywhere. Unlinking gives the track a **per-scene volume and mute**: e.g. game audio at −6 dB in "Gameplay" and muted in "Just Chatting". The fader shows a small scene marker when the value is scene-specific.
- **Ordering**: drag to reorder globally; ⇧-drag reorders for the current scene only. Sort by name, by kind, or manually. **Folders** group tracks.
- **View**: current-scene tracks or all tracks; badges show special roles (**VOD-EXCL** for tracks excluded from the Twitch VOD, **STT** for the track feeding captions).
- **Channel remap**: any input channel to any track channel (up to 32 input channels), stereo/mono switch, **stereo panner**.

## Mixer UI

The mixer sits under the canvas ([05](05-interface-shell.md)). Each track is a `ChannelStrip`:

```text
┌─ Mic ─────────── ▾ ┐
│ ● Gain          ⚙ │  ← effect rows: active dot (click = bypass), name, edit
│ ● Voice Clarity ⚙ │
│        +           │  ← add effect (searchable picker)
│ ▐▐  │  [EDIT]      │  ← meter (peak/RMS, peak hold, clip latch) and fader
│ ▐▐  │              │
│ ▐▐  ┼  [🔗]        │
│ ▐▐  │  [CUE]       │
│ ▐▐  │  [MUTE]      │
│ [ MacBook Pro Mic ▾]│ ← input selector
└────────────────────┘
```

- **EDIT** expands the strip into the Inspector with all parameters: effect chain detail, pan/balance, mono/stereo/downmix, sync offset (ms, ±2000), mix routing checkboxes, and monitoring mode.
- **CUE** sends this track to the Cue mix (headphones). Multiple tracks can be cued. Cue is independent of what's on air, so you can check music before playing it.
- **MUTE** mutes the track in all mixes. Mute state is large, red, and impossible to miss. Optional **push-to-talk / push-to-mute** hotkeys per track.
- **Fader** is post-effect gain into the mixes; −∞ to +12 dB, unity marked.
- **Mix routing** per track: by default all tracks feed Stream and Recording; a track can be excluded from Stream (e.g. copyrighted music only in local monitoring) or sent only to a specific recording track.
- Tracks can be grouped into **folders** (e.g. "Guests") with a group fader; collapsed folders show one combined meter.
- **Narrow mode** collapses strips to name + meter + mute, for small screens or during live shows.

### Meters

- Peak and RMS (or LUFS-M, selectable) per channel, with peak hold and a clip latch.
- Scale: 0 to −60 dBFS; zones: green below −20, yellow −20 to −9, red above −9.
- Master sections show **integrated loudness (LUFS-I)** and true peak with per-platform targets (e.g. −14 LUFS for YouTube) and a "Normalise" helper that sets the master limiter and gain to hit the target.
- Meters update every display frame while visible and stop when hidden.

## Built-in audio effects

All effects are native (the `lightair-audio-fx` module), real-time safe, and share the same `EffectList` UI as video effects. Each has presets, A/B compare, and bypass.

| Effect | Description |
|---|---|
| **Gain** | Trim −30 to +30 dB, polarity invert |
| **Voice Clarity** | One-knob vocal chain: noise suppression → gate → de-esser → EQ (high-pass, warmth, presence) → compressor → limiter, tuned by a short calibration (5 s speech + 5 s silence). Amount as levels (Low, Moderate, High, Very high) or continuous, plus optional automatic gain. Expands to show each stage. The default on new mic tracks |
| **Noise suppression** | Neural (DeepFilterNet-class model, on-device) with strength control; light DSP mode (RNNoise-class) for low-power machines |
| **Noise gate / expander** | Threshold, range, attack, hold, release, look-ahead, with a visual gate indicator on the meter |
| **Compressor** | Threshold, ratio, knee, attack, release, makeup, auto-makeup; gain-reduction meter in the strip |
| **Limiter** | True-peak brickwall, ceiling, release |
| **Parametric EQ** | 8 bands (high/low pass, shelves, bells, notch) with an interactive frequency-response curve over a live spectrum analyser |
| **De-esser** | Frequency-targeted sibilance reduction with listen mode |
| **Ducking (sidechain)** | Lower this track when another track (or a set of tracks) is active: amount, threshold, attack, release, hold. Typical: music ducks under the mic |
| **Reverb** | Algorithmic and convolution (with bundled impulse responses) for intro music and effects |
| **Delay** | Time (ms or tempo-synced), feedback, mix; also used as a sync tool |
| **Pitch / voice changer** | Pitch shift and formant, presets (deeper, robot, radio, telephone) |
| **Channel tools** | Mono, swap L/R, downmix, channel select, stereo width |
| **Audio plugins (VST3 / AU)** | Lightair's plugin host: scans installed plugins, shows native plugin editors in a floating window, saves state in the project. Optional per-plugin isolated process |

Video-related audio features:

- **Sync offset** per track and **auto-sync**: a clap test (visual + audio) measures camera/mic offset and sets it automatically.
- **Audio-reactive** effects and layers (Audio Visualiser layer, Beat Pulse camera motion) subscribe to any track's levels.

## Monitoring

- **Cue mix** goes to a chosen monitoring device (headphones) with its own level control and an optional "Stream mix" mode to hear exactly what viewers hear.
- **Per-track monitoring mode**: off / cue only / always monitor (useful for instruments).
- **Talkback** (later): a dedicated mic track that only reaches the cue mix of remote guests.

## Output routing

| Output | Audio |
|---|---|
| Stream destinations | Stream mix (per destination you can choose another mix, e.g. a music-free mix for platforms with stricter rules) |
| Twitch VOD track | A **VOD mix** (Stream mix minus tracks marked "Exclude from VOD") sent as Twitch's second audio track; can also be used as the main audio for other destinations |
| Recording | Recording mix as track 1; optional extra tracks: each mix and/or each **individual track** (multitrack recording for editing) |
| Clips (replay) | Same configuration as recording |
| ISO recording | Each track as a separate file or track |
| Virtual camera | Video only; optional **virtual microphone** output carrying the Stream mix to calls (Zoom, Discord) |
| NDI / SRT outputs | Chosen mix |

## Hardware mixers

BEACN, Elgato Wave Link and GoXLR integrations expose the hardware/software mixer's channels and mixes as Lightair tracks (e.g. "Wave Link Stream", BEACN "Audience Mix"), with level and mute kept in sync in both directions.

## Health & safety

- **Silence detection**: a live mic that's been silent for N seconds while you're live raises a gentle warning ("Your mic seems silent").
- **Clipping warning** with suggested gain change.
- **Device lost**: strip shows the missing device, keeps its settings, reconnects automatically.
- **Sample rate mismatch** handling is automatic (libobs resampling) and invisible except in diagnostics.
- **Global mute**, **privacy cut**, and **mute all except** hotkeys.

## Engine mapping

- Each track = the input's `obs_source_t` (or a Lightair wrapper source for app/channel captures) + its audio filters (Lightair effects are filters).
- Fader = `obs_source_set_volume`; mute = `obs_source_set_muted`; balance = `obs_source_set_balance_value`; sync = `obs_source_set_sync_offset`; routing = `obs_source_set_audio_mixers` bitmask, where Lightair allocates mix indices.
- Master chains are implemented by a per-mix Lightair audio output processor (patch candidate upstream: per-mix filters), or, without the patch, by applying master processing inside the output's audio encoder pipeline.
- Meters use one `obs_volmeter` per track and per master.
- Monitoring uses libobs audio monitoring with the selected device; cue is a Lightair-managed set of monitored sources.
