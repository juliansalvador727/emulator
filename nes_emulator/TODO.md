# NES Emulator — Consolidated Roadmap

This is the single source of truth for unfinished work in this repository.
`PPU_PLAN.md` and `MAPPER_PLAN.md` are historical implementation handoffs, not
current plans. The Rust emulator under `nes_emulator/` is the active project;
the older C emulator under `NES/` is retained as a reference and has a separate,
lower-priority backlog at the end of this file.

Current verified baseline (2026-07-13):

- 179 passing Rust tests.
- All official 6502 opcodes; `nestest` matches 5,003 official-opcode entries.
- NROM, MMC1, UxROM, CNROM, MMC3, AxROM, and GxROM/GNROM.
- Dot-driven background and sprite rendering with mapper-visible PPU fetches.
- Five-channel APU including DMC DMA, filtering, and SDL2 playback.
- Deterministic visual regressions for SMB1, Zelda, and SMB2.
- Two-minute release probes exceed real-time performance with stable sample
  production; see `probes/RESULTS.md`.

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

- [x] Mirror `$3000-$3EFF` to `$2000-$2EFF` for reads and writes instead of
  panicking.
- [x] Correct palette behavior:
  - `$3F10/$3F14/$3F18/$3F1C` aliases on both reads and writes;
  - palette-space mirroring through `$3FFF`;
  - buffered-read interaction and open-bus high bits;
  - PPUMASK grayscale and color-emphasis output.
- [x] Model PPU open bus and decay closely enough for the relevant test ROMs.
- [ ] Complete `$2002/$2004/$2007` behavior during rendering, including OAM
  access restrictions/corruption and rendering-time `$2007` address changes.
- [ ] Model delayed PPUMASK rendering enable/disable effects and exact left-edge
  clipping transitions.
- [ ] Tighten pre-render/vblank ordering, `$2002` race windows, NMI suppression,
  and immediate NMI behavior on `$2000` changes.
- [ ] Add targeted register tests plus relevant nesdev/blargg ROM results.

## P1 — Implement real DMA and CPU/PPU/APU bus timing

- [x] Give OAM DMA its real 513/514 CPU-cycle stall based on CPU parity.
- [x] Advance the PPU and APU while the CPU is stalled.
- [ ] Model OAM DMA reads/writes on alternating CPU cycles and preserve the
  current OAMADDR wrapping behavior.
- [ ] Define and test DMC DMA/OAM DMA arbitration, including DMC steals during
  OAM DMA and instruction-boundary effects.
- [ ] Add integration tests for parity, elapsed PPU dots, APU progression, and
  DMA interaction; validate with DMA timing test ROMs.

## P1 — CPU compatibility and timing

- [ ] Implement commonly used unofficial 6502 opcodes, addressing modes,
  flags, dummy accesses, page-cross timing, and unstable-opcode behavior where
  commercial software requires it. Replace the remaining `todo!()` dispatch.
- [ ] Complete the `nestest` trace beyond the first unofficial opcode and add
  dedicated illegal-opcode test ROM results.
- [ ] Audit instruction, interrupt, reset, and DMA timing at bus-cycle
  granularity as needed by timing ROMs and mapper write quirks.
- [ ] Implement accurate reset and power-cycle state separately; do not treat
  application startup, reset, and save-state restore as the same operation.

## P1 — APU correctness

- [ ] Validate `$4017` write parity/delay and frame-counter sequencing with APU
  test ROMs.
- [ ] Validate DMC fetch stalls, IRQ assertion/acknowledgement, address wrapping,
  looping, and DMA arbitration against test ROMs.
- [ ] Validate channel mixer levels, nonlinear mixing, filters, sample-rate
  conversion, and long-run clock drift against known references.
- [ ] Add PAL and Dendy APU timing tables when region support is introduced.
- [ ] Keep probe reporting for queue depth, drops, underflows, device reopens,
  and sample drift green during timing changes.

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
