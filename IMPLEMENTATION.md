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

## M3 · Core document ✅
- `lightair-core`: Project / Format / Scene / Layer / Input / Track / Output model with stable IDs, commands, validation, undo/redo with gesture coalescing.
- Engine reconciler applies patches to libobs (idempotent).
- `lightair-store`: project package read/write, autosave journal, recovery.

## M4 · Studio shell 🔨
Done:
- Design tokens applied to gpui-component's theme; Lucide icons.
- Window anatomy from [05](docs/05-interface-shell.md): title bar with health strip and primary actions, Layers, Scenes (ON AIR tally), live Canvas, Mixer strip, Outputs, Inspector, status bar.
- Canvas editing: click to select (topmost visible layer), drag to move with snapping guides to canvas edges and centre (⌘ disables), corner-handle resize (⇧ keeps aspect), arrow-key nudge, delete, Esc to deselect.
- Every edit is a command: journaled, undoable (⌘Z / ⇧⌘Z), one undo step per drag gesture; scene switching is not undone.
- Add layer: display, image (file picker), text, colour. Add scene. Layer visibility and lock.
- Inspector: position, size, rotation steppers; fit mode; fill canvas; delete.

Remaining:
- `lightair-ui` crate with the studio components (scrubbable fields, `LayerRow`, `SceneCard`, `FormatTabs`).
- gpui-kit dock layout (resize, collapse, pop-out, workspaces); command palette (⌘K).
- Rename layers/scenes inline, drag to reorder, groups, multi-select, rotation and crop handles, rulers and guides.
- Studio mode, onboarding, settings window.

## M5 · Audio 🔨
Done:
- `Track` and `Effect` in the document (commands, undo, diff, persisted in `audio.json`).
- Reconciler: each global track's input sits on its own output channel; volume, mute and cue (monitoring) map to libobs; effects are libobs audio filters, rebuilt when the chain's shape changes and updated in place otherwise.
- Lock-free meter cells fed by libobs volume meters; the end-to-end test checks a −6 dBFS tone reads −6.0 and drops by exactly the fader amount.
- Mixer panel: channel strips with effect list (bypass dot, remove, presets), L/R meters with zones and falling peaks, dB-taper fader (one undo step per drag, double-click for unity), Cue, Mute, input name. Microphone and desktop-audio tracks; new mic tracks get the Voice Clarity chain.

Remaining:
- Voice Clarity as one native effect with calibration (`lightair-audio-fx`), plus EQ, de-esser, ducking, reverb, delay, pitch.
- Mixes and routing (Stream, Recording n, Cue, VOD), master chains, LUFS meters.
- Scene tracks, per-scene volume (link/unlink), folders, channel remap, sync offset and auto-sync, VST3/AU host, hardware mixer integrations.

## M6 · Outputs 🔨
Done:
- Engine output manager: recording (hybrid MP4, MKV, MOV) and RTMP(S) streaming, each with its own encoders; Apple hardware H.264 picked automatically. libobs output signals drive a runtime status table (starting, active, reconnecting, stopping, failed with a plain-language reason).
- End-to-end test, verified with ffprobe: a recording has H.264 + AAC of the right length, and a stream reaches a local ffmpeg RTMP server.
- Output settings and destinations in the document (`outputs.json`); stream keys only in the Keychain.
- Clips: a libobs replay buffer kept running while Clips is on (length and format per project); the Clip button saves the last seconds; a test saves a 3 s clip and probes it. libobs writes clips through OBS's `obs-ffmpeg-mux` helper, which `cargo xtask dev` / `bundle` link next to the executable.
- A headless self-test inside the app bundle (`LIGHTAIR_SELFTEST=camera`) confirms camera frames with Camera Studio attached.
- UI: Record with live timer, Go live for every enabled destination, LIVE badge and live canvas border, two-click End stream, Outputs panel with recording status, destination rows (toggle, status, remove) and an add-destination form.

Remaining:
- Encoder sharing between destinations with identical settings; per-destination formats and bitrates.
- Go Live review sheet, stream manager (titles, categories), platform sign-in (with M11).
- Clip library (browse, trim, share); virtual camera, NDI, SRT/WHIP, ISO and multitrack recording, pause/split, markers, test recording.

