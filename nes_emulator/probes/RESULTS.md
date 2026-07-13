# P0 baseline results

Recorded on 2026-07-13 in a Linux release build. These figures are evidence
for the current baseline, not universal performance promises; rerun the probe
on the target host before changing PPU timing.

## Two emulated minutes, headless

Each run covered 7,200 completed frames. All produced 5,283,327 samples with
only +0.089 sample of cumulative clock drift after the warm-up frame.

| Case | Mapper | Emulated FPS | Avg host frame | p95 | Max |
| --- | ---: | ---: | ---: | ---: | ---: |
| SMB1 | 0 / NROM | 1,201 | 0.833 ms | 0.945 ms | 1.785 ms |
| Zelda | 1 / MMC1 | 1,098 | 0.911 ms | 1.360 ms | 3.467 ms |
| SMB2 | 4 / MMC3 | 1,022 | 0.979 ms | 1.438 ms | 4.962 ms |

This leaves ample CPU/rendering margin for a 60.0985 FPS presentation target.
The nine reviewed BMPs in `baselines/` cover frames 180, 360, and 600 for
each case and are checked byte-for-byte by `run_visual_regressions.sh`.

## Artifact capture

The SMB2 transition issue is reproducible with:

```sh
PROBE_SHOTS=/tmp/smb2-capture \
PROBE_CAPTURE_FRAME=600 PROBE_CAPTURE_RADIUS=3 \
PROBE_REPORT=/tmp/smb2-capture.csv \
cargo run --release -- probe games/smario2.nes \
  "start@120-135,a@240-255,right@300-600" 603
```

Frames 597-603 show the transition into a mostly black playfield. Their report
rows contain one OAM DMA and zero visible-time PPU register writes per frame,
so the capture does not implicate an extra DMA, the fixed `$FF` OAM-Y wrap, or
a mid-frame CPU write. The leading hypothesis is the already-listed P1 work:
MMC3 A12 IRQ edges and dot-accurate PPU fetch/scroll timing. No additional
timing change was made from this capture alone.

## Host audio

`PROBE_REALTIME=1` exercises the same SDL audio pump and backlog-controlled
pacing as the windowed emulator. The host audio device is unavailable inside
the filesystem sandbox used for these results, so the long validation uses
SDL's dummy sink. The summary records queue min/max/end depth plus dropped,
underflow, and reopen counters, making a real-device rerun directly comparable.

The 7,200-frame dummy-sink run completed in 127.768 seconds at the sink-locked
56.352 FPS. Queue depth remained bounded at 2,932-14,740 bytes and ended at
8,396 bytes, with zero dropped samples, underflow samples, or device reopens.
The below-nominal rate reflects this dummy sink's effective clock, not renderer
capacity; backlog pacing intentionally follows the host DAC to preserve audio.
