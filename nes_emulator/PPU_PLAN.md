# Making the PPU Scanline-Accurate — Handoff

Context handoff for a future session. This describes how to move the Rust NES
emulator (`nes_emulator/`, following bugzmanov/nes_ebook) from its current
**whole-frame-at-vblank** renderer to a **scanline-accurate** one, which is the
prerequisite for correct MMC3 (mapper #4) and for any mid-frame raster effect.
Everything below is grounded in the current code with `file:line` references.
Read it top to bottom before touching anything.

This is the companion to `MAPPER_PLAN.md`. That doc stops at MMC3 and points
here: MMC3-the-mapper is an afternoon, but MMC3-that-looks-right is gated on the
PPU rework described below.

## Why this is needed

The NES PPU produces the image one scanline at a time, top to bottom, and games
change PPU/mapper state *between* scanlines to do things a single snapshot can't
express:

- **MMC3 scanline IRQ** (SMB3, Kirby, Mega Man 3-6): the game arms an IRQ to
  fire partway down the screen and, in the handler, changes scroll and/or CHR
  banks so the top playfield and bottom status bar render differently.
- **Mid-frame scroll splits** (status bars generally): a game zeroes the scroll
  at a sprite-0 hit or IRQ so the HUD stays fixed while the level scrolls.
- **Mid-frame CHR / palette swaps**: animation and split backgrounds.

Our renderer samples every one of these **once**, at vblank, so none of them can
appear. MMC1 was fine because its runtime changes (mirroring, banks) are set
*before* a frame and stay stable *through* it — a vblank snapshot captures them
(which is exactly why Zelda just worked). MMC3's whole point is changing state
*within* a frame.

## Current architecture (what has to change)

Rendering is a single pass fired once per frame:

- `NesPPU::tick()` (`ppu/mod.rs:55`) advances `cycles`/`scanline` over the 341-
  cycle-per-line, 262-line frame. It sets vblank + the NMI at scanline 241
  (`ppu/mod.rs:65`), approximates sprite-0 hit (`ppu/mod.rs:84`
  `is_sprite_0_hit`), and returns `true` at frame end. **It does no rendering.**
- The bus fires `gameloop_callback` on the NMI edge (`bus.rs:65`), and
  `main.rs:120` calls `render::render(ppu, &mut frame)` **once**, then presents
  the texture, queues audio, and paces the loop (`main.rs:120-129`).
- `render::render` (`render/mod.rs:12`) reads the *current* PPU state for the
  whole 256x240 image:
  - scroll from `ppu.scroll.scroll_x/scroll_y` (`render/mod.rs:13`),
  - nametable + pattern-table selection from `ppu.ctrl` (`render/mod.rs:23`,
    `render/mod.rs:219`),
  - CHR tiles through the shared mapper via `read_tile` (`render/mod.rs:117`),
  - sprites from `ppu.oam_data` (`render/mod.rs:76`).
  Because it runs once, whatever these hold at vblank is what the entire frame
  gets.
- Scroll is modelled as a simplified `scroll_x/scroll_y` u8 pair
  (`ppu/registers/scroll.rs`) that is **separate** from the `$2006` address
  (`ppu/registers/addr.rs`). Real hardware shares a single 15-bit internal
  address (`v`), a temp latch (`t`), and fine-x (`x`); `$2000/$2005/$2006`
  writes all funnel into `t`/`v`. Our split model is fine for a per-frame
  snapshot but is the wrong shape for precise mid-frame scroll splits (see
  Phase 3).
- Interrupt plumbing today: NMI from the PPU (`bus.rs:70` `poll_nmi_status`) and
  a level-triggered IRQ line from the APU only (`bus.rs:76` `poll_irq_status` →
  `apu.irq_pending()`). The CPU polls both each instruction (`cpu.rs:595-600`),
  IRQ masked by `INTERRUPT_DISABLE`. **There is no mapper IRQ source yet.**
- `on_scanline()` exists on the `Mapper` trait as a default no-op
  (`mapper/mod.rs:30`) with no caller — the hook waiting to be wired.

## Target: scanline accuracy (and what it is *not*)

Aim for **scanline-accurate**, not dot-accurate. Render each visible scanline
(0-239) as the PPU crosses it, sampling scroll / ctrl / CHR-bank / mirroring /
OAM as they stand *at that line*. This is the pragmatic bar: it's a fraction of
the work of a per-dot PPU and unlocks essentially every commercial game.

What scanline accuracy buys:
- MMC3 IRQ can fire at the right line → status-bar splits hold still.
- Mid-frame scroll / CHR / palette / mirroring changes become visible.
- Sprite-0 hit gets a real per-line position instead of the current
  cycle-count guess.

What it still won't do (set expectations, don't chase these unless a target
game needs them):
- **Mid-scanline** raster tricks (color changes partway across a line, exact
  A12-edge IRQ timing to the dot). A12 counting is approximated as "once per
  visible line" — same compromise the C emulator makes (`NES/src/ppu.c:292`,
  commented `// TODO cycle-accurate A12 based IRQ` in `mmc3.c:66`).
