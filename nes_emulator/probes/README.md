# ROM probe and visual regressions

Always profile an optimized build. The basic command is:

```sh
cargo run --release -- probe <rom> "<button@from-to,...>" <frames>
```

The final `PROBE_SUMMARY` records emulated FPS, average/p95/max host frame
time, generated samples and long-run sample drift. Audio queue fields are
`4294967295` (`BACKLOG_UNAVAILABLE`) in the default headless mode.

Set `PROBE_REALTIME=1` to pace at the NTSC frame rate and feed the real SDL
audio pump. That mode additionally records queue minimum/maximum/end depth,
dropped samples, estimated underflow samples, and device reopens. It needs a
working SDL audio device; CI can use `SDL_AUDIODRIVER=dummy` to exercise the
pipeline without speakers.

## Deterministic images and reports

- `PROBE_SHOTS=<dir>` enables BMP output.
- `PROBE_SHOT_FRAMES=180,360,600` writes only those exact frames.
- `PROBE_SHOT_EVERY=<n>` writes every nth frame. If neither option is set,
  the legacy default is every 50 frames.
- `PROBE_BASELINES=<dir>` compares each written BMP byte-for-byte with the
  corresponding baseline and returns a non-zero status on a missing or changed
  image.
- `PROBE_REPORT=<path.csv>` writes per-frame timing, sample count, frame hash,
  audio depth, OAM DMA count, and visible-time PPU-register-write context.
- `PROBE_VERBOSE=1` mirrors a compact per-frame record to stderr.
- `PROBE_TRACE_WRITES=1` logs APU register writes; it is off by default so it
  cannot perturb profiling.

Run all reviewed cases with:

```sh
./probes/run_visual_regressions.sh
```

The runner verifies each ROM's SHA-256 before executing it. The ROM files are
already part of this repository; if they are removed for licensing reasons,
restore matching local copies under `games/` without changing the baselines.

## Capturing an intermittent artifact

For a suspect frame, capture it and its neighbors while producing a CSV that
can correlate the event with OAM DMA and mid-frame PPU writes:

```sh
PROBE_SHOTS=/tmp/capture \
PROBE_CAPTURE_FRAME=900 PROBE_CAPTURE_RADIUS=3 \
PROBE_REPORT=/tmp/capture.csv \
cargo run --release -- probe games/mario.nes "start@120-135" 904
```

The output contains frames 897 through 903. Inspect the matching CSV rows for
`oam_dmas`, `visible_ppu_writes`, and the last register/scanline/dot. Sprite
overflow is not yet exact in the scanline renderer. Its implementation and the
corresponding capture-report extension are tracked centrally in `../TODO.md`.
