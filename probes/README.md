# ROM probe and visual regressions

Always profile an optimized build. The basic command is:

```sh
cargo run --release -- probe <rom> "<button@from-to,...>" <frames>
```

The final `PROBE_SUMMARY` records emulated FPS, average/p95/max host frame
time, generated samples and long-run sample drift against actual emulated CPU
cycles. Audio queue fields are
`4294967295` (`BACKLOG_UNAVAILABLE`) in the default headless mode.

Set `PROBE_MAX_SAMPLE_DRIFT=<samples>` to make the probe fail when absolute
drift exceeds the given threshold. `PROBE_REQUIRE_HEALTHY_AUDIO=1` additionally
requires realtime mode, an available queue below its configured high-water
mark, and zero dropped samples, estimated underflows, or stream reopens.

Set `PROBE_REALTIME=1` to use the windowed frontend's sub-frame SDL audio pump
and chunk pacer. That mode additionally records queue minimum/maximum/end
depth, dropped samples, estimated underflow samples, and device reopens. Select
`NES_AUDIO_PROFILE=low|balanced` exactly as in the windowed emulator. It needs
a working SDL audio device; CI can use `SDL_AUDIODRIVER=dummy` to exercise the
pipeline without speakers. Normal playback and the standalone `audio_probe`
example use the same process-wide SDL3 bound-stream pump, 48 kHz signed-16-bit
format, adaptive queue controller, and pending-only high-water backpressure.
Clock correction happens in the pump before SDL; the live stream remains fixed
at 48 kHz. SDL queue inspection and submission happen at most once per 16 ms
interval, and a bound device found paused under backpressure is resumed in
place.

## Deterministic images and reports

- `PROBE_SHOTS=<dir>` enables BMP output.
- `PROBE_SHOT_FRAMES=180,360,600` writes only those exact frames.
- `PROBE_SHOT_EVERY=<n>` writes every nth frame. If neither option is set,
  the legacy default is every 50 frames.
- `PROBE_BASELINES=<dir>` compares each written BMP byte-for-byte with the
  corresponding baseline and returns a non-zero status on a missing or changed
  image.
- `PROBE_REPORT=<path.csv>` writes per-frame timing, sample count, frame hash,
  cumulative emulated CPU cycles, total/SDL/pending audio depths, OAM DMA count,
  and visible-time PPU-register-write context.
- `PROBE_VERBOSE=1` mirrors a compact per-frame record to stderr.
- `PROBE_TRACE_WRITES=1` logs APU register writes; it is off by default so it
  cannot perturb profiling.

Run all reviewed cases with:

```sh
./probes/run_visual_regressions.sh
```

Run the two-minute audio-clock acceptance sweep across NROM, MMC1, and MMC3:

```sh
./probes/run_audio_validation.sh
```

It defaults to 7,200 frames per case and enforces at most one sample of drift.
Override the duration with `AUDIO_VALIDATION_FRAMES`; set `PROBE_REALTIME=1`
to add the strict host queue/drop/underflow/reopen check on a real audio device.

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
overflow state is not yet included in capture reports; that diagnostics work
is tracked centrally in `../TODO.md`.

## PPU/MMC3 test ROMs

The `test-rom` command runs ROMs that use the standard blargg status protocol
at `$6000-$6004`, plus older PPU/MMC3 suites that return a code at `$00F8`,
without opening SDL video or audio:

```sh
cargo run --release -- test-rom test-roms/local/example.nes 50000000
```

For a repeatable P0 suite, copy legally obtained test ROMs under
`test-roms/local/`, add their paths and SHA-256 hashes to
`test-roms/p0-cases.txt` (the checked-in manifest is pinned to the named
upstream revision), then run:

```sh
./test-roms/run_p0_validation.sh
```

Alternatively, point the runner at an existing checkout of the pinned test-ROM
repository instead of copying ROMs into `test-roms/local/`:

```sh
NES_TEST_ROMS_ROOT=../nes-test-roms ./test-roms/run_p0_validation.sh
```

The runner rejects hash mismatches and writes a tab-separated report containing
the emulator Git revision, NTSC configuration, case, ROM hash, result, and
diagnostic. Set `P0_RESULTS` to retain that report at a specific path. Missing,
mismatched, or failing cases make the command fail.
