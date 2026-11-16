# Lightair vs OBS benchmark

The same streamer scenario built and measured in Lightair and in a vanilla OBS Studio, both
driven through the same obs-websocket v5 requests (OBS's own server; Lightair's compatible
one). Results and findings: [docs/benchmarks/lightair-vs-obs.md](../../docs/benchmarks/lightair-vs-obs.md).

Needs: macOS, OBS 33 in /Applications, ffmpeg, a release bundle (`cargo xtask bundle --release`).

```sh
python3 tools/bench/bench.py prepare          # OBS copy, clean OBS settings, media → target/bench
./target/bench/obs-swap.sh in                 # clean OBS settings in place of yours (renamed, kept)
python3 tools/bench/bench.py setup obs        # builds the six scenes in OBS (once)
python3 tools/bench/bench.py startup obs 5
python3 tools/bench/bench.py run obs base
./target/bench/obs-swap.sh out                # your OBS settings back
python3 tools/bench/bench.py setup lightair   # the same scenes in a scratch Lightair workspace
python3 tools/bench/bench.py startup lightair 5
python3 tools/bench/bench.py run lightair base
```

macOS OBS has no portable mode (it ignores `--portable`), so a vanilla OBS means swapping its
settings folder: `obs-swap.sh in` renames yours to `obs-studio.lightair-bench-backup` and puts
the clean one in its place; `out` puts yours back. Lightair runs on port 4466 with its own
workspace (`target/bench/la-ws`), never yours.

- `quick APP TAG`: 30 s idle with Gameplay on air (for iterating on a change); `BENCH_SCENE`
  picks another scene. It reports CPU energy (`power_mw`) and instructions (`instr_g`): compare
  those, not CPU %, which counts time on any core at any clock.
- `profile APP TAG`: macOS `sample` of the main process; `prof.py` and the call graph show
  where the time goes per thread.
- Results land in `target/bench` as JSON (`startup-*.json`, `run-*.json`).