## M7 · Formats 🔨
Done:
- Every scene exists once per format: one libobs scene per (scene, format) in that format's canvas (`obs_canvas_create`, activate + scene-ref, audio mixed once), sharing inputs. Program switches every canvas. The reconciler syncs formats from the snapshot, and a test checks incremental reconciliation matches a full rebuild across two canvases.
- Previews for any canvas (`obs_canvas_render` into the preview pipeline).
- Format tabs, side-by-side editing (the pointer edits whichever stage it is over), "Add vertical" with auto-reframe (backgrounds fill, other layers keep their arrangement in a centred band). Layers panel and inspector follow the active format; new layers get a layout in every format.
- Recording and each destination can target a format (aspect badge); a vertical recording is 1080×1920.

Remaining:
- Per-format visibility toggle in the UI, copy layout from another format, safe-area overlays, square and custom formats, smarter reframe (face-aware with Camera Studio), Twitch Dual Format.

## M8 · Effects & transitions 🔨
Done:
- Transition settings in the document (Cut, Fade, Move; duration). Every format's output channel goes through its own libobs transition.
- Move: a temporary scene per format where layers that continue (same input, then same name) glide from A's layout to B's with easing, driven by a libobs tick callback; entering layers fade in, leaving layers fade out; the canvas then lands on B. A test samples the glide halfway and checks the landing.
- Transition picker (Cut / Fade / Move, duration) under Scenes.

Remaining:
- Morph, Stinger (track mattes), wipes, slide; per-scene and per-pair overrides; per-format transitions; studio mode (preview/program, Take).
- `lightair-video-fx` effect catalog with the effect stack UI and presets; masks; layer animations.

## M9 · Camera Studio 🔨
Done:
- `modules/lightair-camera-fx`: the Dewy pipeline (pigment-model skin, Vision tracking, Core ML segmentation, makeup, effects, atmosphere, colour, light) built in-tree as a libobs module without Qt or the frontend API, rebranded (`lightair_camera_studio` filter, "Camera Studio"); `cargo xtask modules` builds it. A test loads it, renders through it, and checks it adds no lag once warm.
- Input effects in the document (applied wherever the input appears), with replace-not-merge settings so a look resets what it doesn't set.
- Camera layers from the device list; new cameras get Camera Studio (off until a look is chosen). Inspector section: on/off, every look by category, and skin controls.

Remaining:
- The full Camera Studio panel (Shape, Makeup, Effects, Atmosphere, Colour, Light tabs; thumbnails rendered from the user's face; before/after; saved looks; look packs).
- Background blur/remove/replace, auto-frame, multi-face (new features beyond Dewy).
- Verify with a live camera (needs the macOS camera permission for Lightair).

## M10 · Layers & creator features
Text, shapes, counters/timers, browser layer (CEF), media, phone camera, NDI, screen draw, templates, automations ([07](docs/07-scenes-layers-canvas.md)).

## M11 · Audience & control
Platform sign-in, stream manager, unified chat, activity, alerts, widgets, captions, hotkeys, Stream Deck, MIDI/OSC, remote API ([12](docs/12-chat-alerts-integrations.md)).

## M12 · Compatibility & import
OBS plugin classifier, frontend API shim, custom browser docks, Qt runtime, OBS scene collection import ([13](docs/13-extensibility-and-compatibility.md), [14](docs/14-data-persistence-settings.md#importing-from-obs)).

## M13 · Engine from source & distribution 🔨
Done:
- `cargo xtask bundle`: a development `Lightair.app` (Info.plist with camera/microphone usage strings, the brand icon, OBS frameworks and plugins linked from an installed OBS 33, ad hoc signature), so macOS asks Lightair itself for privacy permissions.

Remaining:
- Build libobs and core modules from the submodule; release bundle with everything copied in; Developer ID signing and notarisation (needs your certificate and Apple ID); updater; Windows and Linux ports.

## M14 · Quality
Performance budgets in CI, golden renders, A/V sync, soak tests, accessibility and localisation passes ([16](docs/16-quality-performance-accessibility.md)).
