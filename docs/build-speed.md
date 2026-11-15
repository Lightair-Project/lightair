# Build and test speed

The edit → check → test loop should take seconds. This page measures where the time went,
ranks the causes, and records what changed, with numbers before and after.

**In short:** `target/debug/deps` held 1.2 million leftover object files, and rustc reads that
folder every time it starts. Clearing them took a `cargo check` from more than 11 minutes
(stopped unfinished) to about 2 seconds. That cause now has a guard against coming back. After
it, the other changes made the loop about twice as fast again:

| Step (after an edit to `lightair-core`) | Before | After |
|---|---:|---:|
| `cargo check --workspace --all-targets`, leftovers in place | > 678 s (stopped) | — |
| `cargo check --workspace --all-targets` | 2.3 s | 1.8 s |
| … after a public-API change | 4.4 s | 2.7 s |
| Build every test program | 8.2 s | 3.4 s |
| … after a public-API change | 9.1 s | 3.2 s |
| `cargo xtask test`, nothing to rebuild | 27.2 s | 13.3 s |
| `cargo xtask test`, first run after a rebuild | 39.5 s | 17.1 s |
| Cold `cargo build --workspace` | 218 s | 144 s |
| Cold test programs, after that build | 107 s | 52 s |

How to work now:

```sh
cargo xtask check       # the edit loop: cargo check, every target, macOS (≈ 1–3 s)
cargo xtask test        # every test (≈ 13 s to run)
cargo xtask precommit   # what a commit needs: clippy for macOS and Windows, 0 warnings; the tests
cargo xtask prune       # clear object files earlier builds left (the test and check commands do it too)
```

## How it was measured

These were measured on an Apple M4 Pro (14 cores, 24 GB) with Rust 1.99.1, on 2026-11-15.
Builds ran at `nice -n 10`. The machine wasn't idle: Lightair itself was running (about 70 % of
a core, camera on), along with Claude and Spotlight. The numbers are what a working day sees;
expect about ±0.5 s on the small ones.

- **Incremental.** The root file of a crate was touched (`crates/lightair-core/src/lib.rs`,
  `crates/lightair-render/src/lib.rs`, `crates/lightair-app/src/main.rs`), then the command
  was timed. A touch costs the same as changing a function body. Changing a public API means
  every crate that uses it has to be type-checked again. To measure that, each step added a new
  `pub fn` (a dead-code `fn` in the app), and the file was restored afterwards. The scripts
  ran `cargo build --timings` for the per-unit times.
- **Cold.** A fresh `CARGO_TARGET_DIR` ran `cargo build --workspace`, then
  `cargo test --workspace --no-run`, then `cargo check --workspace --all-targets`.
- **Link.** rustc printed the linker command (`-C save-temps --print link-args`), which was
  replayed with Apple's `ld` and with LLVM's `ld64.lld`. Each figure is the best of 3.
- **Tests.** `cargo xtask test` was run twice: once right after a build (the first launch of
  each new program), then again. The per-program times are in `target/test-times.json`.

## Where the time went

### 1. A million leftover object files (the biggest cause, by far)

`target/` was 130 GB. `target/debug/deps` held **1,211,311 files**, and 1,201,251 of them were
`*.rcgu.o` object files, 80 GB in all. The cause:

- On macOS, debug info stays in the object files. Rust's default there is
  `split-debuginfo = "unpacked"`, and the linked program only points at the objects. So rustc
  keeps the objects of every test program and binary next to it.
- Each **incremental** build writes them under **new names**
  (`smoke-<hash>.<unit>.<build>.rcgu.o`), and nothing deletes the old ones. A scratch crate
  showed this. Built three times incrementally, its test program left 75 objects. Built without
  incremental compilation, the objects have fixed names and are overwritten. One `smoke` program
  had 25,479 objects from earlier builds.
- **rustc reads the whole `-L dependency=target/debug/deps` folder every time it starts**,
  to find crates. With an empty search folder, compiling a one-line crate takes 0.05 s. With
  `target/debug/deps` as the search folder, the same compile took 17.1 s.
