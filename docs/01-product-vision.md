# 01 · Product Vision & Principles

## The one-line pitch

**Lightair is a native live-production studio for creators: the power of OBS, the feel of a modern creative tool, and everything you used to install a plugin for, already inside.**

## The problem

OBS Studio is the most capable free broadcasting engine in the world. Its interface is not.

- **The UI is a 2012 control panel.** Docks of list widgets, modal property dialogs, settings split across six windows, and a "Filters" window that hides the most important part of a camera setup behind a right-click.
- **Basic things need plugins.** Move transitions, vertical/dual-format streaming, multistreaming, source clones, per-app audio routing, noise suppression that sounds good, a usable text source, chat overlays, replay "clip" buttons, beauty/background effects. Each plugin has its own UI language, update cadence, and compatibility risk every time OBS updates.
- **Nothing is discoverable.** Power is buried. New users bounce; experienced users build muscle memory around workarounds.

A streaming app can feel like Figma or Logic: a clear layer model, inline effects, a real audio mixer, and a canvas you edit directly. The apps that come closest are closed source, and they aren't built on the engine that the streaming ecosystem has standardised on.

## The answer

Lightair keeps **libobs**, the engine inside OBS (capture, compositing, encoding, outputs, the plugin ecosystem), and replaces everything the user touches with a **native Rust interface built on GPUI and gpui-kit**. Features that OBS users get from plugins are first-party, designed together, and share one interaction model.

## Who it's for

| Persona | Who they are | What they need from Lightair |
|---|---|---|
| **The solo streamer** | Streams games or Just Chatting 3–5×/week, one camera, one mic, maybe a phone as a second camera. | Looks great and sounds great in minutes. Face looks good on camera. Chat on screen. Vertical clips for TikTok/Shorts without a second app. |
| **The creator-educator** | Records tutorials, podcasts, webinars; streams occasionally. | Clean screen + camera layouts, per-app audio, ISO recordings for editing, reliable "record a test first" workflow. |
| **The power user / producer** | Runs shows, multiple scenes, Stream Deck, multiple destinations. | Keyboard-driven, scriptable, remote-controllable, multistream with per-destination formats, stinger and move transitions, studio mode. |
| **The migrating OBS user** | Has a scene collection and a dozen plugins. | Imports their OBS setup, and finds that the plugin they relied on is now a built-in feature. |

## Product principles

These settle design arguments. When two principles conflict, the higher one wins.

1. **Live is sacred.** Nothing in the interface may ever drop a frame, stall audio, or interrupt an output. UI work never runs on the engine's threads; engine work never waits on the UI. Risky changes during a live show are guarded (confirmations only where an action is destructive *and* visible to the audience).
2. **Built in, not bolted on.** If streamers routinely install a plugin for it, it's a first-party feature designed into the model. A feature is only "done" when it looks, behaves, saves, undoes, and is remote-controllable like every other feature.
3. **Direct manipulation over dialogs.** Edit on the canvas. Adjust in place. Properties live in an inspector beside the work, never in modal windows. Every dialog is a design failure to justify.
4. **One model, many views.** A layer is a layer, whether you see it in the layer list, on the canvas, in the vertical format, or through the remote API. One source of truth, one undo stack, one set of names.
5. **Fast and honest.** The app tells the truth about health: dropped frames, encoder overload, disk space, network, audio clipping, are visible at a glance, in plain language, with a suggested fix.
6. **Calm by default, powerful on demand.** The default surface is small. Depth is revealed progressively: hover reveals, an "Edit" button expands, a command palette reaches everything.
7. **Native everywhere.** Native rendering, native windows, native permissions, native performance. Lightair should use less CPU and memory than OBS's own frontend.
8. **Open engine, open format.** GPL-3.0. Project files are documented and versioned. Your setup is yours.

## What Lightair is not (non-goals)

- **Not an OBS skin.** Lightair never runs OBS's Qt frontend. Source, filter, encoder, output and service plugins work directly. Plugins that use OBS's frontend work through a compatibility layer, and their Qt panels load only through an opt-in Qt runtime (see [13 · Extensibility](13-extensibility-and-compatibility.md#compatibility-layer-for-frontend-plugins)).
- **Not a video editor.** Recording workflows end at a clean file (and markers, ISO tracks, clips). Trimming beyond clip in/out points belongs to editors.
- **Not a cloud service.** Lightair works fully offline. Integrations with platforms (sign-in, chat, metadata) are optional conveniences, never requirements.
- **Not a copy of another app.** Lightair has its own visual identity and its own decisions where they serve users better.

## Positioning

| | OBS Studio | Streamlabs Desktop | **Lightair** |
|---|---|---|---|
| Engine | libobs | libobs (Electron UI) | **libobs** |
| UI technology | Qt Widgets | Electron/React | **Rust + GPUI (GPU-native)** |
| Source | Open (GPL-2) | Open (GPL-3) | **Open (GPL-3)** |
| Move transitions, vertical, multistream | Plugins | Partly built in, some paid | **Built in** |
| Inline effects & real mixer | No | Partial | **Yes** |
| Beauty / face-aware camera | Plugins | No | **Built in, pigment-accurate** |
| OBS plugin compatibility | Native | Partial | **Source/filter/output plugins** |
| Resource overhead | Medium | High | **Low** |

## Signature experiences

These are the moments the product is designed around. Each one is a design test: if a change makes one of these worse, it's wrong.

1. **First launch to live in under five minutes.** Permissions explained in one screen. Camera, mic and display detected. A starter scene built from what's connected. Sign in to a platform, review, Go Live.
2. **"My face looks good."** Turn on the camera and pick a Look. Skin, light and colour are fixed in one tap, and it still looks like you.
3. **"My voice sounds good."** Voice Clarity is on the mic track by default: noise removed, levelled, de-essed, gently compressed. One knob for the amount.
4. **One show, two shapes.** Every scene exists in landscape and vertical at once. Arrange both, stream both, record both.
5. **Clip that.** One press saves the last 30–120 seconds, in both formats, named and ready to post.
6. **Rehearse before you go live.** A test recording plays back inside the app so you can hear and see exactly what viewers will get.
7. **Edit live without fear.** Every change is undoable. Studio mode lets you stage the next scene privately, then take it with a move transition.

## Success measures

Qualitative and quantitative signals the team watches. None are deadlines.

- A new user completes the first-run flow and goes live or records without opening Settings.
- Migrating OBS users import a scene collection and reach parity without installing a plugin.
- UI never causes a skipped or lagged frame in soak tests (see [16 · Quality](16-quality-performance-accessibility.md)).
- Lightair's process uses measurably less CPU than OBS 33 with an equivalent scene.
- Every action in the UI is reachable from the keyboard, the command palette, and the remote API.
