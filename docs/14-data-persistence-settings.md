# 14 · Data, Persistence & Settings

## What gets stored, where

| Data | Scope | Location (macOS) | Format |
|---|---|---|---|
| **Projects** | One show setup: formats, scenes, layers, inputs, tracks, outputs, transitions, hotkeys, workspace layout | `~/Library/Application Support/Lightair/Projects/<name>.lightair/` (a package directory) | JSON (versioned) + assets |
| **App settings** | Per-user preferences not tied to a show (appearance, language, default paths, update channel) | `~/Library/Application Support/Lightair/settings.json` | JSON |
| **Library** | Reusable user content: templates, looks, effect presets, stingers, LUTs, stickers | `~/Library/Application Support/Lightair/Library/` | Pack format |
| **Secrets** | Platform tokens, stream keys, remote API password | macOS Keychain / Windows Credential Manager / Secret Service | OS store |
| **Caches** | Thumbnails, compiled shaders, LUT bakes, model caches | `~/Library/Caches/Lightair/` | Disposable |
| **Recordings & clips** | Media output | User-chosen (default `~/Movies/Lightair/`) | Media files + sidecar JSON |
| **Logs & crash reports** | Diagnostics | `~/Library/Logs/Lightair/` | Text logs, minidumps |
| **Third-party OBS plugins** | Optional | `~/Library/Application Support/Lightair/Plugins/` | OBS plugin bundles |

Windows and Linux use the platform equivalents (`%APPDATA%`, `%LOCALAPPDATA%`, XDG directories).

## The project package

```text
My Show.lightair/
├─ project.json          manifest: format version, app version, ids, formats, settings
├─ scenes/               one file per scene (stable ids; diff-friendly)
│  ├─ 7f3c….json
│  └─ …
├─ inputs.json           media inputs (devices, captures, files, browser, plugin sources)
├─ audio.json            tracks, mixes, monitoring, effect chains
├─ outputs.json          destinations, encoders, recording, clips, virtual camera (no secrets)
├─ automation.json       hotkeys, scene automations, macros, controller mappings
├─ workspace.json        dock layout, workspaces, panel state per display configuration
├─ plugins/              data OBS plugins save through the frontend API, one file per plugin
├─ assets/               media copied into the project (optional "collect assets")
└─ journal/              autosave journal (see below)
```

- **Stable IDs.** Every entity has a UUIDv7. References use IDs, never names, so renaming never breaks anything.
- **One file per scene** keeps diffs readable and makes git-versioning a show setup practical.
- **Device references** are stored as a *role* plus a *device match*: `{"role": "main camera", "match": {"unique_id": "…", "name": "MacBook Pro Camera", "model": "…"}}`. When a project opens on another machine, unmatched roles prompt once ("Which camera is your main camera?") and the answer is remembered.
- **Paths** to media are stored relative to the project when inside it, absolute otherwise, with "Collect assets" to make a project portable.
- **Plugin sources** store their `obs_data` settings as opaque JSON with the plugin id and version.

### Schema and migrations

- `project.json` carries `formatVersion` (integer). The loader applies migrations stepwise (`v3 → v4 → v5`) in `lightair-store`, each a pure function with fixtures and tests.
- Opening a project from a newer Lightair version is refused with an explanation (no silent data loss); the user can open a read-only copy.
- Before migrating, the original is backed up into `journal/backups/`.

## Saving, autosave & recovery

- **There is no Save button for the show.** Every committed command is persisted.
- The core actor appends each committed command to an **append-only journal** (`journal/current.log`, length-prefixed, fsync batched every 250 ms and on outputs start/stop).
- Every 60 s and on quit, the journal is **compacted** into the JSON files (written atomically: temp file + rename).
- On launch after a crash, the loader reads the last snapshot and replays the journal tail. The user sees "Lightair recovered your project" with the option to restore outputs that were live.
- **Version history**: compacted snapshots are kept (hourly for a day, daily for a month) and browsable in *Project → History*, with "Restore as copy".

## Undo / redo

- Per project, unlimited during a session, persisted for the last 200 entries so undo survives a restart.
- Entries have human names ("Move 'Camera'", "Apply look 'Golden Hour'").
- Runtime and telemetry state is never in undo (starting a stream isn't undoable; changing its title is).
- Remote API and extension changes are undoable and labelled with their origin ("Undo: Stream Deck: Switch scene").

## Settings model

Settings are layered; the most specific wins:

1. **Built-in defaults** (in code, documented).
2. **App settings** (user-wide).
3. **Project settings** (per show: canvas formats, audio sample rate, outputs).
4. **Scene overrides** (transition, look, automations).

The Settings UI shows where a value comes from and offers "Use app default". Settings that require an engine reset (sample rate, base resolution, colour format) are applied with a short preview freeze and are blocked while live.

## Profiles

OBS's split between "scene collections" and "profiles" confuses users. In Lightair a **Project** holds both the show and its output configuration, because they belong together. For people who run the same show to different places, a project has **Output presets** ("Twitch + YouTube", "Recording only", "Podcast ISO") switchable from the Go Live sheet.

## Importing from OBS

`lightair-store` reads OBS scene collection JSON and profile `basic.ini` / `recordEncoder.json` / `streamEncoder.json`:

| OBS | Lightair |
|---|---|
| Scene collection | Project (Landscape format) |
| Scenes, scene items, groups | Scenes, layers, groups (transforms, crop, bounds, visibility, locks, blending) |
| Sources | Inputs (native mappings for core types; plugin sources kept as plugin inputs) |
| Filters (core) | Lightair effects with converted parameters |
| Filters (known plugins) | Built-in equivalents ([13](13-extensibility-and-compatibility.md#built-in-replacements-for-popular-plugins)) |
| Audio mixer settings, monitoring, tracks | Tracks and mixes |
| Transitions, overrides, stingers | Transitions |
| Hotkeys | Hotkeys (re-mapped to Lightair commands) |
| Profile: stream service, encoders, recording | Destinations and encoding presets (stream keys moved to the keychain) |
| Aitum Vertical scenes | Vertical format layouts |
| Custom browser docks | Custom browser docks in the workspace |
| Plugin data saved in the collection | `plugins/` data for enabled frontend plugins |

The import produces a **report**: converted ✓, replaced by built-in ↻, kept as plugin ⧉, needs attention ⚠ (with a jump-to link for each).

## Exporting

- **Project package** (zip of the `.lightair` directory) for sharing or backup, with optional asset collection and secrets always excluded.
- **Template export** of selected scenes, with devices converted to roles.
- **Diagnostics bundle**: logs, system info, project (sanitised: secrets and personal paths stripped), engine stats. Generated locally; the user decides where it goes.

## Security of stored data

- Secrets never touch project files, logs, diagnostics bundles or crash reports.
- Logs redact stream keys, tokens and URLs with credentials by pattern before writing.
- Remote API passwords are stored in the OS credential store; the API is off by default and listens on localhost only unless the user enables LAN access.