- A `cargo check --workspace --all-targets` (about 80 rustc runs) was stopped after 11 minutes,
  unfinished. Its rustc processes sat in disk wait while the SSD served about 5,500 small
  metadata reads per second. Listing the folder alone took 28–35 s.

**Fix.** A one-time cleanup deleted the 1,092,876 objects that no program in the folder used,
freeing 52 GB in 2.5 minutes. It then deleted 661 binaries from builds that no longer exist,
with their objects. That left 16,747 files, 22 GB in all, and rustc started in 0.1 s again.
To keep it that way:

- `cargo xtask prune` deletes every object that no program in `deps` refers to. It reads each
  program's object list with `nm -ap` (OSO entries), at about 0.06 s per program.
- `cargo xtask test` deletes the leftovers of the programs it just built. This runs alongside
  the tests, so it costs nothing.
- `cargo xtask check` prunes first when the folder holds more than 50,000 files. Plain `cargo
  build` and `cargo test` still leave objects: about an hour of measuring took the folder from
  16.7k to 77k files.

**Considered and not done.**
- `split-debuginfo = "packed"` would run `dsymutil` on every link, adding seconds to each.
- `incremental = false` for test programs can't be done alone. `[profile.test]` applies to every
  unit in `cargo test`, libraries included. Any difference from `dev` would also build every
  library twice.

**Disk.** `target/debug/incremental` holds 48 GB from older builds. That's disk space only:
rustc doesn't search there. A single `cargo clean` frees it, at the cost of a full rebuild
(about 3.5 minutes).

### 2. Sixty-seven test programs

Every file in `crates/*/tests/` was its own test program. The unit tests of each crate add
more, for 67 in all. After a change to `lightair-core`, the test build ran 65 units for
**80.5 CPU-seconds**, and each test program took 1.5–2.2 s.

Linking isn't where the time goes. Replaying the linker shows:

| Program | Apple `ld` | `ld64.lld` |
|---|---:|---:|
| `lightair-engine/smoke` (5 MB) | 0.16 s | 0.23 s |
| `lightair-core/document` (5 MB) | 0.07 s | 0.09 s |
| `lightair` app (90 MB) | 0.34 s | 0.68 s |

The cost is rustc starting each program over: it loads the metadata of about 200 crates, then
generates the same generic code (serde_json, tracing-subscriber, …) in every program. Each new
program also takes about 0.5 s more the first time it runs, while macOS checks a newly linked
executable. That added 33 s of process time to the first test run after every rebuild.

A faster linker isn't the answer. Apple's current `ld` beats `lld` on every program here.

**Fix.** Each crate's integration tests are now one program: `tests/<crate>/main.rs`, with a
`mod` per former file. Test programs went from 67 to 17. Engine and preview tests start
libobs, which can start only once per process (more under "The test harness" below).

### 3. Full debug info, for our crates and for every dependency

`[profile.dev]` set no `debug`, so it used the default: full debug info. The dependencies (GPUI,
wgpu, naga, image, …) were built at `opt-level = 2` and also inherited full debug info.
Optimized code with full debug info is the slowest combination to generate, and nobody reads
debug info for dependencies.

**Fix.**

```toml
[profile.dev]
debug = "line-tables-only"     # backtraces and stepping by line, no variable descriptions

[profile.dev.package."*"]
opt-level = 2
debug = false                  # backtraces still name their functions
```

For a debugger session that needs variables: `CARGO_PROFILE_DEV_DEBUG=full cargo build`.

| Measured alone (cold, fresh target dir) | Before | After |
|---|---:|---:|
| `cargo build --workspace` | 218.0 s | 144.1 s |
| `cargo test --workspace --no-run`, after that | 106.6 s | 52.2 s |
| `cargo check --workspace --all-targets`, after that | 90.2 s | 31.7 s* |

\* `check` doesn't generate debug info, so most of that difference is load on the machine.

### 4. Tests waiting in real time

The test run is bounded by its slowest programs, and those were fixed sleeps:

