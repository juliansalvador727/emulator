# NES Emulator — Consolidated Roadmap

This is the single source of truth for unfinished work in this repository, and
the consolidation point for important operational knowledge (host audio failure
model, native Windows build). The Rust emulator at the repository root is the
active project; the older C emulator under `NES/` is retained as a reference
and has a separate, lower-priority backlog at the end of this file.

Current verified baseline (2026-07-14):

- 234 passing Rust tests.
- All 256 6502 opcodes (official and undocumented); `nestest` matches the
  reference for all 8,991 instruction lines. `instr_test-v5` (16/16),
  `instr_timing` (2/2), and `instr_misc` (4/4) pass.
- NROM, MMC1, UxROM, CNROM, MMC3, AxROM, and GxROM/GNROM.
- Dot-driven background and sprite rendering with mapper-visible PPU fetches.
- P1 PPU register and memory behavior complete. OAM DMA is modeled as real
  alternating get/put cycles with OAMADDR wrapping, and DMC DMA arbitrates with
  it on get cycles; the remaining sub-cycle DMA fidelity is gated on the
  cycle-accurate CPU bus-timing work.
- Five-channel APU including DMC DMA, filtering, and SDL3 bound-stream playback.
- Audio stall watchdog with staged recovery; stream open/destroy isolated on a
  helper thread (see the host audio section below for the WSLg failure model).
- Host audio uses the safe sdl3 crate API; the direct sdl3-sys dependency is
  gone and the only remaining `unsafe` is two documented `impl Send` markers in
  `src/audio.rs` (the crate's !Send audio types are conservative; SDL3 streams
  are internally locked and documented thread-safe).
- Fullscreen (`--fullscreen`, F11) and borderless windowed-fullscreen
  (`--windowed-fullscreen`/`--borderless`) with aspect-correct letterboxing.
- Native Windows cross-build via `cargo win` (mingw-w64, aliases in
  `.cargo/config.toml`); verified on hardware with no audio/fps/latency issues.
  Native Windows is the preferred way to play on WSL hosts.
- Deterministic visual regressions for SMB1, Zelda, and SMB2.
- Two-minute release probes exceed real-time performance with stable sample
  production; see `probes/RESULTS.md`.
- Frontend presentation/input now runs at vblank before the CPU's NMI handler;
  audio is delivered and paced in 256-sample sub-frame chunks; low-latency and
  WSL-safe profiles plus deterministic one-frame run-ahead are available.

## P0 — Replace the scanline compositor with a real dot-driven PPU

The hardware-timed pipeline is now the production renderer. Remaining work in
this section is focused on edge-case fidelity and test-ROM validation.

- [x] Model background fetches on their real dots:
  - nametable, attribute, pattern-low, and pattern-high bus accesses;
  - fetch latches and background shift registers;
  - shifter reloads, fine-X selection, and per-dot background pixels;
  - prefetches on dots 321-336 and dummy fetches on dots 337-340.
- [x] Move loopy scrolling entirely onto the hardware fetch timeline:
  - coarse-X increments every tile;
  - Y increment at dot 256;
  - horizontal copy at dot 257;
  - vertical copy on pre-render dots 280-304;
  - correct behavior when rendering is disabled or toggled mid-frame.
- [x] Implement the sprite pipeline:
  - primary/secondary OAM and clear/evaluation timing;
  - the eight-sprite limit and hardware sprite-overflow behavior;
  - sprite pattern fetches on dots 257-320;
  - 8x8/8x16 addressing, flipping, priority, and sprite shift registers;
  - per-dot sprite-zero hit, including left-edge and x=255 behavior.
- [x] Produce each visible pixel from the background and sprite pipelines, then
  remove `render::render_scanline`, `NesPPU::composite_scanline`, and the
  pending scanline-derived sprite-zero-hit mechanism from production timing.
- [x] Put every real PPU memory access on one modeled PPU address-bus path so
  mappers observe the same CHR fetches that generate pixels.
- [x] Keep NTSC odd-frame dot skipping and frame completion exact after the
  renderer migration.
