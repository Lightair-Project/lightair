# Screen Studio in Lightair: research and integration plan

Status: research done, nothing built yet. Goal: every **offline** Screen Studio capability exists
in Lightair, done the Lightair way (live-first, multi-format, on-device), with an original design
and none of Screen Studio's assets, wording or branding. Lightair becomes three places: the
**Library** (every recording, by project), **Live** (today's studio) and the **Editor**.

Scope: everything runs on the Mac. Screen Studio's online features are left out: shareable links,
comments and view counts, cloud transcription, cloud voices, online background search, project
sharing through links, team accounts.

Researched 14 November 2026 from Screen Studio's own site (home page, roadmap, ~30 guide pages)
and coverage of its 4.0 (5 October 2026) and 4.1 (26 October 2026) releases. Sources are at the
end. The official changelog page renders no entries, so release dates come from third-party
coverage.

## 1. What Screen Studio is

A macOS screen recorder (Ventura 13.1+, best on Apple silicon; $9/month yearly or $29 monthly, all
features in every plan) whose output looks edited without editing. Its central trick, in its own
words: it **records the screen without the mouse cursor, records precise mouse data alongside, and
puts the cursor back in at export**. Because the cursor, clicks and keys are data rather than pixels,
it can smooth the cursor, resize or restyle it, zoom to where you clicked, blur motion and loop the
cursor, all after recording.

Its pipeline is **record raw tracks, then edit, then render**:

```text
Record ─► project bundle ─────────────────────────► Editor (timeline) ─► Render ─► MP4 / GIF / ProRes
          screen (no cursor), cursor + clicks +       zooms, layouts, cuts,        on-device, faster
          keys, camera, mic, system audio             speed, masks, captions…      than real time
```

Lightair today is the other half of that world: a **live compositor** (libobs) with scenes,
layers, multiple output formats (canvases) at once, Camera Studio, an audio engine, streaming and
recording. It records what the program shows; it has no editor. So integrating Screen Studio
means two things:

1. **Live**: the effects that can run in real time become part of Lightair's capture layers and
   scenes, on stream and in recordings: drawn cursor, click effects, follow-zoom, keystrokes,
   captions, camera layouts, backgrounds.
2. **Studio takes and the Editor**: a recording mode that keeps the raw tracks and input data, and
   an Editor with a timeline and an offline renderer for everything that needs hindsight:
   auto-zoom placed before the click, cuts, edit-by-transcript, typing speed-ups, masks, re-aiming
   per format.

## 2. Library, Live, Editor

A switcher at the top left of the window moves between three places (also in the command palette
and the View menu, with keyboard shortcuts):

```text
┌ Library ┬ Live ┬ Editor ┐
```

### Library

Every recording Lightair made, sorted by project.

- **Projects down the side** (the packages in Lightair's Projects folder), plus **All recordings**.
  Selecting a project shows its recordings newest first, grouped by day.
- **Each item** is a card with a thumbnail, title, date, length and kind:
  - recording (program, one file per format)
  - studio take (raw tracks and input data)
  - clip (replay)
  - multitrack audio
  - edited export
- **Filters and search** by kind, format (wide, vertical…), date and title. Markers are listed
  inside an item and searchable.
- **Actions:**
  - **Open in Editor**, **Play** (Quick Look, full-screen preview).
  - **Reveal in Finder**, **Rename**, **Duplicate**.
  - **Move to project** (an older recording made under the wrong project).
  - **Move to Trash**: recoverable from the Trash, and Lightair asks first. Nothing is ever
    deleted outright.
- **Storage** per project and in total, with what can be cleaned up (raw takes already exported,
  old clips). Lightair suggests; the user confirms.

Where it comes from: today, recordings, clips and multitrack files land in one folder
(`~/Movies/Lightair` or the folder set in Recording settings), with nothing tying a file to the
project that made it. Each project package gets a **recordings index**
(`recordings/index.json`): an entry per item, with its id, kind, files, start time, length,
formats, markers, title and thumbnail. Every recording, clip, take and export adds its entry when
it finishes. The media files stay where the user chose; the index points at them, and a moved or
missing file shows as such, with **Locate…**. **Add existing recordings…** brings older files from
a folder into a project's index.

### Live

Today's studio: scenes, layers, formats, mixer, outputs. It gains a **Studio take** record button
(section 4.2) and the live Screen Studio effects (drawn cursor, follow-zoom, keycaps, captions,
camera layouts). Everything it records appears in the Library as soon as it finishes, and the
post-recording card offers **Open in Editor**.

### Editor

Edits one Library item at a time:

- **Takes:** full features (cursor, auto-zoom, keys).
- **Ordinary recordings and imported videos:** manual zooms, cuts, captions, masks, layouts, audio.

Its project is the item's edit file. Exports go back into the Library under the same project,
linked to the item they came from. Details are in section 4.2.

## 3. Feature inventory, mapped to Lightair

Status: **Has** (Lightair already does it), **Part** (pieces exist), **New**.
Path: **Live**, **Edit** or **Both**.
Phase: see section 5.

### Recording

| Screen Studio | What it does | Lightair | Path | How | Phase |
|---|---|---|---|---|---|
| Display, window or area | Pick what to record | Has (display/window/app capture via ScreenCaptureKit; crop) | Both | A **Studio take** records the chosen capture layer raw | 2 |
| Webcam, mic, system audio | Recorded with the screen | Has (camera layers, audio engine, desktop/app audio) | Both | Take keeps each as its own track (multitrack stems already exist) | 2 |
| Cursor recorded as data | Screen captured without cursor; cursor path, clicks, cursor shapes logged | New | Both | **Input recorder** (section 4.1); capture layer with the cursor off | 2 |
| Pause and resume | Stop mid-take, continue | Has (recording pause) | Both | Take pauses all tracks and the input log together | 2 |
| Countdown | Seconds to get ready | Part (timer layer) | Both | Countdown before a take, shown on screen, not recorded | 2 |
| One shortcut to start/stop | Global hotkey | Has (global hotkeys) | Both | Add "Start/stop studio take" action | 2 |
| Crash recovery (saved every 2 s) | A take survives power loss | Part (libobs recording) | Both | Fragmented MP4/MOV for every track; input log flushed every second; recover on launch | 2 |
| Recording flag (⌘⌥⌃F) | Marks moments on the timeline | Has (markers) | Both | Markers land on the take's timeline | 2 |
| Speaker notes | Talking points on screen, not recorded | New | Both | Floating notes window excluded from capture (same exclusion as Lightair's own windows) | 3 |
| Resize recorded window | Set a window to an exact size in one click | New | Live | Accessibility API resize (asks the user for Accessibility access; never changed for them) | 4 |
| Hide desktop icons | Clean desktop while recording | New | Live | Exclude Finder's desktop windows from the capture filter (no system setting changed) | 3 |
| Hide Dock icon / recording widget | Controls not in the recording | Part (Lightair excludes its own windows) | Live | Small take widget, excluded from capture | 2 |
| iPhone/iPad over USB | Record a device's screen with a cable | New | Both | CoreMediaIO "screen capture devices" exposed as a camera input (libobs's capture supports this); device bezels drawn by Lightair (our own artwork, no Apple marketing frames) | 4 |
| iPhone Mirroring | Record the mirrored iPhone window | Has (window capture) | Both | Preset that picks the iPhone Mirroring window | 4 |
| Import any video | Edit an existing video like a take | New | Edit | Take from a file: video track, no input log (manual zooms only) | 3 |
| Multi-clip recordings (roadmap) | Several takes in one project | New | Edit | Editor timeline holds several takes | 5 |

### Zoom

| Screen Studio | What it does | Lightair | Path | How | Phase |
|---|---|---|---|---|---|
| Auto zoom | Zooms to where you clicked | New | Both | Edit: zoom segments generated from clicks (clustered), editable. Live: **follow-zoom** eases to the click area after it happens (no hindsight); optional program delay (0.5–1 s) lets live zoom start before the click | 3 |
| Manual zoom | Zoom to a chosen point and level | New | Both | Zoom segment with a fixed target; live as a scene action or hotkey | 3 |
| Instant zoom | Zoom with no animation | New | Both | Segment flag | 3 |
| Zoom level per zoom | How close | New | Both | Segment parameter (1.25×–4×) | 3 |
| Classic and Loupe zoom (4.0 "Glass Loupe") | Whole-frame zoom, or a magnifier over the area | New | Both | Two zoom kinds: frame zoom (crop/scale of the capture layer) and loupe (circle or rounded magnifier with lens edge, rest of frame visible) | 3 |
| Smart framing | Zooms avoid the webcam covering the cursor | New | Both | Zoom target solved against the camera's rectangle in each format | 4 |
| Screen animation: Focused / Smooth | Spring settings for screen movement | New | Both | Spring presets on the zoom engine | 3 |
| Sharper zoomed text (on-device AI) | Super-resolution when zoomed in | New | Edit | First: render from the full-resolution capture (recorded at native Retina size). Later: MetalFX upscaling (Apple's), no custom model | 5 |
| Always keep zoomed in (vertical) | In a narrow format, the frame follows the cursor | Part (formats, auto-reframe) | Both | Per-format follow-crop: in a vertical canvas the screen layer is cropped and follows the cursor | 3 |

### Cursor

| Screen Studio | What it does | Lightair | Path | How | Phase |
|---|---|---|---|---|---|
| Smooth cursor | Shaky paths become glides | New | Both | Critically damped spring on the logged path; live adds ~1 frame of latency | 2 |
| Cursor size and style after recording | Bigger, different look | New | Both | Lightair draws the cursor: system shapes at high resolution, plus our own styles (touch dot, outline, filled) | 2 |
| Sharp system cursors | Hi-res replacements as cursors grow | New | Both | Our own vector cursor set covering macOS's standard shapes | 2 |
| Hide cursor / hide when idle | Remove it, or fade when resting | New | Both | Draw flags with idle timeout | 2 |
| Hide cursor in sections | Per timeline range | New | Edit | Range flag | 3 |
| Loop cursor position | Returns to start at the end | New | Edit | End-of-clip path blend | 3 |
| Rotate while moving | Slight tilt that trails motion | New | Both | Rotation from velocity | 2 |
| Remove shakes / optimise cursor types | Filter jitter and rapid shape flicker | New | Both | Jitter filter; minimum hold time for shape changes | 2 |
| Always use default cursor | Ignore I-beam and other shapes | New | Both | Draw flag | 2 |
| Disable smoothing in a section | Exact movement where needed (menus) | New | Edit | Range flag | 3 |
| Click effects (circle, ripple) | Visible clicks | Part (display capture "highlight clicks" was designed, not built) | Both | Drawn effects at logged clicks | 2 |
| Click sounds | Soft click per click | New | Both | Short sample through the audio engine, on its own track | 3 |
| Motion blur (cursor, zoom, pan) | Optics-like blur on fast motion | New | Both | Velocity-based blur shader: cursor sprite and zooming screen layer; strengths per kind | 3 |
| Touch cursor for device recordings | A dot instead of an arrow | New | Both | Style option | 4 |

### Camera

| Screen Studio | What it does | Lightair | Path | How | Phase |
|---|---|---|---|---|---|
| Position, size, roundness, mirror | Camera bubble | Has (layout, shape, flip) | Both | Camera styles as layer presets | 2 |
| Smaller during zooms | Camera shrinks when the screen zooms | New | Both | Camera size follows the zoom engine's state | 3 |
| Layouts: Fullscreen camera, Split screen, Camera overlay, Screen only, Camera cutout | Switch within one recording, transitions animate | Has (scenes, Move transition, background removal) | Both | Live: layout scenes with Move transitions. Edit: a layouts track with the same five layouts, animated | 2 (live) / 3 (edit) |
| Camera cutout | Background removed on the Mac | Has (Camera Studio background removal, Apple Vision) | Both | Cutout layout uses it | 2 |
| Face tracking | Keeps the face centred | Part (face tracker in Camera Studio; auto-frame designed) | Both | Build Camera Studio's auto-frame | 3 |
| Hide during silence | Camera steps aside when you stop talking | New | Both | Voice activity from the audio engine drives the layout (with hold and fade) | 3 |
| Camera looks, LUTs, sharpen | Colour grading and detail | Has (grades, LUT filter); sharpen New | Both | Add a sharpen effect (edge-aware) to the effect catalog | 3 |
| Camera crop | Keep part of the camera picture | Has (crop) | Both | No work | — |
| Portrait / Studio Light / backgrounds (Apple's) | macOS camera effects | Has (system camera effects panel) | Live | No work | — |

### Audio

| Screen Studio | What it does | Lightair | Path | How | Phase |
|---|---|---|---|---|---|
| Noise reduction, "Podcast" voice | On-device voice cleanup | Has (Voice Clarity, noise suppression, EQ, compressor) | Both | "Podcast" = a Voice Clarity preset; Edit applies it offline too | 2 |
| Set volume per section | Volume on a range | Has (track fader); range New | Edit | Volume envelope on the timeline | 3 |
| Background music, looping | A track under the voice | Has (media layer, ducking) | Both | Edit: a music track that loops to length, ducked under the voice. Library: user's files only (no bundled music) | 3 |
| AI voiceover | Typed script becomes a voice | New | Edit | On-device speech only (AVSpeechSynthesizer, including the user's Personal Voice) | 5 |

### Editing

| Screen Studio | What it does | Lightair | Path | How | Phase |
|---|---|---|---|---|---|
| Trim, cut, speed up | Basic edits | New | Edit | Timeline ranges: cut, speed (0.25×–8×) | 3 |
| Speed up typing | Detects typing stretches, offers a speed-up | New | Edit | Detected from the key log | 3 |
| Edit by transcript | Delete text to cut; remove fillers and long pauses | New | Edit | Word-timed transcript (4.4) drives cuts | 4 |
| Scrubber with audio | Drag to hear | New | Edit | Editor playback with audio scrub | 3 |
| Crop | Keep part of the recording | Has (crop) | Both | No work | — |
| Copy frame as image | Snapshot to clipboard | Part (screenshot of program) | Both | Copy the editor's current frame | 3 |
| Command menu (⌘K) | Every setting by keyboard | Has (command palette) | Both | Add editor commands | 3 |
| Undo | Every edit | Has (document undo) | Edit | Editor edits are document commands | 3 |

### Look and framing

| Screen Studio | What it does | Lightair | Path | How | Phase |
|---|---|---|---|---|---|
| Background: wallpaper, gradient, colour, image | Behind the recording | Part (colour/image layers) | Both | "Backdrop" for the screen layer: colour, gradient, image, the Mac's own wallpapers, any image in Assets (Screen Studio's online image search is left out) | 2 |
| Padding, rounded corners, inset, shadow | Recording floats on the backdrop | Part (layout shape for corners) | Both | Layer style: padding, corner radius, inset, drop shadow (angle, distance, softness) | 2 |
| Any aspect ratio (Wide 16:9, Vertical 9:16, Square 1:1, Classic 4:3, Tall 3:4) | Re-target after recording | Has (formats, per-format layouts, auto-reframe) | Both | Editor renders every format of the project; zooms re-aim per format | 3 |
| Reel mode (Instagram/TikTok overlay) | Check nothing hides under platform UI | Has (safe areas) | Both | No work | — |
| Captions (on-device Whisper or Apple speech; styles, animations, fonts, colours, position) | Readable captions | Part (captions widget designed, not built) | Both | Shared speech engine (4.4): live captions layer and editor caption track, styles in our design system | 4 |
| Show shortcuts (keycap overlay; size; single keys; per-shortcut disable) | Keys pressed appear on screen | New | Both | Key log (4.1) to a keycap layer; live and edited | 3 |
| Masks and highlight | Cover a region, or dim all but one | Part (pixelate, blur effects) | Both | Mask/highlight regions on a timeline; live as a mask layer | 3 |
| Hide sensitive data and faces (4.1) | Detects emails, phones, cards, keys and faces, covers them | New | Both | Vision text recognition plus pattern matching, Vision face detection; edit: every frame; live: sampled with tracking | 5 |
| Animations: motion blur, cursor and screen springs | Feel of motion | New | Both | Section 3.3 | 3 |
| Presets: whole look, shared with a team | Consistency | Part (looks, templates, prefabs, scene assets) | Both | "Studio preset" file (backdrop, styles, cursor, zoom, captions) in Assets | 3 |

### Export and sharing

| Screen Studio | What it does | Lightair | Path | How | Phase |
|---|---|---|---|---|---|
| MP4 720p/1080p/4K at up to 60 fps | Export | Has for live recording | Edit | Offline render, VideoToolbox H.264/HEVC | 3 |
| GIF up to 1080p, looping | Small loops for docs | New | Edit | GIF encoder with good palettes; check licences (gifski and libimagequant are AGPL/GPL: don't link them unless the licence fits) | 4 |
| ProRes 422 LT/422/HQ/4444 (4.1) | For Final Cut, Resolve, Premiere | Part (libobs ProRes on Apple) | Both | ProRes in the recording formats and in export | 3 |
| Copy video to clipboard | Paste into Slack or a doc | New | Edit | Pasteboard file URL after export | 3 |
| Extract raw tracks | Original screen, camera, mic files | New | Edit | A take *is* raw files: "Reveal raw files" | 2 |
| Export transcript (.srt matching cuts) | Subtitles file | New | Edit | From the caption track after cuts | 4 |
| Hardware encoding, faster exports | Media engine | Has (VideoToolbox) | Edit | Render pipeline in section 4.2 | 3 |
| Preview quality modes (Quality, Performance, Power saving) | Editor playback | New | Edit | Editor preview settings | 3 |
| Share whole project | Teammate keeps editing | New | Edit | Offline: **Export package**, a take with its edits as one file (AirDrop, a drive); **Import package** adds it to a project's Library | 4 |
| Quick share after recording | Save, copy, edit right away | New | Both | Post-recording card: Open in Editor, Copy, Reveal, Show in Library | 3 |
| Shareable links, comments, view counts | Hosted sharing | — | — | Left out (online) | — |

### Not carried over

- **Reactions**: Apple's camera reactions are a macOS feature (Screen Studio's guide only explains
  how to turn them off). Lightair's Camera Studio help can do the same.
- **Licensing, accounts, affiliates**: Lightair has no paid tiers (docs/02, "Lightair has no paid
  tiers").
- **Everything online**: shareable links (comments, private links, view counts), cloud
  transcription, cloud AI voices, online background search, sharing projects through links,
  team accounts, and Screen Studio's planned MCP server.

## 4. The new parts

### 4.1 Input recorder

Logs, at up to 120 Hz while a take or live cursor effects run:

- **cursor position** in display points, with a timestamp in the capture clock;
- **clicks** (button, down/up, double), **scrolls**, and the **cursor's shape and hot spot**, so it
  can be redrawn at any size;
- **keys** for the shortcut overlay and typing detection. Modifiers and keys only, never stored
  as text. Typing-detection keeps only timing.
- **focused app and window frame**, for auto-zoom targets and "resize recorded window".

Permissions: mouse position needs none. Clicks and keys need **Input Monitoring** (a listen-only
event tap). Lightair asks the user and explains why, and never changes privacy settings itself.
Without the permission, Studio takes still work: no clicks means no auto-zoom (manual zooms only),
and no keys means no keycaps. The log is a compact binary file next to the take's media, flushed
every second for crash safety.

### 4.2 Studio takes and the Editor

A **take** is a folder:

```text
Take 2026-11-14 10.32.lightair-take/
  screen.mov        capture layer, cursor off, native resolution, fragmented (crash-safe)
  camera.mov        each camera layer in the scene
  mic.wav, system.wav …   stems (the multitrack recorder already writes these)
  input.bin         section 4.1
  take.json         timing, devices, formats, markers
  edit.json         the edit: tracks below (an undoable document)
```

The **Editor** (section 2) shows a preview per format, and a timeline with
tracks for:

- clips: cuts and speed
- zooms
- layouts
- masks
- captions
- keys
- audio

Everything is a document command, so undo, the command palette and presets work as they do now.

**Rendering.** libobs is a real-time compositor: it can't seek, or render faster than real time,
deterministically. The editor needs both, so the plan adds a small offline renderer, a new crate
`lightair-render`:

- **Decode:** AVFoundation (`AVAssetReader`).
- **Composite:** Metal (through `wgpu`, or Metal directly), with Lightair's effect shaders ported
  to its shading language.
- **Encode:** VideoToolbox (`AVAssetWriter`).

The same renderer draws the editor's scrubbing preview.

Lightair's live effects keep running in libobs. Shaders used in both paths (cursor, zoom, motion
blur, backdrop and shadow, masks) are written once and compiled for both, to keep live and edited
output identical. **Decision to confirm:** the dedicated renderer, rather than driving libobs
offline. That is the largest technical choice in this plan.

### 4.3 Zoom and motion engine

One engine, used live and in the editor:

- **Segments:** start and end, kind (frame or loupe), target (auto: follow the clicks in the
  segment; manual: a point), level, and instant or animated.
- **Auto zooms** come from click clusters: clicks close in time and space make one segment, with
  lead-in and hold times, and are merged when they overlap.
- **Springs:** a critically damped spring moves zoom, pan and cursor, with Focused (settles fast)
  and Smooth presets.
- **Framing:** targets are clamped to the screen layer and solved per format. The camera's
  rectangle is kept clear (smart framing). In narrow formats, follow-crop applies.
- **Motion blur:** velocity from the springs drives a directional blur on the screen layer and the
  cursor sprite, with a strength per kind.

Live, the engine runs on the Lightair side and sets the screen layer's crop and scale each frame,
plus a cursor overlay drawn by a new libobs filter. Edited, it runs in `lightair-render`.

### 4.4 Speech engine

One on-device speech-to-text service for live captions (designed as CH-10) and the editor:

- **Engine:** Apple's speech recognition (SpeechAnalyzer, macOS 26+) where available, otherwise a
  bundled Whisper model (Base/Small/Medium, as Screen Studio offers). Check the model licences;
  whisper.cpp and the Whisper weights are MIT.
- **Output:** word timings, which drive edit-by-transcript, filler and pause removal, caption
  timing and .srt export.

### 4.5 Offline

Everything in this plan runs on the Mac, with no account and no network: recording, input data,
rendering, transcription, voice cleanup, voiceover, masks and detection, export. Nothing in a
recording leaves the Mac unless the user copies or moves the file themselves.

## 5. Phases

Each phase ships with tests and screenshots, like the audio plan.

1. **Library and the switcher:**
   - the Library / Live / Editor switcher (the Editor shows "coming soon" until phase 3);
   - recordings index in each project;
   - every recording, clip and multitrack file registers itself;
   - Library view: projects, cards with thumbnails, filters, search, markers, Quick Look, Reveal,
     Rename, Duplicate, Move to project, Move to Trash (asked first), storage;
   - Add existing recordings; missing files with Locate.

   *Tests:* index entries written on every finished recording, clip and stem set; a moved file is
   found by Locate; trash never deletes outright. *Screenshots:* Library by project, empty state.
2. **Studio take and a drawn cursor (live):**
   - input recorder;
   - takes (raw tracks, crash-safe, markers, pause), in the Library;
   - capture layer with the cursor drawn by Lightair: size, styles, smoothing, rotation, idle hide,
     click effects;
   - backdrop and layer style (padding, corners, inset, shadow);
   - camera layout scenes; "Podcast" voice preset;
   - take widget, countdown, start/stop hotkey; post-recording card.

   *Tests:* the input log is aligned with the screen track to within one frame, and a take
   survives a killed process. *Screenshots:* cursor styles, backdrop.
3. **Editor and zoom:**
   - `lightair-render`;
   - the Editor with cuts, speed, zoom track (auto and manual, frame and loupe, springs, motion
     blur), layouts track, masks and highlight;
   - keycap overlay, typing speed-up, click sounds, background music track, volume ranges;
   - hide cursor and disable smoothing per range, loop cursor, camera smaller during zooms,
     face-tracking auto-frame, hide-during-silence;
   - per-format render with follow-crop;
   - MP4 and ProRes export into the Library, copy to clipboard, presets;
   - editing ordinary recordings and imported videos.

   *Tests:* render is deterministic (the same edit gives the same frames), auto-zoom segments from
   synthetic click logs, export at 4K60 runs faster than real time on Apple silicon.
4. **Words and devices:**
   - on-device speech engine, captions (live and edited, styles), edit by transcript, filler and
     pause removal, .srt export;
   - GIF export; export/import package;
   - iPhone/iPad over USB with our own bezels; resize recorded window; smart framing;
   - speaker notes.
5. **Extras (all on-device):**
   - sensitive-data and face detection;
   - AI voiceover with the Mac's own voices;
   - super-resolution zoom (MetalFX);
   - multi-take projects.

## 6. Where Lightair goes further

These follow from Lightair being a live, multi-format studio rather than a recorder:

- **Live**: drawn cursor, click effects, follow-zoom, keycaps, captions and camera layouts on a
  stream, not only in an export.
- **Every format at once**: a take renders the wide, square and vertical versions together, each
  with its own zoom targets and layouts (Lightair's formats already hold per-format layouts).
- **Real audio engine**: per-track effects, plugins, multitrack stems and loudness targets,
  instead of a single voice preset.
- **Scenes and assets**: intro and outro scenes, prefabs and scene assets placed into an edit.

## 7. Open questions

- **Renderer:** confirm the dedicated offline renderer, and whether live and edit share shader
  source (section 4.2).
- **Scope of the editor:** a focused screen-recording editor (Screen Studio's scope) or a general
  video editor. This plan assumes the former.
- **Library location:** this plan keeps media where the user records it and indexes it per
  project. The alternative is moving recordings inside each project package; that's tidier, but
  heavy for big files and breaks the recording folder setting.
- **Switcher shortcuts:** ⌘1–⌘9 already take scenes on air, so the switcher needs others
  (proposed ⌃1/⌃2/⌃3).
- **Input Monitoring:** without it there's no auto-zoom or keycaps. Is a degraded mode without the
  permission acceptable, or should Studio takes require it?
- **Assets we must make ourselves:** cursor set, device bezels, wallpapers and caption styles.
  Nothing is copied from Screen Studio or Apple marketing material.

## Sources

- [Screen Studio home page](https://screen.studio/): current features, privacy model, pricing
- [Screen Studio roadmap](https://screen.studio/roadmap): completed, in-progress and planned features
- [Screen Studio guide](https://screen.studio/guide), including:
  - [Auto zoom](https://screen.studio/guide/auto-zoom)
  - [Adding and editing zooms](https://screen.studio/guide/adding-editing-zooms)
  - [Cursor](https://screen.studio/guide/cursor)
  - [Animations](https://screen.studio/guide/animations)
  - [Camera](https://screen.studio/guide/camera)
  - [Dynamic camera layouts](https://screen.studio/guide/dynamic-camera-layouts-)
  - [Background](https://screen.studio/guide/background)
  - [Aspect ratio](https://screen.studio/guide/aspect-ratio)
  - [Captions](https://screen.studio/guide/captions)
  - [Shortcuts](https://screen.studio/guide/shortcuts)
  - [Masks and highlight](https://screen.studio/guide/adding-a-mask-and-highlight)
  - [Speed up typing](https://screen.studio/guide/speed-up-typing-segments)
  - [Export settings](https://screen.studio/guide/explanation-of-export-settings)
  - [Performance settings](https://screen.studio/guide/performance-settings)
  - [Recording iPhone and iPad](https://screen.studio/guide/recording-iphone-ipad)
  - [Quick share widget](https://screen.studio/guide/quick-share-widget-)
  - [System requirements](https://screen.studio/guide/system-requirements)
- [AlternativeTo: Screen Studio 4.0](https://alternativeto.net/news/2026/10/screen-studio-4-0-adds-transcript-editing-a-camera-cutout-tool-and-more-mouse-effects/) (5 October 2026)
- [AlternativeTo: Screen Studio 3.0](https://alternativeto.net/news/2024/12/screen-studio-3-0-launched-with-shareable-links-dynamic-layouts-command-menu-and-more) and [3.1 masks](https://alternativeto.net/news/2025/3/screen-studio-3-1-adds-new-privacy-feature-masks)
- [Product Hunt: Screen Studio 4.0](https://www.producthunt.com/posts/screen-studio-4-0)
