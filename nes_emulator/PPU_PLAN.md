# PPU Scanline Accuracy — Current State & Handoff

The emulator is scanline-accurate through Phase 3: it renders visible lines as
they complete, clocks MMC3's scanline IRQ, computes sprite-0 hits from actual
opaque-pixel overlap, and uses loopy `v/t/x` state for scrolling.

This is the companion to `MAPPER_PLAN.md`. It records the PPU half of the
MMC3 work and the remaining timing limitations. Source locations are named by
module rather than fragile line numbers.

## Why this was needed

The NES PPU produces the image one scanline at a time, top to bottom, and games
change PPU/mapper state *between* scanlines to do things a single snapshot can't
express:

- **MMC3 scanline IRQ** (SMB3, Kirby, Mega Man 3-6): the game arms an IRQ to
  fire partway down the screen and, in the handler, changes scroll and/or CHR
  banks so the top playfield and bottom status bar render differently.
- **Mid-frame scroll splits** (status bars generally): a game zeroes the scroll
  at a sprite-0 hit or IRQ so the HUD stays fixed while the level scrolls.
- **Mid-frame CHR / palette swaps**: animation and split backgrounds.

The former whole-frame-at-vblank renderer sampled each of these only once, so
the entire image reflected whichever PPU/mapper state happened to exist at
vblank. MMC1 largely hid that limitation because its state is normally stable
for a whole frame; MMC3 depends on changing state *within* one.

## Current architecture

- `NesPPU::tick()` advances the 341-cycle-per-line, 262-line frame. As each
  visible line completes (0-239), it composites that line into the PPU-owned
  `Frame`, computes sprite-0 hit for the line, and clocks `Mapper::on_scanline`
  when either background or sprite rendering is enabled. It raises vblank/NMI
  at scanline 241 and reports frame completion after the pre-render line.
- `render::render_scanline` is the production compositor. It samples the
  PPU's scroll, control, OAM, palette, nametable mirroring, and mapper-backed
  CHR at that line.
- The bus's NMI callback is presentation-only: `main.rs` and `probe.rs` read
  `ppu.frame()` rather than invoke a renderer.
- Scroll and PPUADDR share `LoopyRegister` (`ppu/registers/loopy.rs`): current
  address `v`, temporary address `t`, fine-X `x`, and the common `$2005/$2006`
  latch. `$2000/$2005/$2006` now feed the same state. At scanline boundaries
  the PPU increments vertical scroll and reloads horizontal bits; it reloads
  vertical bits at the frame boundary (the scanline-level analogue of the
  pre-render copy).
- The CPU's IRQ input is the OR of the APU and mapper level-triggered IRQ
  lines (`bus.rs`). MMC3 acknowledges/disables its IRQ on a write to `$E000`.

## Target: scanline accuracy (and what it is *not*)

Aim for **scanline-accurate**, not dot-accurate. Render each visible scanline
(0-239) as the PPU crosses it, sampling scroll / ctrl / CHR-bank / mirroring /
OAM as they stand *at that line*. This is the pragmatic bar: it's a fraction of
the work of a per-dot PPU and unlocks essentially every commercial game.

What the implemented scanline model buys:
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
  line. Loopy state gets the line right; the dot stays approximate.

## Implementation history and remaining work

### Phase 0 — carve out a per-scanline renderer (no behaviour change) — ✅ DONE
This phase originally extracted a per-line compositor from the old whole-frame
renderer. Its rectangle-based nametable stitching was deliberately replaced in
Phase 3 by direct loopy-address pixel sampling, because it could not represent
simultaneous horizontal and vertical wrapping correctly.

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

The former whole-frame renderer has no production callers. Phase 3 replaced its
static-scene golden tests with focused loopy-scroll and sprite-composition
tests; SMB1 (NROM), Pac-Man (NROM), and Zelda (MMC1) remain useful visual probe
targets.

After Phase 1, mid-frame CHR-bank and scroll changes that land *between*
scanlines already show up, with no mapper changes.

### Phase 2 — MMC3 IRQ (the payoff) — ✅ DONE
- `NesPPU::tick` calls `self.mapper.borrow_mut().on_scanline()` once per
  **visible** rendered line (`ppu/mod.rs`, right after `composite_scanline`),
  gated on `mask.show_background() || mask.show_sprites()`. This approximates
  the C timing (dot ~260) as "at the end of each visible line."