- [x] Add focused tests for each fetch phase, scrolling, sprite-zero, overflow,
  vblank/NMI, rendering blanking, mapper-visible accesses, and odd frames.
- [x] Run established PPU test ROMs for scrolling, sprite-zero, overflow,
  vblank/NMI, and odd-frame behavior through `test-roms/run_p0_validation.sh`.
- [x] Re-run release probes and reviewed visual regressions after each vertical
  slice; investigate deliberate baseline changes rather than blindly replacing
  images.

Acceptance: rendering is driven by actual per-dot fetches, MMC3 sees real PPU
bus activity, relevant nesdev/blargg PPU tests pass, and SMB1/Zelda/MMC3 games
remain visually and audibly stable at 60.0985 FPS.

## P0 — Drive MMC3 IRQs from actual PPU A12 edges

Do this as part of the dot-driven fetch work, not as a second synthetic
scanline schedule.

- [x] Replace `Mapper::on_scanline()` with a mapper hook that observes PPU bus
  addresses/accesses without making non-MMC3 mappers timing-aware.
- [x] Track PPU A12 state and clock the MMC3 counter only on qualified low-to-
  high transitions, including the required low-time filter.
- [x] Verify background/sprite pattern-table combinations, blanked rendering,
  pre-render fetches, short low pulses, and extra fetches caused by PPU access.
- [x] Preserve and test `$C000/$C001/$E000/$E001` latch, reload, enable,
  acknowledge, and level-triggered IRQ behavior. MMC3 revision differences
  remain deferred until cartridge/submapper metadata can identify them.
- [x] Remove the dot-260 approximation and its tests once edge-driven tests are
  in place.
- [x] Validate with MMC3 IRQ test ROMs and SMB2/SMB3-style status-bar splits;
  compare against the captured SMB2 transition documented in
  `probes/RESULTS.md`.

Acceptance: IRQ timing is a consequence of real PPU fetch addresses and stable
status-bar splits no longer exhibit one-line jitter.

## P1 — Finish PPU register and memory behavior

Status: complete. The dot-driven renderer now uses delayed PPUMASK ownership,
per-pixel clipping, blanked backdrop output, and dot-windowed vblank/NMI state.

- [x] Mirror `$3000-$3EFF` to `$2000-$2EFF` for reads and writes instead of
  panicking.
- [x] Correct palette behavior:
  - `$3F10/$3F14/$3F18/$3F1C` aliases on both reads and writes;
  - palette-space mirroring through `$3FFF`;
  - buffered-read interaction and open-bus high bits;
  - PPUMASK grayscale and color-emphasis output.
- [x] Model PPU open bus and decay closely enough for the relevant test ROMs.
- [x] Complete `$2002/$2004/$2007` behavior during rendering, including OAM
  access restrictions/corruption and rendering-time `$2007` address changes.
  Verified with `oam_read.nes` (SHA-256
  `f298973dabeb61ca35007445f7a615f77e87703c958c870986af83b1aabde926`),
  which reports status 0 through the blargg `$6000` protocol.
- [x] Model delayed PPUMASK rendering enable/disable effects and exact left-edge
  clipping transitions.
- [x] Tighten pre-render/vblank ordering, `$2002` race windows, NMI suppression,
  and immediate NMI behavior on `$2000` changes.
- [x] Add targeted register tests plus relevant nesdev/blargg ROM results.
  `ppu_vbl_nmi.nes` (SHA-256
  `8dbab1be785585c399cf055ef02147b788ab75fd80e81cf9568a2feafc03fb7d`)
  reports all 10 tests passed, and `ppu_open_bus.nes` (SHA-256
  `d4208a3ff6340532dd0fced7f9d408d5b6585853a0ddc9c1f64ee1722ef08e67`)
  reports passed through the blargg `$6000` protocol.
  The older standalone `vbl_nmi_timing` ROMs that depend on the CPU cycle of
  each register access remain acceptance coverage for the bus-cycle timing
  work below; do not compensate for instruction-granularity CPU accesses with
  an incorrect global PPU offset.

