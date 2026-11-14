# Windows: every Lightair feature on both platforms

Status: phase 1 done locally (everything compiles and passes clippy for `x86_64-pc-windows-msvc`;
the CI workflow is written, not yet run: the repository has no remote). Lightair is designed for macOS and Windows (docs/15: Windows 10 22H2+ and 11, x64;
Linux after Windows), but so far the code has only been built and run on macOS, and about a dozen
parts are macOS-only. From now on, **every feature is designed for both**: a new feature names its
Windows path before it's built, and nothing lands that would stop the Windows build.

## 1. Principles

- **One product, two platforms.** The same features, the same project files, the same UI. Where an
  OS offers something the other doesn't, Lightair builds its own (an eyedropper on Windows) or
  says plainly that the feature is for one platform (Audio Units).
- **Projects are portable.** A project made on a Mac opens on Windows with its scenes, layouts,
  mixer and effects intact. Machine-specific choices (which camera, which display, which app) are
  re-picked, not broken.
- **The platform is behind one seam.** OS calls live in a `lightair-platform` crate with a macOS
  and a Windows backend. The rest of the code is cross-platform Rust, libobs and GPUI.
- **Same code, same tests.** The suite runs on both, in CI, from the first Windows phase.

## 2. Where Lightair is macOS-only today

| Area | Today (macOS) | Windows | Kind of work |
|---|---|---|---|
| **libobs build** | Links an installed `OBS.app`'s frameworks (`libobs-sys/build.rs`) | Build libobs from the pinned `third_party/obs-studio` (D3D11 backend) or link a matching OBS install's `obs.dll`; bindgen as now | Build |
| **Preview** | IOSurface → `CVPixelBuffer` → GPUI `surface()` (`lightair-preview/mac.rs`) | DXGI shared NT handles from libobs (D3D11) into GPUI's Windows renderer; GPUI's `surface()` is macOS-only, so Lightair adds a Windows backend (docs/03, docs/04) | New backend |
| **Project source kinds** | Projects store libobs ids: `screen_capture`, `sck_audio_capture`, `av_capture_input_v2`, `coreaudio_input_capture`, … | **Portable kinds** in the project (display, window, app audio, desktop audio, camera, mic, interface), mapped to libobs ids per platform when played: `monitor_capture`/`window_capture`/`game_capture`, `wasapi_output_capture`, `wasapi_process_output_capture`, `dshow_input`, `wasapi_input_capture`. Settings translated (display UUID ↔ monitor id, app bundle id ↔ executable). Existing projects migrated once | Model + engine |
| **Encoders** | VideoToolbox (H.264, HEVC, ProRes) | NVENC, AMF, QSV, x264 (libobs `obs-nvenc`, `obs-qsv11`, `win-mf`); ProRes via FFmpeg (software) | Engine settings |
| **Camera Studio** (`modules/lightair-camera-fx`) | Apple Vision / Core ML segmentation and tracking (`backend-vision.mm`, `segmenter-coreml.mm`) | The module already has ONNX backends (`backend-onnx.cpp`, `mediapipe.cpp`): ONNX Runtime with DirectML on Windows. The module's quality picker shows what each platform has | Build + test |
| **macOS camera effects panel** (`system_effects.rs`) | Portrait, Backgrounds, Studio Light | Windows Studio Effects where the camera and PC support them (blur, eye contact, framing); hidden otherwise | Platform |
| **Audio engine** | cpal on CoreAudio; real-time thread via Mach time-constraint policy (`audio_host.rs`, not guarded) | cpal on WASAPI; real-time thread via MMCSS ("Pro Audio"); guard the Mach code | Platform |
| **App and desktop audio** | ScreenCaptureKit audio (`sck_audio_capture`) | WASAPI process loopback (per app) and desktop loopback | Kinds (above) |
| **Running apps list** (source menu, `mixer.rs`) | NSWorkspace | Top-level windows' processes (for process loopback) | Platform |
| **Plugins** | Audio Units (AudioToolbox) and VST3 (CFBundle loading) | VST3 only: load the bundle's `Contents/x86_64-win/*.vst3` DLL (`InitDll`/`ExitDll`), editors attach to an `HWND`. Audio Units are macOS-only by nature; the Plugins page shows what the platform has | Platform |
| **Plugin editor windows** (`plugin_editor.rs`) | NSWindow | Win32 window hosting the VST3 view | Platform |
| **Main-thread dispatch** (`dispatch2` in `main.rs`) | GCD main queue | GPUI's foreground executor (cross-platform; replaces GCD on both) | Refactor |
| **Global hotkeys** (`hotkeys.rs`) | Carbon `RegisterEventHotKey` | `RegisterHotKey` | Platform |
| **Eyedropper** (`eyedropper.rs`) | NSColorSampler | Lightair's own: a full-screen capture with a magnifier loupe | New |
| **Stream keys** | Keychain (`keyring` with `apple-native`) | Credential Manager (`keyring` with `windows-native`) | Feature flag |
| **Paths** | `~/Library/Application Support/Lightair`, `~/Movies/Lightair` | `%APPDATA%\Lightair`, the user's Videos folder (Known Folders) | Platform |
| **Reveal, Trash** | Finder reveal, macOS Trash | Explorer `/select`, Recycle Bin | Platform |
| **Displays** (`displays.rs`, guarded) | CoreGraphics display UUIDs | DXGI outputs / monitor ids | Platform |
| **Virtual camera** (designed) | Camera Extension | DirectShow / Media Foundation virtual camera (docs/03) | Already planned |
| **Updates, signing** (designed) | Sparkle, notarisation | Signed feed updater, Authenticode, MSIX (docs/15) | Already planned |
| **Fonts, colours, UI** | GPUI | GPUI on Windows (Direct Write); check gpui-kit's Windows support at the pinned version | Verify |