- Split points that depend on the *exact* dot of a `$2005/$2006` write within a
  line. Phase 3 (loopy `v/t/x`) gets the line right; the dot stays approximate.

## The plan (phased; keep `cargo test` green at every step — 117 tests)

### Phase 0 — carve out a per-scanline renderer (no behaviour change) — ✅ DONE
`render::render` now loops `for line in 0..240 { render_scanline(ppu, frame,
line) }`, with the signature unchanged so `main.rs` and `probe.rs` are
untouched. `render_scanline` (`render/mod.rs`) composites one line: nametable
selection, the two background nametables via `render_name_table_line`, then
`render_sprites_line`, all writing only to row `line`. The two per-line helpers
compute the single source row that maps to `line` (background: `line -
shift_y`; sprites: skip those not covering the line, mirror the read for
v-flip) instead of scanning the whole table, and preserve the original reverse
sprite iteration so priority/overlap is identical. Tile/palette helpers
(`read_tile`, `bg_palette`, `sprite_palette`) are unchanged.

Safety net (keep forever): the original single-pass renderer is embedded
verbatim as a test-only `reference_render`, and three golden-frame tests
(`golden_frame_no_scroll/_horizontal_scroll/_vertical_scroll`, +3 → 117 total)
assert `render` is byte-identical to it on a busy NROM scene — varied
CHR/nametable/attribute/palette data, all four sprite-flip combinations, an
overlapping pair, corner/edge sprites, one clipped at the bottom — across both
mirrorings. Verified: SMB1 (NROM) and Zelda (MMC1) still render correctly via
the `PROBE_SHOTS` probe.

Note: the per-line background path re-reads each tile row up to 8× (once per
line). Harmless now; it disappears in Phase 1 when each scanline is rendered
exactly once as the PPU crosses it.

### Phase 1 — render from inside the frame timing, sampling per line — ✅ DONE
Rendering now lives in the scanline timeline instead of the vblank callback.

- The **PPU owns its `Frame`** (`ppu/mod.rs` field `frame: Option<Frame>`). In
  `tick()`, each time the PPU crosses a 341-cycle boundary it composites the
  line it just finished (visible lines 0-239) via `NesPPU::composite_scanline`,
  which calls the unchanged `render::render_scanline`. Per-line CHR-bank reads
  are automatically current through the shared mapper; scroll/ctrl/OAM are read
  from the PPU's own fields at that instant.
- The vblank callback is now **presentation only**: `main.rs` grabs
  `ppu.frame()` and blits it; `probe.rs` screenshots `ppu.frame()`. Neither
  calls a renderer.

Ownership decision (the flagged structural choice): the frame is a field, but
`render_scanline` stays a free `fn(&NesPPU, &mut Frame, usize)`. `composite_scanline`
`Option::take`s the frame out for the duration of the call — that hands `&self`
to the compositor while it writes the detached frame, then puts it back. `take`
swaps in `None`, so there's no per-line reallocation of the 180KB buffer, and no
weird `Default`. (The alternative — folding the whole compositor into a
`&mut self` method — would have meant rewriting every render helper as a method;
detaching via `Option` kept the render module untouched.)