## P1 — Implement real DMA and CPU/PPU/APU bus timing

- [x] Give OAM DMA its real 513/514 CPU-cycle stall based on CPU parity.
- [x] Advance the PPU and APU while the CPU is stalled.
- [x] Model OAM DMA reads/writes on alternating CPU cycles and preserve the
  current OAMADDR wrapping behavior. `Bus::run_oam_dma` streams the page one
  byte per get/put pair through `NesPPU::oam_dma_write` (post-incrementing
  OAMADDR, discarded during rendering) instead of copying it atomically at the
  `$4014` write.
- [x] Define and test DMC DMA/OAM DMA arbitration for the common (non-
  corrupting) alignment: `Bus::service_dmc_during_oam` gives a pending DMC
  fetch priority on a get cycle, adding a halt/alignment cycle plus the DMC get
  and delaying — never dropping — the OAM data. The rarer sub-cycle case where
  the steal corrupts an OAM byte, and true instruction-boundary effects, depend
  on a mid-instruction cycle-accurate CPU and are tracked under "CPU
  compatibility and timing" below; `sprdma_and_dmc_dma` still reports $0F for
  that reason.
- [x] Add integration tests for parity, elapsed PPU dots, APU progression, and
  DMA interaction (`bus.rs`: page copy with OAMADDR wrap, PPU/APU advancement
  across the whole stall, and a DMC steal during OAM DMA).
- [~] Slice 4 (DMC-DMA-during-read): a non-OAM DMC fetch is now serviced on the
  CPU's next read cycle (`Bus::dmc_halt_before_read`, called from
  `CPU::mem_read`) instead of blindly after the access, so the RDY halt re-reads
  the CPU's address before the real read -- a side-effecting register
  ($2007/$4016) is read several times, the documented DMC-DMA-during-read
  behavior (unit test `dmc_dma_during_a_2007_read_repeats_the_read`). Regression
  free (nestest, all prior ROMs, 235 unit tests, and audio/visual baselines with
  0 dropped/underflow samples). The blargg acceptance ROMs still do NOT pass and
  are gated on cycle-exact DMC timing, not the CPU: `sprdma_and_dmc_dma`/`_512`
  need exact per-alignment DMA cycle counts. The re-read count is a fixed 4-cycle
  approximation until that lands. `4-irq_and_dma` additionally needs per-cycle
  IRQ sampling through the DMA stall.
- Correction: `dmc_dma_during_read4/*` do NOT hang in `sync_dmc.s` (earlier claim
  disproven). They run to completion and report over console/serial with an
  internal CRC and no `$6000` signature, so the harness saw a timeout rather than
  a result. `PRINT_HOOK=<print_char_ addr>` dumps the byte stream; `$e679` is
  `print_char_` in these ROMs. Beware: the shipped `.nes` files disagree with the
  header comments in `source/` -- the `check_crc` constant in the binary is the
  only oracle. `dma_2007_write` genuinely **passes**.
- [ ] `dmc_dma_during_read4/double_2007_read` and `read_write_2007` produce no
  output at all (they hang before their first print). Neither uses DMC DMA -- they
  test PPU `$2007` dummy-read quirks (`sta $2007,x` dummy-reads before writing;
  `lda $20F7,x` with X=$10 page-crosses into a double read). This is a PPU bug,
  not an APU one.

## P1 — CPU compatibility and timing

- [x] Implement commonly used unofficial 6502 opcodes, addressing modes,
  flags, dummy accesses, page-cross timing, and unstable-opcode behavior where
  commercial software requires it. All 256 opcodes now have a table entry and a
  dispatch arm (`opcodes.rs`/`cpu.rs`); the `todo!()` fallthrough is gone. NOP,
  LAX, SAX, SBC, DCP, ISB, SLO, RLA, SRE, RRA, ANC, ALR, ARR, AXS and the
  unstable ANE/LXA/SHx/TAS/LAS/JAM are covered. Indexed reads, indexed stores,
  and read-modify-writes issue their dummy read at the un-carried address, and
  ROL/ROR on memory now set the Z flag. Unstable ANE/LXA use the all-ones magic
  constant blargg's tests were captured with.