- **Mapper IRQ source**: the `Mapper` trait gained `fn irq_pending(&self) ->
  bool { false }` (`mapper/mod.rs`), OR'd into `bus.rs poll_irq_status`
  alongside `apu.irq_pending()`. Level-triggered like the APU line; the CPU
  already services it (`cpu.rs:599`), no CPU change.
- MMC3 implemented in `mapper/mmc3.rs`: 8 KB PRG / 1 KB CHR banking with the
  R0-R7 registers, PRG mode + CHR inversion, `$A000` mirroring, 8 KB PRG-RAM,
  and the IRQ latch/counter/reload/enable. `on_scanline` reloads-or-decrements
  and asserts `irq_line` at zero when enabled; a `$E000` write disables and
  acknowledges (drops the line). Cribbed from `NES/src/mappers/mmc3.c`.
- Tests: +11 MMC3 unit tests (banking, CHR inversion, IRQ timing/ack) and a
  PPU/MMC3 integration test proving blank lines do not clock the IRQ while
  rendered lines do. Sprite-0 overlap is covered by the Phase 3 compositor
  tests → **132 total, all green**.
  Verified: SMB2 (`smario2.nes`, MMC3) boots through title
  → player-select → level start → scrolling gameplay with correct per-screen
  CHR banking and no IRQ lockup; Zelda (MMC1) unaffected. Registered mapper `4`
  in `mapper::from_rom`.

### Phase 3 — loopy `v/t/x` scroll model — ✅ DONE
`LoopyRegister` replaces the separate `$2005` scroll and `$2006` address
models. It holds `v`, `t`, fine-X, and their shared write latch; `$2000`,
`$2005`, and `$2006` writes now follow the standard loopy routing, and a
PPUSTATUS read resets that one latch.

The scanline renderer samples `(v, fine_x)` directly. Rather than choosing one
of two rectangular nametable strips, it derives every background pixel's tile,
attribute, and logical nametable from loopy state. Horizontal and vertical
nametable wrapping therefore work together. The PPU advances fine/coarse Y and
reloads horizontal bits after visible lines, then reloads vertical bits at the
frame boundary. This follows the C implementation's ordering at scanline
granularity rather than per dot.

The same pass also corrected renderer behavior exposed by the C comparison:
PPUMASK background/sprite and left-edge clipping are honored, sprites use OAM Y
plus one, 8x16 sprites and the eight-sprite scanline limit are supported, and
sprite background priority is composited correctly. Unused OAM sprites at Y
`$FF` stay off screen instead of wrapping to the top row. Tests cover loopy
writes, fine-X and nametable wrapping, masking, sprite priority/position,
8x16 sprites, OAM-Y hiding, and the scanline limit; the suite remains **132
tests, all green**.

### Phase 2.5 — real sprite-0 hit (done alongside Phase 2)
The Phase 1/2 renderer sampled scroll per line correctly, but SMB1's status-bar
split still landed ~7 lines too early: the old `is_sprite_0_hit` fired at the
sprite's *top edge* (`scanline == OAM_y`, pixels ignored), so the game changed
scroll partway through the 32px status bar and its second row (score/time)
scrolled with the playfield.

Fixed by computing a **real** sprite-0 hit per scanline
(`NesPPU::sprite_zero_hit_on_scanline`, `ppu/mod.rs`): it walks sprite 0's
opaque pixels on the line and tests each against the background pixel actually
drawn there (`background_opaque_at` / `background_color_index` sample the
nametable through the current scroll, matching `render_scanline`'s selection via
the shared `scanline_nametables`). The hit is set right after that line
composites, so the CPU's `$2002` busy-poll reacts before the next lines render.
Honors the x=255 quirk and the left-8px background clip. Phase 3 moved this
check into the compositor so it shares the exact background/sprite pixels that
are drawn. Verified: SMB1 status bar now holds fixed over the scrolling
playfield; SMB2 (MMC3) and Zelda (MMC1) unaffected.

### Phase 4 — finer timing (optional, rarely needed)
Per-*dot* sprite-0 hit (exact cycle within the line) and any per-dot effect.
Phase 2.5 makes the hit land on the correct *scanline*; the dot within it is
still approximate. Skip unless a specific target game demands it; scanline
accuracy covers the commercial library.

## Testing

- `cargo test` is currently green with **131 tests**.
- **Scroll and composition tests** cover loopy writes, fine-X, nametable
  wrapping, PPUMASK clipping, sprite priority/position, 8x16 sprites, and the
  eight-sprite scanline limit. Keep these targeted cases when changing the
  renderer.
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