Tick-granularity: the `self.cycles >= 341` boundary is now a **`while`** loop,
so if a single `tick(cycles*3)` carries the PPU past more than one line, each is
rendered (and, Phase 2, each will fire `on_scanline`). In practice one CPU
instruction is ≤ ~21 PPU cycles so at most one line finishes per call, but the
loop is correct regardless.

`render::render` (the whole-frame loop) lost its production callers and moved
into the render test module — its only remaining job is the golden comparison
against `reference_render`. The three golden-frame tests still pass (117 total),
and SMB1 (NROM), Pac-Man (NROM), and Zelda (MMC1) verified correct via the
`PROBE_SHOTS` probe after the move.

After Phase 1, mid-frame CHR-bank and scroll changes that land *between*
scanlines already show up, with no mapper changes.

### Phase 2 — MMC3 IRQ (the payoff)
- Call `self.mapper.borrow_mut().on_scanline()` once per **visible** rendered
  scanline, only when rendering is enabled (background or sprites on). Mirror
  the C timing (dot ~260, i.e. just after the visible portion of the line) —
  approximated here as "at the end of each visible line."
- Add a **mapper IRQ source**. Give the `Mapper` trait a way to expose a pending
  IRQ (e.g. `fn irq_pending(&self) -> bool { false }`), OR it into
  `bus.rs:76 poll_irq_status` alongside `apu.irq_pending()`. The CPU already
  services that line (`cpu.rs:599`); no CPU change needed beyond the OR.
- Implement MMC3 (`mapper/mmc3.rs`) with its IRQ latch/counter/enable, decrement
  in `on_scanline`, and assert the line when the counter hits zero and IRQs are
  enabled. Crib `NES/src/mappers/mmc3.c` (`load_MMC3`, `on_scanline` at
  `mmc3.c:65`, register writes at `mmc3.c:120+`). Banking is the easy part; the
  IRQ is why it waited for this rework.

### Phase 3 — loopy `v/t/x` scroll model (optional, for split correctness)
Only if mid-frame scroll splits look wrong after Phase 2. Replace the separate
`scroll_x/scroll_y` (`scroll.rs`) and `$2006` `AddrRegister` (`addr.rs`) with the
hardware `v` (current), `t` (temp), and `x` (fine-x) registers; route
`$2000/$2005/$2006` writes into `t`/`v` per the standard loopy rules and derive
the per-scanline scroll from `v`. This is a wide change — it touches every PPU
register write path and the ppu-register tests (`ppu/mod.rs` test module) — so
do it as its own phase with its tests updated deliberately.

### Phase 4 — finer timing (optional, rarely needed)
Per-dot sprite-0 hit and any per-dot effect. Skip unless a specific target game
demands it; scanline accuracy covers the commercial library.

## Testing

- `cargo test` stays green (117) at every phase.
- **Phase 0 golden-frame tests** are the key guard: the test-only
  `reference_render` (old single-pass) vs the new per-scanline loop must be
  byte-identical on an NROM scene. Kept forever; they catch regressions in every
  later phase.
- **Regression check the simple mappers**: NROM (SMB1 — heavy sprite-0),
  UxROM, CNROM, AxROM, and MMC1 (Zelda) must still render correctly after the
  timeline moves. Use the headless probe with `PROBE_SHOTS` (see `MAPPER_PLAN.md`
  Testing) — screenshots are the fastest eyeball.
- **Phase 2 payoff check**: drop an MMC3 game in `games/` (SMB3, Kirby) and
  confirm the status bar stays put while the level scrolls — that's the split
  the old renderer could never show. Expect slight jitter at the split line
  (the per-line A12 approximation), not a stable pixel-perfect edge.

## Reference: the C emulator next door

`../NES/` (obaraemmanuel/NES) renders per dot and is the model to crib timing
from — you're targeting a coarser scanline granularity, but the *ordering* is
the same:

- `NES/src/ppu.c` — the per-dot loop; `ppu.c:292` is where it calls
  `mapper->on_scanline()` mid-line (dot ~260), the timing this port approximates
  once per visible line.
- `NES/src/mappers/mmc3.c` — `on_scanline` (`mmc3.c:65`, counter/latch/enable)
  and the register decode, both of which translate almost directly once the hook
  fires.
