# The Editor as a full video editor

Status: phases 1 to 5 built (14 November 2026); see the notes under them. This replaces the Screen Studio plan's
"focused editor" decision (docs/plans/screen-studio.md, section 7): the Editor becomes a
multi-track video editor, in the way CapCut is, while a Studio take keeps everything that made it
special (zooms, the redrawn pointer, the backdrop) as a kind of clip.

## 1. What it edits

A **sequence**: the result's size and frame rate, and tracks.

- **Media:** the files the sequence uses: the project's Library items (recordings, clips, takes,
  exports) and files imported from anywhere.
- **Video tracks**, drawn bottom track first (an upper track is in front). **Audio tracks** below.
- **Clips** on a track: a piece of a medium (its in and out points), where it starts on the
  timeline, its speed, volume, and, for pictures, where it is (position, scale, rotation,
  opacity). A video clip plays its own sound unless it's muted.
- **Take clips:** a Studio take on a video track renders through the screen recording look (its
  zooms, pointer, backdrop: the existing `Edit`), then is placed like any other clip.

**Workspaces and projects.** What Lightair called a project is a **workspace** (its scenes,
mixer, outputs and Library). The Editor works on **projects** inside the workspace, as many as
wanted, each a file of its own (`edits/<id>.json`: its name and sequence). Every project's media
panel holds what the workspace has (its Library items and files imported into it), so editing
starts in the Editor, not in the Library; the Library's "Open in Editor" adds the item to the open
project. The Editor opens on the last project, and lists the workspace's projects (new, open,
rename, delete) from its toolbar and when none is open.

A project is a document: every change is a step to undo, and it's saved as it changes.
(In code and on disk the workspace is still `Project` and a `.lightair` package.)

## 2. The window (CapCut's arrangement)

```text
┌ Media ─────────┬ Player ───────────────────────┬ Inspector ──────────────┐
│ Library items  │                               │ Video · Audio · Speed   │
│ Import…        │   the frame at the playhead   │ position, scale, rotate │
│ (drag, or +)   │   ▶  00:00:46 / 00:20:19      │ opacity, volume, speed  │
├────────────────┴───────────────────────────────┴─────────────────────────┤
│ select · split · delete · undo · redo                  zoom ─────●──     │
│ ruler 00:00   00:30   01:00   01:30 …                                     │
│ V2  [ clip ][ clip ]                                                      │
│ V1  [ clip ][ clip ][ clip ]                                              │
│ A1  [ audio ……………………………………………………………… ]                                  │
└───────────────────────────────────────────────────────────────────────────┘
```

## 3. Phases

1. **Sequence and renderer:** the model (media, tracks, clips, transforms, speed, volume);
   the frame at any time (each track's clip decoded and composited with its transform); playback;
   export with every audio source cut, sped, delayed and mixed; Studio takes as clips.
   *Tests:* clips land where they're placed and in order of tracks; trimmed and sped clips map
   their source time; the export's length and sound follow the sequence.
2. **The window:** media, player, inspector, the multi-track timeline (move between tracks and
   in time, trim by either edge, split at the playhead, delete, snapping to edges and the
   playhead, zoom), undo and redo, opening any Library item or a new empty sequence.

   Built: projects in the workspace (new, rename, delete, the last one reopened); media from the
   workspace's Library and imported files (dragged onto a track or added with +); the player and
   playback; the inspector (transform, volume and mute, speed, a take's screen look, the
   project's size and frame rate); the timeline (ruler, tracks with show, mute and lock, clips
   moved in time and between tracks, trimmed by their edges, snapping, split, delete, zoom and
   scroll), undo and redo; export as MP4, ProRes or GIF into the Library.
   The earlier single-take Editor's transcript, captions and hidden areas came back in phase 3
   and the round after phase 5 (below).
3. **Words and looks:** text clips with styles, transitions between clips, filters and effects
   (the effect catalog's video effects), keyframes for transforms, stickers; captions as a track.

   Built: text and shape clips (drawn on the CPU with cosmic-text and tiny-skia, kept until they
   change), from the media panel's Text and Shapes tabs (style presets, clicked in at the
   playhead or dragged onto a track), edited in the inspector (words, size, line width, colour,
   weight, alignment, a box behind, an outline; a shape's kind, size, corners, fill and line).
   Filters per clip in the compositor's shader (presets, and brightness, contrast, saturation,
   warmth, vignette, blur). Transitions across a cut (crossfade, dip to black, slide, zoom; 0.3
   to 2 s; one cut or every cut), shown on the timeline. Keyframes for the transform (added or
   removed at the playhead, eased between, kept in place by splits and trims), marked on clips.
   Captions: the project's sound mixed and transcribed on the computer (whisper), the lines made
   into text clips on a captions track (made again in place, restyled together).
   Since: the effect catalog's shader effects on clips (the 22 `fx.*` effects ported to WGSL,
   fx.wgsl, their settings read from the .effect files; an Effects tab with the catalog's
   controls); emoji stickers (colour glyphs drawn in their colours); pictures moved and scaled
   by dragging them in the player, snapping to the centre lines; fades in and out as one
   control for picture and sound; hidden areas drawn over a clip in the player (pixelated).
