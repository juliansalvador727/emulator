# P0/P1 PPU baseline results

Recorded on 2026-07-15 in a Linux release build. These figures are evidence
for the current baseline, not universal performance promises; rerun the probe
on the target host before changing PPU timing.

## Deterministic timing validation

The Rust suite now has 274 passing tests. P0/P1 coverage records exact
mapper-visible background, prefetch, dummy, and sprite fetch addresses/dots;
blanked rendering; sprite-zero left-edge and x=255 behavior; vblank/NMI races;
odd-frame skipping; all background/sprite pattern-table combinations; PPUDATA
A12 activity; the eight-dot MMC3 low filter; and IRQ latch/reload/enable/
acknowledge/level behavior. PPU register coverage also includes palette
mirroring, greyscale/emphasis output, the CPU-facing I/O latch, partial-bit
refresh, deterministic decay, OAM attribute read masking, delayed PPUMASK
rendering ownership, per-pixel left-edge clipping changes, disabled-rendering
backdrop output, and the `$2002`/`$2000` NMI suppression/control windows.

`test-roms/run_p0_validation.sh` provides hash-checked execution and revisioned
TSV results for external blargg/nesdev ROMs. The binaries remain outside this
checkout. The configured suite now passes completely; broader compatibility
work remains tracked in the P0 roadmap.

An exploratory run used `christopherpow/nes-test-roms` commit
`95d8f621ae55cee0d09b91519a8989ae0e64753b`; the checked-in manifest records
the individual ROM hashes. Results after fixing dynamic CPU branch/page-cross
cycles, OAM-DMA stalls, CPU-visible register phase, vblank/NMI edges, odd-frame
sampling, sprite evaluation, mapper-visible `$2006`/`$2007` activity, and
empty-slot sprite fetch addresses were:

| Suite | Passed | Remaining first failure |
| --- | ---: | --- |
| `ppu_vbl_nmi` combined | 10/10 | None |
| `ppu_open_bus` | 1/1 | None |
| `sprite_hit_tests_2005.10.05` | 11/11 | None |
| `sprite_overflow_tests` | 5/5 | None |
| `mmc3_irq_tests` (revision B) | 5/5 | None |

All 23 configured cases pass, including PPU open-bus behavior, MMC3 counter/manual A12 clocking,
revision-B zero-latch behavior, sprite-hit/overflow timing, vblank/NMI races,
and exact odd/even frame timing.

The focused `run_mapper_validation.sh` manifest passes 8/8: both
`cpu_dummy_writes` ROMs verify the original-value and modified-value writes on
adjacent cycles; the repository-authored MMC1 ROM verifies the 512 KiB SUROM
outer bank, four SXROM PRG-RAM banks, RAM disable, and mapper reset state; and
MMC3 IRQ tests 1-4 plus revision B verify counter, A12, scanline, and zero-latch
behavior.

This result was reproduced on revision `6ec8976c8df0b1a708b5d6afe59defa2a5dc5ce6`
with `NES_TEST_ROMS_ROOT=../nes-test-roms`; the full suite again passed 22/22.

The P1 register pass reran `ppu_vbl_nmi.nes` and `ppu_open_bus.nes` from the
same upstream revision: the combined vblank/NMI ROM again reported all 10
tests passed, and the open-bus ROM reported passed. The older standalone
`vbl_nmi_timing` set was also explored: frame basics and even/odd timing pass,
while its five CPU-alignment-sensitive ROMs still fail. Those CPU-visible
accesses currently occur at instruction granularity, so their single-cycle bus
placement belongs to the separately tracked CPU/PPU bus-timing work rather
than being hidden by a compensating PPU offset.

The dedicated `scrolltest/scroll.nes` ROM (SHA-256
`04ebe8b768ffc31fc1fa18a21f2e1884d0d3df2588f5efee53e881a339874db7`)
was also run headlessly for 180 frames. The reviewed output showed its complete
multidirectional-scrolling test pattern without seams or blank regions.