- [x] Complete the `nestest` trace beyond the first unofficial opcode and add
  dedicated illegal-opcode test ROM results. The trace marks undocumented
  opcodes with `*` and is now side-effect-free (it no longer reads PPU/APU
  registers for its `= xx` annotation), so `cargo run --release -- nestest`
  matches the reference for all 8,991 instruction lines. Test ROM results
  (blargg `$6000`): `instr_test-v5` 16/16, `instr_timing` 2/2, and
  `instr_misc` 4/4 (including all four dummy-read groups) pass.
- [~] Audit instruction, interrupt, reset, and DMA timing at bus-cycle
  granularity. Instruction timing (`instr_timing`), dummy reads (`instr_misc`),
  page-cross penalties, and one-instruction CLI/SEI/PLP interrupt latency (IRQ
  is polled at the instruction boundary using the pre-instruction I flag) are
  done and tested. The cycle-stepped foundation now exists (slice 1, done): the
  CPU ticks the PPU/APU on each memory access (`CPU::bus_cycle`, gated by an
  `executing` flag so trace/test pokes don't advance time) and reconciles the
  remaining internal cycles in `finish_instruction`, so per-instruction cycle
  totals are unchanged (nestest still matches 8,991 lines; `instr_timing`,
  `instr_misc`, `instr_test-v5`, and the SMB1/Zelda/SMB2 visual baselines all
  stay green) while interrupt entry is now the correct 7 cycles.
  Slice 2 (done) built the cycle-accurate interrupt model on that foundation:
  per-cycle interrupt sampling with penultimate-cycle recognition (`CPU::cycle`
  latches `_delayed` line states), a unified 7-cycle `service_interrupt` for
  BRK/IRQ/NMI with a late (PCL-push) vector decision so a pending NMI hijacks a
  BRK/IRQ vector, RTI's immediate I-flag effect, and the branch-specific poll
  point (end of cycle 2). `cpu_interrupts_v2`: `1-cli_latency` and
  `2-nmi_and_brk` now PASS (were failing); no regressions (nestest, instr_*,
  ppu_vbl_nmi, visual baselines all green). The three still failing are gated on
  work outside the interrupt core: `3-nmi_and_irq` needs sub-PPU-dot NMI sync
  resolution (the PPU advances 3 dots per CPU-cycle tick, so NMI edge timing is
  quantized); `5-branch_delays_irq` uses `sync_apu`/`CUSTOM_IRQ` and needs exact
  APU frame-counter IRQ timing (the separate "APU correctness" item below);
  `4-irq_and_dma` needs per-cycle IRQ sampling during DMA (slice 4).
  Slice 3 (done, no code needed): all 7 `vbl_nmi_timing` sub-tests
  (`1.frame_basics`..`7.nmi_timing`) now pass with NO compensating PPU offset --
  the per-cycle CPU timing places each register access on its true cycle, and
  the only PPU/CPU sync constants are the legitimate 3/6-dot NMI clock-domain
  delays (`nmi_interrupt_at`). `ppu_vbl_nmi` (10/10) and `oam_read` stay green.
  Still to do: the DMA sub-cycle cases (slice 4). See the plan file.
- [ ] Implement accurate reset and power-cycle state separately; do not treat
  application startup, reset, and save-state restore as the same operation.
  (Still one `reset()`; power-on vs. reset RAM/APU/PPU differences are unmodeled.)

## P1 — APU correctness

- [x] Validate `$4017` write parity/delay and frame-counter sequencing with APU
  test ROMs. `apu_test` 8/8, `blargg_apu_2005.07.30` 11/11 (includes the `$4017`
  parity/delay and frame-counter sequencing singles). `cpu_interrupts_v2/
  5-branch_delays_irq` now passes too, so the frame-counter IRQ timing it was
  blocked on is good. Remaining `apu_reset` 4017_written/4017_timing failures are
  power-on frame-counter *phase*, tracked under the reset/power-cycle item, not
  here.
- [ ] Validate DMC fetch stalls, IRQ assertion/acknowledgement, address wrapping,
  looping, and DMA arbitration against test ROMs. Measured against
  `dmc_dma_during_read4` (see the correction above for how to read these):
  - [x] `$4016` extra-shift count: FIXED (62a00ae). Hardware steals exactly **one**
    shift per halt (`08`->`07`); we stole three. The repeats are real, but /OE
    stays asserted across them so the 4021 only clocks once (`Joypad::peek`).
    Proven by sweeping the shape: at three stalls the `$4016` count tracked the
    re-read count exactly (1->`07`, 2->`06`, 3->`05`) while `dma_2007_read` needs
    2-3 re-reads for its accepted `33 44`/`44 55` -- so no single re-read count
    satisfies both, and the extra shifts, not the extra reads, were the error.
  - [ ] The DMA still fires **one cycle late**: each iteration shifts the DMA one
    clock, and both `dma_2007_read` and `dma_4016_read` put our anomaly on
    iteration 4 (delay 3354) where hardware has iteration 3 (delay 3353).
    Magnitudes are now correct; only the position is wrong. This is the last thing
    between us and both ROMs passing.
  - `dma_2007_read` already produces an accepted variant (`44 55` = three extra
    reads); only its position is wrong.
  - Dead ends, do NOT retry: (a) deriving the stall count from `self.cycles`
    parity -- hangs `dma_2007_write`; a 2-cycle stall hangs generally, and a
    4-cycle stall removes the anomaly entirely. (b) Giving the DMC timer a phase
    lead over the other APU units -- swept leads 0/1/2 with **no effect at all**,
    because `sync_dmc.s` re-synchronizes the ROM to the DMC timer and cancels any
    global phase offset. The residual cycle is therefore NOT the timer phase: it
    is in the servicing path, i.e. which CPU read cycle the halt is inserted
    before. Fixing it properly means modeling RDY per-cycle in the CPU instead of
    calling `dmc_halt_before_read` at the top of `CPU::mem_read`.
- [ ] Validate channel mixer levels, nonlinear mixing, filters, sample-rate
  conversion, and long-run clock drift against known references. `apu_mixer` 4/4
  (square/triangle/noise/dmc) pass; long-run clock drift still unmeasured.
- [ ] Add PAL and Dendy APU timing tables when region support is introduced.
- [ ] Keep probe reporting for queue depth, drops, underflows, device reopens,
  and sample drift green during timing changes.

## P1 — Low-latency host presentation, input, and audio

Status: implementation complete and validated on native Windows hardware. The
normal path preserves console timing, while optional run-ahead reduces
game-internal response latency without committing speculative machine or audio
state.

### Host audio failure model and recovery (consolidated from fix_apu.md)

The WSLg/Pulse audio server wedges nondeterministically mid-session: the SDL3
bound stream stops being consumed while `SDL_AudioStreamDevicePaused` still
reports active. Observed on hardware (2026-07-14):

- The queue freezes exactly at target, shutting the write gate; every fresh
  sample then drops as backpressure while `underflows`/`reopens` stay 0.
- A resume on the wedged stream "succeeds" and drains about one device buffer
  before re-freezing, so equality-based freeze detection resets and never
  escalates. Sustained backpressure is the reliable stall signal — a healthy
  stream shows none at all.
- After destroying a wedged stream, `SDL_OpenAudioDeviceStream` can block
  forever (the server is wedged process-wide). Any blocking SDL open/destroy
  on the pump thread would freeze stats and leak the unbounded producer
  channel.

Implemented response (`src/audio.rs`): a `StallWatchdog` keyed on sustained
backpressure escalates resume (250 ms) → clear+resume (600 ms) → destroy and
reopen (1.25 s), trusting recovery only after 500 ms of continuous health;
open/destroy run on the `audio-open` helper thread so a hung SDL call leaves
the pump draining and `pending` bounded. Residual limitation: if the open
blocks forever, audio stays off for that session (one stuck helper thread);
gameplay/video/input continue and only `wsl --shutdown` restores WSLg audio.

- [ ] If WSLg wedges recur in practice, verify the clear+resume stage recovers
  in place at least sometimes; otherwise consider retrying the blocked open on
  a second helper thread with a cap.

### Native Windows build (consolidated from windows.md)

Running natively removes the WSLg bridge entirely — verified: no latency, fps,
or audio issues, and lower latency targets (20 ms) work on WASAPI. `cargo win
-- <rom>` cross-compiles with mingw-w64 (`gcc-mingw-w64-x86-64`,
`g++-mingw-w64-x86-64`, `rustup target add x86_64-pc-windows-gnu`) and launches
the `.exe` through WSL interop. Native builds pin the LowLatency profile at
compile time because `WSL_DISTRO_NAME` leaks into Windows processes launched
from a WSL shell. See README for user-facing instructions.

- [x] Raise a host `frame_ready` event at the start of vblank independently of
  NMI enable, and service it before any NMI-handler controller poll.
- [x] Move SDL presentation and event handling out of the recursive bus
  callback so the frontend owns explicit frame boundaries.
- [x] Deliver APU output every 256 samples and pace those chunks across wall
  time instead of producing a frame-sized burst followed by a long sleep.
- [x] Add native low-latency and WSLg-safe profiles, a bounded total
  queued+pending budget, and stale-audio dropping when the sink falls behind.
  Playback uses a 48 kHz signed-16-bit bound stream in the same process-wide
  SDL3 runtime as video and input, matching the stable C frontend lifecycle. A
  small pre-SDL sample-count controller absorbs host-clock drift without
  changing emulation speed or repeatedly mutating the live SDL stream;
  high-water backpressure preserves the live stream and discards only excess
  samples that SDL has not accepted yet. The pump checks and writes at most
  once per 16 ms interval, matching the C frontend's frame-rate call cadence,
  and resumes a paused bound device in place instead of reopening it.
- [x] Pace exclusively from the fixed 48 kHz sample timeline. Queue depth is
  diagnostic/control state only, avoiding the former feedback loop that ran
  SMB1 at ~61.6 FPS while dropping about 1,100 samples per second.
- [x] Report presentation time, queued/pending/target/device audio depths,
  playback-rate correction, backpressure events, paused-device resumes, drops,
  underflows, reopens, production rate, and input-to-`$4016` poll time.
- [x] Add deep snapshots for CPU, bus, PPU, APU, joypad, and every supported
  mapper; use them for optional one-frame run-ahead with speculative audio
  delivery suppressed and canonical state advanced exactly once.
- [x] Keep reviewed SMB1/Zelda/SMB2 images unchanged by shifting scripted probe
  input one frame to represent the same game-state moments at the new boundary.

Acceptance: all unit tests and reviewed visual regressions pass; low-latency
mode uses a 40 ms SDL3 input target, balanced mode remains the automatic WSL
fallback, and stale input is bounded without a synchronous device reopen;
snapshot restore reproduces the same next frame and mapper RAM; speculative
audio never reaches SDL.

## P1 — Harden existing mappers and cartridge memory

- [ ] MMC3:
  - [x] implement `$A001` PRG-RAM enable and write protection;
  - finish four-screen nametable storage/behavior;
  - support relevant board/revision differences after NES 2.0 submappers exist.
- [ ] MMC1:
  - ignore consecutive serial writes on adjacent CPU cycles;
  - support SUROM/SXROM large-PRG banking when a target ROM requires it;
  - model PRG-RAM enable/banking for the relevant boards.
- [ ] Implement battery-backed PRG/CHR RAM persistence with atomic writes and a
  configurable save directory.
- [ ] Distinguish volatile RAM, nonvolatile RAM, ROM, and absent memory from
  cartridge metadata rather than assuming one 8 KiB PRG-RAM block.
- [ ] Add focused unit tests and at least one legal ROM/test-ROM validation case
  for every mapper behavior change.

## P1 — Cartridge formats and region metadata

- [ ] Parse NES 2.0 headers, mapper extensions, submappers, exponent/multiplier
  ROM sizes, PRG/CHR RAM and NVRAM sizes, console type, and region timing.
- [ ] Improve iNES validation for malformed/truncated files and ambiguous
  archaic headers while retaining trainer support.
- [ ] Use battery and RAM metadata to configure mapper memory and persistence.
- [ ] Select NTSC/PAL/Dendy timing from metadata with an explicit user override.
- [ ] Add parser fixtures for valid and invalid iNES/NES 2.0 combinations.

## P2 — Mapper and game-library expansion

- [ ] Choose new mappers from a documented target game library rather than
  numerical order. Record the motivating ROM/test ROM and expected behavior.
- [ ] Highest-value current candidates:
  - mapper 11 (Color Dreams);
  - mapper 13 (CPROM);
  - mapper 75 (VRC1);
  - mapper 180;
  - mapper 185.
- [ ] For each mapper, cover bank wrapping, CHR-RAM/ROM behavior, mirroring,
  PRG-RAM, bus conflicts where applicable, and reset state.
- [ ] Consider more complex IRQ/audio mappers only after the dot-driven PPU and
  cartridge metadata foundations are stable.

## P2 — Regions, input, and emulator features

- [ ] Add PAL and Dendy CPU/PPU/APU timing and runtime region selection.
- [ ] Add a second standard controller at `$4017`.
- [ ] Add peripherals only for a target title: Zapper, Four Score, and other
  expansion devices.
- [ ] Add versioned save states only after reset semantics and mapper state are
  explicit; include CPU, PPU pipeline, APU, DMA, controllers, RAM, mapper, and
  timing phase.
- [ ] Add configurable battery-save locations and clear error reporting.
- [ ] Consider NSF/NSFe playback and Game Genie only if they become product
  goals; neither should block core console accuracy.

## P2 — Validation, diagnostics, and maintenance

- [x] Create a repeatable test-ROM runner that records ROM hash, pass/fail
  result, emulator revision, and relevant region/configuration.
- [ ] Expand ROM-level visual baselines when a new mapper or timing-sensitive
  behavior is added; keep copyrighted ROMs out of git when licensing requires.
- [ ] Add sprite-overflow state and other new timing signals to probe capture
  reports when those features are implemented.
- [ ] Re-profile NROM, MMC1, and MMC3 after the dot renderer lands; preserve
  steady 60.0985 FPS, bounded audio queues, zero dropped samples, and no
  long-run sample drift on a real audio device.
- [ ] Keep user documentation synchronized with supported mappers, regions,
  controls, save behavior, test count, and known limitations.

## Optional legacy C emulator backlog (`NES/`)

The C tree is primarily a behavioral reference for the active Rust port. Only
take these on if maintaining that project becomes an explicit goal.

- [ ] Replace its MMC3 dot-260 IRQ approximation with qualified A12 edges.
- [ ] Improve mid-scanline PPU timing behavior.
- [ ] Add battery-backed persistent save RAM.
- [ ] Add Dendy timing/ROM support.
- [ ] Add keyboard multiplayer and original NES-controller support.
- [ ] Add UNIF, FDS, and IPS format support.
- [ ] Add MMC5/mapper 5.
- [ ] Decide whether BCD arithmetic is worth implementing; the NES CPU disables
  decimal arithmetic, so it is not required for console compatibility.
- [ ] Replace the Android placeholder UI action/text and review project-owned
  Android TODOs separately from vendored SDL activity/template code.

## Working rules

- Keep `cargo test` green and add a focused unit test for every hardware fix.
- For timing work, add a ROM-level probe or test-ROM result in addition to unit
  tests.
- Treat NES documentation and test ROMs as sources of truth; do not preserve an
  approximation merely because the legacy C emulator uses it.
- Make timing changes in small vertical slices that remain playable and
  measurable.
- Profile with `--release`; debug-mode frame rate is not a useful performance
  metric.
- Do not commit copyrighted ROMs unless redistribution is clearly permitted;
  record hashes and scripts so local validation remains reproducible.