| Program (before → after, slowest test) | Before | After | What changed |
|---|---:|---:|---|
| `engine/stream_protocols` | 9.3 s | 5.7 s | Polls for SRT data instead of sleeping 1.5 s. ffmpeg's UDP probe drops from 5 s to 1.5 s (`-analyzeduration`), and it captures 0.8 s, not 1.5 s |
| `preview/transform3d` | 8.8 s | 1.4 s | Took 3 frames per sampled pixel (≈ 80 pixels). Now waits once per check, then samples that frame |
| `preview/transitions` | 8.7 s | 5.4 s | Samples every frame and looks for the midway picture in any of them, instead of one timed sample. 800 ms transitions, not 1.6 s, and no extra full-length sleep. No longer runs alone |
| `engine/recording_tools` | 8.7 s | 5.7 s | 3.6 s of recording, not 6.5 s. A keyframe every second, so the split lands soon. The paused-time check is measured against the clock |
| `engine/outputs` | 6.9 s | 6.3 s | Recording: waits for the first write (at least 1 s) instead of 1.2 s plus a wait. Clips and stream: 1.1 s, not 1.5 s |
| `engine/stream_sharing` | 6.3 s | 5.4 s | 1.2 s + 1.2 s, not 2 s + 2 s. Checks are relative ("the second got 0.6 s more than the first") |
| `engine/audio_engine` | 5.0 s | 3.4 s | Peaks over 600 ms, not 1,500 or 1,000 ms (a 440 Hz cycle is 2.3 ms) |
| `engine/encoding_options` | 4.5 s | 3.1 s | 2.2 s of HEVC with ≥ 2 keyframes, not 3.2 s with ≥ 3 |
| `engine/camera_studio` | 4.3 s | 2.7 s | 1 s warm-up and 1 s measured, frame minimums halved to match |
| `preview/stinger` | 3.6 s | 2.2 s | Stops watching once it has seen green, then blue |
| `engine/multi_recording` | 3.8 s | 3.1 s | 0.7 s before pausing, not 1.2 s |
| `engine/mix_tracks` | 2.5 s | 1.8 s | 0.8 s of MKV to count audio streams, not 1.5 s |

Timers (≈ 5–6 s) stay as they are. A 2 s countdown, a 1.2 s pause check and an added second
are what that test is about.

`ALONE` in the xtask made `preview/transitions` run alone, after everything else, adding
8.7 s to the end of each run. It now holds up with other tests running, so `ALONE` is gone.

`lightair-render/src/thumbs.rs` `every_look_gets_a_thumbnail` renders all 309 thumbnails.
The whole `lightair-render` unit-test program takes 1.2 s, so the test stays as it is. It's the
only check that every filter, effect, transition and animation renders at all.

### 5. Windows clippy at every step

`cargo clippy --target x86_64-pc-windows-msvc` checks the whole graph a second time, for
another target. Warm, after an edit to core, that took 5.1 s before and 3.1 s after. After a
profile or dependency change it takes 83 s. It runs in `cargo xtask precommit`, not in the
edit loop.

`gpui-pre`'s build script compiles a Windows resource file with `llvm-rc`, which Homebrew's LLVM
keeps off PATH. `precommit` adds `/opt/homebrew/opt/llvm/bin` to PATH when `llvm-rc` isn't
found there.

### 6. Checking only the changed crates doesn't pay

