# P0 baseline results

Recorded on 2026-07-13 in a Linux release build. These figures are evidence
for the current baseline, not universal performance promises; rerun the probe
on the target host before changing PPU timing.

## Deterministic timing validation

The Rust suite now has 163 passing tests. P0 coverage records exact
mapper-visible background, prefetch, dummy, and sprite fetch addresses/dots;
blanked rendering; sprite-zero left-edge and x=255 behavior; vblank/NMI races;
odd-frame skipping; all background/sprite pattern-table combinations; PPUDATA
A12 activity; the eight-dot MMC3 low filter; and IRQ latch/reload/enable/
acknowledge/level behavior.

`test-roms/run_p0_validation.sh` provides hash-checked execution and revisioned
TSV results for external blargg/nesdev ROMs. The binaries remain outside this
checkout; the remaining P0 roadmap entries stay open until the configured
suite passes completely.

An exploratory run used `christopherpow/nes-test-roms` commit
`95d8f621ae55cee0d09b91519a8989ae0e64753b`; the checked-in manifest records
the individual ROM hashes. Results after fixing mapper-visible `$2006`, `$2007`
post-increment, and empty-slot sprite fetch addresses were:

| Suite | Passed | Remaining first failure |
| --- | ---: | --- |
| `ppu_vbl_nmi` combined | 0/10 | 01 #2, VBL period is way off |
| `sprite_hit_tests_2005.10.05` | 8/11 | 07 #3; timing ROMs 09/10 also fail |
| `sprite_overflow_tests` | 0/5 | 1 #5; later ROMs depend on earlier passes |
| `mmc3_irq_tests` (revision B) | 3/5 | Details #7 and scanline timing #2 |

MMC3 counter clocking, manual `$2006`/`$2007` A12 clocking, and revision-B
zero-latch behavior now pass their dedicated ROMs. Remaining failures are kept
visible rather than being treated as a completed P0 acceptance run.

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
a mid-frame CPU write. The capture predates the mapper-visible `$2006/$2007`
and empty-sprite-fetch fixes above. Remaining MMC3 scanline-ROM failures point
to CPU/PPU alignment and interrupt timing rather than the removed dot-260
approximation.

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
