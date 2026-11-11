# Lightair · Implementation Plan

The design docs in [`docs/`](docs/README.md) describe the finished product. This file is the **build order**: milestones that each end in something that runs, in an order where each one stands on the last. No dates; a milestone is done when its checklist is.

Status key: ✅ done · 🔨 in progress · ⬜ not started

## M0 · Foundation ✅
- Cargo workspace with the crate layout from [03 · Architecture](docs/03-architecture.md#crate-graph).
- GPL-3.0-or-later licence, `.gitignore`, README.
- `cargo xtask` entry point.

## M1 · Engine bootstrap ✅
- `libobs-sys`: bindgen bindings against the pinned libobs headers (33.0.0).
- **Dev runtime**: link against an installed OBS 33 (`/Applications/OBS.app`) and assemble a dev `Lightair.app` bundle whose `Frameworks/` and `PlugIns/` point at OBS's, so OBS core modules resolve their rpaths ([04](docs/04-engine-libobs.md), [15](docs/15-platform-distribution-licensing.md#building)).
- `lightair-engine`: engine actor thread; `obs_startup`, audio/video reset, core module loading with the safe-module allowlist, capability discovery, clean shutdown.
- Headless smoke test: start the engine, create a colour source in a scene, render frames, shut down.

## M2 · Native window with live preview ✅
- `lightair-app` opens a GPUI window (gpui-kit root).
- `lightair-preview`: libobs renders the program canvas into its own render target, a GL blit copies it into a BGRA IOSurface, and VideoToolbox converts it on the GPU to full-range NV12 (the format GPUI's `surface()` draws). No CPU copies.
- A display capture or camera visible live in the window.

## M3 · Core document ⬜
- `lightair-core`: Project / Format / Scene / Layer / Input / Track / Output model with stable IDs, commands, validation, undo/redo with gesture coalescing.
- Engine reconciler applies patches to libobs (idempotent).
- `lightair-store`: project package read/write, autosave journal, recovery.

## M4 · Studio shell ⬜
- `lightair-ui` tokens and theme (from [06](docs/06-design-system.md)); gpui-component themed.
- Title bar (health strip, Go Live / Record / Clip), Layers, Scenes, Canvas, Mixer, Outputs, Inspector, status bar, command palette.
- Canvas editing overlays: select, move, resize, rotate, crop, snapping.

## M5 · Audio
Tracks from inputs, faders, meters (volmeter rings), mute/cue, mixes and routing, Voice Clarity and the built-in effect set ([10](docs/10-audio.md)).

## M6 · Outputs
Recording, streaming (RTMP/RTMPS/SRT/WHIP), multistream with encoder sharing, Go Live review, clips/replay, virtual camera ([11](docs/11-outputs-streaming-recording.md)).

## M7 · Formats
Vertical and custom canvases (`obs_canvas_*`), per-format layouts, side-by-side editing, per-format outputs ([07](docs/07-scenes-layers-canvas.md#formats)).

## M8 · Effects & transitions
`lightair-video-fx` module: effect catalog, presets; Move/Morph/Stinger/Wipe transitions; studio mode ([08](docs/08-video-effects.md), [07](docs/07-scenes-layers-canvas.md#transitions)).

## M9 · Camera Studio
Port the Dewy pipeline into `lightair-camera-fx` and the native Camera Studio panel ([09](docs/09-camera-studio.md)).

## M10 · Layers & creator features
Text, shapes, counters/timers, browser layer (CEF), media, phone camera, NDI, screen draw, templates, automations ([07](docs/07-scenes-layers-canvas.md)).

## M11 · Audience & control
Platform sign-in, stream manager, unified chat, activity, alerts, widgets, captions, hotkeys, Stream Deck, MIDI/OSC, remote API ([12](docs/12-chat-alerts-integrations.md)).

## M12 · Compatibility & import
OBS plugin classifier, frontend API shim, custom browser docks, Qt runtime, OBS scene collection import ([13](docs/13-extensibility-and-compatibility.md), [14](docs/14-data-persistence-settings.md#importing-from-obs)).

## M13 · Engine from source & distribution
Build libobs and core modules from the submodule, bundle, sign, notarise, updater, Windows and Linux ports ([15](docs/15-platform-distribution-licensing.md)).

## M14 · Quality
Performance budgets in CI, golden renders, A/V sync, soak tests, accessibility and localisation passes ([16](docs/16-quality-performance-accessibility.md)).