Crates that didn't change cost almost nothing to check. With only the app edited, `cargo check
--all-targets -p lightair-app` and `--workspace` both took 0.83 s. A subset has a price,
though. Cargo unifies dependencies' features over the selected packages only, so the first
`-p lightair-app` run took 15 s, recompiling dependencies with that subset's features. Making
it workspace-wide (`resolver.feature-unification`) still needs `-Z`.

So `cargo xtask check` checks the whole workspace. `cargo xtask check --changed` limits the
check to the crates changed since the last commit and the crates that use them. The same
applies to `cargo test -p …`, which can rebuild dependencies the first time. `cargo xtask test`
uses `--workspace`.

### 7. Cold builds: dependencies we don't choose

Our own crates are 70 of the 2,011 CPU-seconds in a cold build. The largest units were
`gpui-pre` (91 s), `image` (71 s), `naga` (63 s), `gpui-component` (62 s), `gpui-base` (56 s),
`wgpu-core` (55 s), `objc2-app-kit` (44 s), `bindgen` (41 s), `nnnoiseless` (36 s) and
`cbindgen` (34 s). Some of that is features or duplicates nobody uses:

- `gpui-pre` enables 15 `image` formats, among them EXR, TIFF, DDS and HDR, plus `moxcms` for
  colour management. Lightair asks for PNG only. Fixing it needs a GPUI patch, like the one
  `third_party/gpui-component` carries.
- `gpui-pre-apple` runs `cbindgen` in its build script (34 s, cold only).
- There are two font stacks. `cosmic-text` 0.19 (`lightair-render`) uses
  `read-fonts` 0.37 / `skrifa` 0.40, and GPUI uses 0.41 / 0.44. That's about 55 s of duplicate
  compile until their versions meet. `syn` 2 and 3 are both built for the same reason.

These cost nothing in an incremental loop, so they're listed here, not fixed.

### 8. `be_polite()` and `nice`

The xtask lowers its own priority, and that of everything it starts, to nice 10, as the house
rule asks. Nice only matters when something else wants the CPU. Then the user's apps win, which
is the point. With Lightair running alongside, the suite still finished in 13 s, so this rule
stays.

## Causes, ranked by time saved

| # | Cause | Cost | Fix | Saved |
|---|---|---|---|---|
| 1 | 1.2 M leftover objects in `target/debug/deps` | ~17 s per rustc start; `cargo check` > 11 min | One-time cleanup; `xtask prune`; pruning in `test` and `check` | Minutes per check, every check |
| 2 | 67 test programs | 1.5–2.2 s of rustc each (80 CPU-s per core edit); +0.5 s first launch each | 17 programs; one process per libobs test, from one binary | Test build after an edit 8.2 → 3.4 s (API change 9.1 → 3.2 s); first run 39.5 → 17.1 s |
| 3 | Real-time sleeps; transitions run alone | Suite 26–31 s, 130 s of process time | Condition waits, shorter recordings, frame-by-frame sampling | Run 26.1 → 12.9 s; process time 130 → 89 s |
| 4 | Full debug info for ours and every optimised dependency | Cold +74 s build, +54 s tests; larger objects | `line-tables-only` for ours, none for dependencies | Cold build 218 → 144 s; test programs 107 → 52 s; incremental build after an edit 3.8 → 2.9 s |
| 5 | Windows clippy in the loop | 3–9 s warm, 83 s after a profile or dependency change | Only in `xtask precommit` | All of it, from the edit loop |
| 6 | Dependency features and duplicates (GPUI's `image`, `cbindgen`, two font stacks) | ~130 s of a cold build | Not ours to change without patches | — (cold builds only) |
| 7 | Linker | 0.07–0.34 s per program | None: `lld` is slower | — |

## The test harness

- **One program per crate.** `lightair-core`, `-store`, `-render`, `-audio`, `-engine` and
  `-preview` each have `tests/<crate>/main.rs` with a module per former file. The files moved
  with `git mv`. The proptest regressions file moved with `document.rs`, and the store
  fixtures stay in `tests/fixtures/`.
- **libobs once per process.** Each test that starts an engine (23 engine, 16 preview) begins
  with:

  ```rust
  if lightair_engine::testing::ran_in_own_process() {
      return;
  }
  ```

  Under plain `cargo test`, that re-runs the test program for just that test, at most half the
  cores at a time. It shows the child's output, and fails unless the child reports one test
  passed. With `LIGHTAIR_TEST_OWN_PROCESS` set, the test runs where it is.
  `cargo xtask test` lists such programs' tests (`--list`) and runs each as a process of its
  own, slowest first, alongside the other programs. A process that matched no test counts as a
  failure, so a renamed test can't pass quietly.
- `engine/vst3` stays its own program, because it has its own `main` and runs on the main
  thread.
- `preview/face_tracking`'s four tests used to share one process. Run with
  `LIGHTAIR_FACE_IMAGE`, that would have started libobs four times in it. Each now gets a
  process of its own.

## Coverage: what moved, what became opt-in

Nothing was deleted.

**Moved.** All 59 integration test files, into the six programs above.

**Opt-in.** These are now `#[ignore = "…"]`. `cargo xtask test --machine` runs them, and leaves
out the three measurement tests below:

