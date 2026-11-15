# Lightair vs OBS: streamer benchmark

November 2026. Lightair `1ab6b6a` (release build) against OBS Studio 33.0.0, vanilla. Harness:
[tools/bench](../../tools/bench/README.md).

## Result

| | OBS 33 (vanilla) | Lightair before | Lightair after |
|---|---|---|---|
| Startup to ready (median of 5) | 1.13 s | 0.96 s | **0.99 s** |
| Memory 3 s after launch (all processes) | 1204 MB | 1207 MB | **1128 MB** |
| CPU, idle on Gameplay (app + helpers) | 54.2 % | 103.8 % | **56.2 %** |
| CPU, recording while switching scenes | 55.8 % | 112.7 % | **54.3 %** |
| GPU busy, idle | 31.7 % | 71.4 % | **29.8 %** |
| GPU busy, recording | 32.3 % | 68.3 % | **30.7 %** |
| Render time per frame (libobs), idle | 1.71 ms | 4.83 ms | **0.91 ms** |
| Render time per frame, recording | 3.79 ms | 5.35 ms | **2.77 ms** |
| Memory while recording | 1381 MB | 1352 MB | **1268 MB** |
| Frames skipped (render / output) | 0 / 0 | 0 / 0 | 0 / 0 |
| WindowServer CPU (macOS compositor) | 45 % | 47 % | 55 % |

CPU is percent of one core, summed over the app and its helper processes (Chromium), from
the processes' own CPU time, sampled every second for 60 s. GPU is the GPU's "Device
Utilization" (ioreg). Both recordings came out 1920×1080 at 60 fps with every frame
(3616 frames in 60.27 s for OBS, 3617 in 60.28 s for Lightair).

Lightair now matches OBS on CPU and GPU, starts faster and uses less memory. The one thing it
still costs more is the macOS compositor (WindowServer, +10 points): GPUI redraws the whole
window for each preview frame where OBS's Qt window updates only its preview. See
"What's left".

## Machine and scenario

MacBook Pro, Apple M4 Pro (14 cores), 24 GB, built-in 3024×1964 display at 120 Hz, macOS 27.
Each app ran alone, frontmost, display awake.

Six scenes a streamer would have, built in both apps by the same script through the same
obs-websocket requests (OBS's own server; Lightair's compatible one):

- **Starting Soon**: background image, dark tint, logo, title text, alert overlay (browser),
  music.
- **Gameplay** (on air while idle): 1080p60 gameplay video with sound, 720p30 webcam with a
  frame, logo, title, chat (browser, 400×800), alert overlay (browser, 1920×1080, animated),
  mic, music at −20 dB.
- **Desktop**: display capture of the main display with the same webcam, overlays and audio.
- **Just Chatting**, **BRB**, **Ending**.

Canvas 1920×1080 at 60 fps. Recording: Apple VideoToolbox H.264 (hardware), 12 Mbps average,
2 s keyframes, B-frames, AAC 192 kbps, MP4, the same in both. Browser sources at 30 fps.
The webcam, gameplay, mic and music are generated media files (ffmpeg) so both apps see
identical input; the display capture is real.

**Vanilla OBS**: OBS on macOS has no portable mode, so its settings folder was swapped for a
clean one for the OBS runs (the user's own, with five third-party plugins, renamed and put
back afterwards, byte-for-byte). OBS's log confirms only its bundled modules loaded. Clean
profile: 1080p60, Advanced output with the recording settings above, OpenGL renderer, no
Desktop Audio or Mic/Aux devices (neither app captured a real microphone).

Measured: startup (launch to window, and to the scene collection loaded, answering over the
API) ×5 each; then after 20 s settling, 60 s idle with Gameplay on air; then 60 s recording
while switching scenes every 10 s (Just Chatting, Desktop, BRB, Gameplay, Starting Soon).

## What was wrong, and what changed

In order of effect, measured along the way (idle CPU / GPU):

1. **Off-air sources ran** (104 % → 56 % with the rest; −16 points CPU, −29 GPU on its own).
   A mixer track kept its source active "for its sound", so every video decoded, every web
   page painted and the display capture captured whether its scene was on air or not. Now, as
   in OBS, a source with a picture runs only while a scene showing it is on air; sound-only
   sources (microphones, desktop and app audio) stay on like OBS's global audio sources, and
   off-air web pages are hidden (Chromium stops painting them), as OBS's browser source does.
   This uncovered a Move-transition bug the always-on tracks had hidden (libobs counted a
   video through a fading item one way and back another, and the video stopped on landing):
   fixed, with a test over Cut, Fade and Move.
2. **The window redrew everything at 120 Hz** (104 % → 77 %). The root view, the canvas, the
   mixer and the outputs panel each asked for every display refresh, and in GPUI asking marks
   the view and all its parents dirty, so the whole window re-laid out and repainted 120
   times a second. Now the canvas redraws its own panel only when the preview has a new
   frame, meters move at 30 fps in the same redraws as the canvas, clocks twice a second.
3. **The preview stalled the render thread** (render time 3.2 → 0.8 ms). Each frame it blitted
   the program into an IOSurface, waited for the GPU (`glFinish`), then converted it with a
   VideoToolbox transfer, synchronously. The conversion to NV12 is now a libobs shader, the
   copy is a GPU blit, and a GPU fence hands the frame over when it's done, without waiting.
   (It also fixed a colour slip: the old path encoded BT.709 where the UI decodes BT.601.)
4. **Browser frames went through the CPU** (−2 points). Chromium painted into memory, Lightair
   copied each frame, libobs copied it again and uploaded it. Now on macOS Chromium shares
   its frames as IOSurfaces, drawn by libobs as they are, as OBS's browser source does.
5. **Startup crash, now and then** (stability). Chromium's library loaded lazily, after
   libobs's threads were running; on macOS its allocator takes over the process's memory zone
   as it loads, and a free racing that takeover crashed the app (three crash reports that
   day, the earliest before this work). It now loads first thing in `main`: 20 launches of the
   benchmark workspace in a row, no crash.

Also new: Settings › Canvases › Live preview: **Light** draws the preview at 30 fps (default
Smooth: every canvas frame). Recordings and streams are unaffected.

## What's left

- **WindowServer** works harder for Lightair (55 % vs 45 %), and every preview frame still
  costs a full GPUI redraw (about 15 points of CPU and 20 of GPU at 60 fps, measured by
  turning it off). The fix is a compositor: draw the preview in its own Core Animation layer
  behind GPUI's, so video frames never go through GPUI. GPUI's view already sits in a plain
  container view where such a layer can go below it, but every view above the canvas (the
  window root, the dock, the tab panels) paints an opaque background that would have to make
  way. A change to plan, not to slip in.
- **Meters** cost a mixer re-layout 30 times a second. Fine now that they share the canvas's
  redraws; cheaper still if meters were drawn by one canvas element per strip.
- **Chromium's message pump** runs every 8 ms while pages are open; CEF can schedule it
  instead (`on_schedule_message_pump_work`).
- Not measured: a real camera and microphone (generated files stood in for them), streaming
  (no destination), long sessions, other Macs.

## Reproduce

```sh
cargo xtask bundle --release
python3 tools/bench/bench.py prepare
```

then the steps in [tools/bench/README.md](../../tools/bench/README.md).