Already cross-platform: the core model and commands, the store and journal, the audio graph and
DSP (`lightair-audio`), templates, the effect catalog and its libobs shaders (libobs compiles them
for D3D11), scenes, layers, transitions, formats and the mixer UI.

## 3. The `lightair-platform` crate

One module per concern, each with `macos.rs` and `windows.rs` behind `#[cfg]`, and the
same public functions:

- `paths`: config, projects, recordings, caches.
- `files`: reveal, move to trash, open with.
- `hotkeys`: global shortcuts.
- `credentials`: secrets.
- `apps`: running apps with windows and audio.
- `displays`: monitors with stable ids.
- `realtime`: real-time audio thread priority.
- `pick_color`: the eyedropper.
- `camera_effects`: the OS's own camera effects.
- `input`: pointer, clicks, keys, cursor shape (for the Screen Studio work).
- `plugins`: plugin folders and loaders.

The app and engine call these instead of OS APIs. A test per module runs on both platforms.

## 4. Phases

1. **It builds on Windows:**
   - guard every macOS call;
   - `lightair-platform` with paths, files, hotkeys, credentials, realtime, displays;
   - GPUI's foreground executor instead of GCD;
   - libobs from the submodule on Windows;
   - CI on `windows-latest` running `cargo xtask test` (tests that need macOS frameworks marked
     per platform).

   *Done when:* the Windows CI builds and runs the platform-neutral suite.

   Done: `lightair-platform` (paths, files, apps, realtime); hotkeys on `RegisterHotKey`;
   GPUI's foreground executor for plugin jobs; VST3 DLL loading and a Win32 editor window; a
   portable preview path (frames read back as BGRA, shown as images) until DXGI; the eyedropper
   hidden and camera effects stubbed until phase 3; Audio Unit tests marked macOS-only;
   `.github/workflows/ci.yml` (libobs from the submodule, clippy, `cargo xtask test`).
   Checking from a Mac: `PATH="/opt/homebrew/opt/llvm/bin:$PATH" cargo check --workspace
   --all-targets --target x86_64-pc-windows-msvc` (libobs-sys reads the headers as macOS's
   there and links nothing).
2. **It runs on Windows:**
   - DXGI preview into GPUI;
   - portable source kinds with migration;
   - capture (display, window, game), cameras, WASAPI mic, desktop and app audio;
   - NVENC/AMF/QSV;
   - audio engine on WASAPI.

   *Done when:* a project made on a Mac opens on Windows, plays, records and streams, and the
   screenshot hooks produce Windows screenshots.

   Done: portable kinds (`lightair_core::kinds`: screen, camera, mic, desktop audio, app
   audio) with their own setting names; the engine maps them per platform
   (`lightair_engine::kinds`) and gives an input a new source when an edit maps it to another
   libobs type; projects move to format 2 on open (and OBS imports arrive portable).
3. **Parity:**
   - Camera Studio on ONNX/DirectML;
   - VST3 hosting with editors;
   - eyedropper;
   - Windows Studio Effects;
   - running apps;
   - virtual camera;
   - installer, signing, updates.

   *Done when:* every feature in docs/02's catalog is marked for both platforms or has a stated
   one-platform reason.
4. **Ongoing:** every new feature (the Screen Studio plan first) is built with both backends, and
   CI gates on both.

## 5. Decisions

Recommended, to confirm as the work starts:

- **libobs on Windows:** built from the pinned submodule, the same way macOS release builds will
  be (IMPLEMENTATION.md, M13), so both platforms run the same libobs.
- **Portable project kinds:** the project stores Lightair's own kinds, not libobs ids. This is the
  one change that touches the model, so it comes early (phase 2).
- **One-platform features** say so in the UI rather than disappearing: Audio Units (macOS); later,
  iPhone/iPad over USB (macOS; Android over USB on both, see the Screen Studio plan).
- **Testing on Windows:** CI runs the suite; screenshots come from a Windows machine or VM through
  the same hooks.
