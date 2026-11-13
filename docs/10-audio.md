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
| **Bus** | A strip fed by the tracks routed to it (a group: "Guests") or by sends (an effects return: a shared reverb). Routes to Program or another bus, never in a loop |
| **Program** | The master: what the stream and the recording's main track carry. Its own effects (typically a limiter) and meter |
| **Mix** | An extra output mix built from per-track **sends** (pre or post fader): headphones, a VOD mix without the music, a mix-minus for a remote guest. Up to five reach libobs as extra recording/stream tracks; a mix can also feed Program (an effects return, such as a shared reverb). **Cue** is the monitoring mix |

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
| **Audio plugins (AU / VST3)** | Lightair's plugin host: Audio Units (v2 and v3; AUv3 load out of process) and VST3, listed under **Plugins** in the effect sheet. The plugin's own editor opens in its own window (an AU without one gets Apple's generic view); its state is saved in the project when the editor closes. VST3 bundles without a `moduleinfo.json` are probed in a helper process, so a plugin that crashes on load is only skipped |

Video-related audio features:

- **Sync offset** per track and **auto-sync**: a clap test (visual + audio) measures camera/mic offset and sets it automatically.
- **Audio-reactive** effects and layers (Audio Visualiser layer, Beat Pulse camera motion) subscribe to any track's levels.

## Templates

**Settings → Audio → Start from a template** (or **Templates…** in the mixer's **+** menu) sets the mixer up in one undoable step, replacing its tracks, buses and mixes (scenes stay):

| Template | Sets up |
|---|---|
| **Streaming** | Mic (Voice Clarity), Desktop audio, Music (an app, ducked under the mic, −12 dB), a limiter at −1 dB on Program, a **VOD (no music)** mix |
| **Podcast** | A track per mic (an interface's channels when it has more than two), Voice Clarity on each, a **Guests** bus, a Remote guest (an app) with a **Mix-minus**, a gentle compressor and limiter on Program, every voice recorded to its own file; loudness target −16 LUFS |
| **Music** | Vocal and Instrument (an interface's first two channels), cued for live monitoring, a **Reverb** bus fed by sends, 24-bit files of each track, the 128-frame buffer |
| **Gaming** | Mic, Game and Chat apps as tracks (chat also in headphones), a **No chat** mix for recordings |

## Monitoring

- **Cue mix** goes to a chosen monitoring device (headphones) with its own level control and an optional "Stream mix" mode to hear exactly what viewers hear.
- **Per-track monitoring mode**: off / cue only / always monitor (useful for instruments).
- **Low-latency monitoring** (on by default): the cue (or the stream mix) goes straight from the engine to the device, a few milliseconds behind, for singing and playing along. Off, libobs monitors it (~100 ms).
- **Buffer** (Settings → Audio): the engine's block, 128, 256 or 512 frames (2.7, 5.3 or 10.7 ms at 48 kHz). Changes restart the engine at once (not while multitrack files record).
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

The mixer runs in Lightair's own real-time engine, `lightair-audio` ([plan](plans/audio-engine.md)):

- **Inputs**: interfaces are opened directly (CoreAudio), every channel available to tracks. Sources only libobs can capture (app and system audio, media and browser layers) are pulled in through libobs audio-capture callbacks and kept out of libobs's own mixes.
- **Graph**: compiled from the project's tracks, buses, mixes and master; swapped in without a click, keeping effect state; no allocation or locks on the audio thread. Effects are the engine's processors; plugins are hosted inserts.
- **Outputs**: Program goes back to libobs as an audio source in mix 1, the extra mixes in mixes 2–6, which the stream and recording encoders take. Multitrack files are written by the engine (WAV, sample-aligned).
- **Meters**: the engine's (peak, RMS, gain reduction; LUFS on mixes), read by the UI 30 times a second.
- `LIGHTAIR_LEGACY_AUDIO=1` runs the first cut instead (libobs mixing, effects as libobs filters), for comparison.