| Test | Why |
|---|---|
| `lightair-engine` `device_channels::each_track_takes_its_channels` | An interface source opens this Mac's audio devices |
| `lightair-engine` `monitor::the_monitor_hears_the_cued_tracks` | Real-time audio pacing on this Mac |
| `lightair-engine` `audio_block::a_smaller_buffer_restarts_the_engine_and_keeps_playing` | Real-time audio pacing ("keeps pace" bound) |
| `lightair-app` `system_effects::this_macos_reports_its_effects` | Only checks that macOS 15's methods exist; fails on 12–14 |
| `lightair-app` `shortcuts::macos_takes_a_registration` | Registers a real global shortcut (⌃⌥⌘F12) on this Mac |

**Shortened, same checks.** The table in section 4. Two tests have rescaled thresholds:
`recording_tools`, whose paused time is checked against the measured wall clock, and
`stream_sharing`, whose checks are now relative.

Test counts: 234 passed before. After, 229 passed and 5 are opt-in.

## Garbage tests: tests that test little

From a read of every test. None were deleted. These are worth a decision:

| Test | What's wrong |
|---|---|
| `render/speed::swizzle_cost` | `#[ignore]`d, and asserts nothing: it times a local byte loop and touches no crate code. Delete or move to a benchmark |
| `render/speed::frames_per_second`, `audio/cost::podcast_graph_cost` | `#[ignore]`d measurements that only print. They're useful by hand, but they're not tests |
| `app/system_effects::this_macos_reports_its_effects` | Checks that macOS has two methods, no Lightair logic (now opt-in) |
| `engine/vst3` `a_real_plugin` part | Only runs if Surge XT is installed, and prints "ok" either way |
| `preview/face_tracking` (4 tests) | Never run unless `LIGHTAIR_FACE_IMAGE` is set, which isn't checked in. They cost 4 short processes per run |
| `preview/{crop_view,fx,shape_mask,transform3d}`, `engine/camera_studio` | Pass empty when `cargo xtask modules` hasn't been run, and always on Windows. Consider failing, not skipping, when the module is missing on macOS |
| `engine/portable_kinds` | `cfg(windows)`. As its own program it cost a link on macOS for nothing; merged, it costs nothing |

**Duplicates.**
- `core/document`: `a_drag_gesture_is_one_undo_step` and `dragging_several_objects_is_one_undo_step`
  follow the same gesture path with the same checks.
- `core/document`: `undo_and_redo_restore_snapshots` is mostly covered by the proptest
  `undo_redo_round_trip`.
- The `refit` arithmetic is checked in both `engine/import_obs` and
  `store/import_obs::estimated_objects_refit_to_the_real_size`.
- `engine/stream_protocols` checks again that `ftp://` is refused, which
  `urls_pick_the_protocol` already covers.

**Fragile.**
- `render/sequence::scrubbing_back_reads_frames_already_decoded` asserts `< 400 ms`.
- `engine/plugins`'s two tests shared the process-global `plugins::changed()` flag. As separate
  processes they no longer can.

## Not done

- Patching GPUI's `image` features, or aligning `cosmic-text` with GPUI's font crates. Both
  are cold-build only (section 7).
- Deleting the garbage tests above, which is a call for whoever owns them.
- A shared-engine harness that would run many engine tests in one libobs. libobs can't restart
  in a process, and the tests change process-wide state (outputs, program scene). Running them
  one after another would also be slower than running processes side by side.