## Two emulated minutes, headless

Each run covered 7,200 completed frames. The first three produced 5,750,464
samples and Mike Tyson produced 5,750,465 because its callback boundaries
landed on the other side of one sample tick. Drift is measured against actual
emulated CPU cycles between callbacks, rather than inferred from nominal video
rate: SMB1 was -0.264 sample, Zelda -0.103, SMB2 +0.031, and Mike Tyson -0.176.
The integer sample clock itself is exact over two CPU-clock minutes; the
sub-sample values are the expected boundary phase at the two callback
endpoints.

| Case | Mapper | Emulated FPS | Avg host frame | p95 | Max |
| --- | ---: | ---: | ---: | ---: | ---: |
| SMB1 | 0 / NROM | 189 | 5.290 ms | 7.562 ms | 27.842 ms |
| Zelda | 1 / MMC1 | 183 | 5.461 ms | 7.287 ms | 12.173 ms |
| SMB2 | 4 / MMC3 | 179 | 5.600 ms | 7.366 ms | 17.629 ms |
| Mike Tyson | 9 / MMC2 | 354 | 2.829 ms | 3.154 ms | 7.313 ms |

This leaves ample CPU/rendering margin for a 60.0985 FPS presentation target.
`probes/run_audio_validation.sh` now makes the one-sample drift ceiling a
repeatable acceptance check instead of leaving these figures as a manual
observation.
The twelve reviewed BMPs in `baselines/` cover frames 180, 360, and 600 for
the original three cases plus three gameplay milestones for Mike Tyson. They
are checked byte-for-byte by `run_visual_regressions.sh`.
The suite was rerun while closing P0; all nine current images matched, including
the three MMC3/SMB2 frames, with zero baseline failures.
It was rerun again after the P1 PPU-register timing pass; all nine still
matched with zero baseline failures.

Mapper 9/MMC2 validation adds Mike Tyson's Punch-Out!! to the same runner.
Its three reviewed frames cover the opponent card, ring entry, and an active
Glass Joe bout after scripted Start presses. A separate 3,200-frame probe
reached the bout at 352.863 emulated FPS, produced 2,555,727 samples with
+0.115 sample of boundary drift, and reported zero drops, underflows, device
reopens, or baseline failures.

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

The latency frontend added after that long baseline moves input/presentation to
the start of vblank, delivers audio in 256-sample chunks, and exposes `low` and
`balanced` profiles. A 360-frame low-profile dummy-sink smoke test completed
with zero dropped samples or device reopens and a measured application queue of
640-3,724 bytes (3.6-21.1 ms). The dummy sink reported 99 underflow samples and
ran at its own 63.1 FPS effective clock, so this is an integration smoke test,
not a real-device latency claim. The sandboxed Pulse device remained
unavailable; real-device acceptance must be rerun on the target host.

The unified process-wide SDL3 revision was run through the full SMB1 frontend
for 1,500 frames (24.96 seconds) with a 60 ms target. SDL's dummy sink held the
submitted queue at 5,760 bytes; pending audio stayed within its 8,192-byte
budget, with zero stream reopens and zero underflows. The dummy clock was slow
enough to force pending-sample drops, confirming that sustained pressure stays
bounded without clearing the live stream. The release executable statically
contains one SDL3 runtime and has no SDL2 dependency. This is a recovery-path
check, not a real-device quality or drift result.

After the WSLg stall investigation, the pump was aligned with the stable C
frontend's call cadence: one queue query and at most one submission per 16 ms,
with in-place recovery if a backpressured bound device reports itself paused.
The bundled SDL3 build also uses its normal desktop configuration instead of
Unix-console mode. A 1,320-frame (21.96-second) dummy-sink soak crossed the old
18-second failure point with zero reopens, paused-device resumes, or
underflows. The deliberately slow dummy clock filled the bounded pending queue
and forced drops, so real WSLg acceptance remains a target-host test.