4. **Sound:** waveforms on clips, fades, volume keyframes, detaching a video's sound, music,
   voice clean-up (the audio engine's effects, offline).

   Built: the player plays the sound (the mix streamed by FFmpeg from the playhead to the
   default output; the picture keeps time with what's been heard). Waveforms on audio clips, in
   decibels and shaped by the clip's volume and fades. Fades in and out, volume keys (added or
   removed at the playhead, straight between, kept in place by splits and trims), all in the
   export's mix too. A video's sound detached onto an audio track. Music and sounds from the
   media panel's Audio tab. Voice clean-up per clip: the live mixer's Voice Clarity chain (light,
   medium, strong; natural or podcast), run once on the file on a thread and mixed in its place.
   Since: music turned down under voices (FFmpeg's sidechain compressor, the voices as its
   key); voice-overs recorded from the microphone while the picture plays; sound while
   scrubbing; fades and volume dragged on the clips; the player's sound on the Live monitor's
   device. A Words tab: a clip transcribed, words marked and cut, fillers and long pauses taken
   out, the track closing up.
5. **Speed of work:** filmstrip thumbnails, proxies, smooth real-time playback (frames decoded
   ahead), the GPU's video decoder.

   Built: the player's frames are made at the player's size (the smallest of 640×360 to
   2560×1440 that covers it) and come back from the GPU as BGRA, ready to draw (swapping bytes on
   the window's thread cost 74 ms a 1080p frame in a development build); frames are made into
   images off the window's thread, and the one before last leaves the GPU's atlas (they were
   never freed). Videos are decoded ahead on a thread of their own (the GPU's decoder through
   FFmpeg's `-hwaccel auto`, as before); playback makes frames ahead on one thread and shows
   them in time on another, skipping late ones, the clock and the sound starting with the first
   frame. One graphics device is shared by every renderer. Proxies: videos larger than 1080p
   are copied at 1080p (a keyframe every half second) one at a time on a thread, read by the
   player and never by the export; on by default, per project. Filmstrips on video clips, read
   from keyframes (every frame when there are too few).
   Measured: 1080p frames 186 → 276 a second in the renderer; 2880×1800 at 60 fps, 139 (232 at
   the player's size); the player showed 8 frames a second before and 26–30 now (a development
   build, its window in the background).
   Since: takes through proxies; scrubbing back finds frames already decoded (each stream keeps
   its last frames, and a seek back starts a second early); on macOS the player draws straight
   from the GPU (an IOSurface the compositor renders into, made NV12 by VideoToolbox for GPUI's
   surface element: no read back), 30 frames a second.
