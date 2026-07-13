pub mod frame;
pub mod palette;

use crate::cartridge::Mirroring;
use crate::ppu::NesPPU;
use frame::Frame;

// Renders the first nametable's background (32x30 tiles) into `frame`.
// Reads tile indices straight from VRAM ($2000 nametable) and decodes each
// 8x8 tile's two bitplanes. Palette is a fixed placeholder for now; the
// attribute-table lookup (bg_palette) is the next step.
pub fn render(ppu: &NesPPU, frame: &mut Frame) {
    let scroll_x = (ppu.scroll.scroll_x) as usize;
    let scroll_y = (ppu.scroll.scroll_y) as usize;

    // Pick which physical nametable is "main" (top-left of the visible area)
    // and which one the scroll bleeds into. With vertical mirroring the two
    // logical nametables lie side by side, so horizontal scroll wraps into the
    // neighbour; with horizontal mirroring they're stacked, so vertical scroll
    // does. In both cases VRAM only holds two 1KB tables, and the second is a
    // mirror of the other pair.
    let mirroring = ppu.mirroring();
    let (main_nametable, second_nametable) = match (mirroring, ppu.ctrl.nametable_addr()) {
        (Mirroring::Vertical, 0x2000)
        | (Mirroring::Vertical, 0x2800)
        | (Mirroring::Horizontal, 0x2000)
        | (Mirroring::Horizontal, 0x2400) => (&ppu.vram[0..0x400], &ppu.vram[0x400..0x800]),
        (Mirroring::Vertical, 0x2400)
        | (Mirroring::Vertical, 0x2C00)
        | (Mirroring::Horizontal, 0x2800)
        | (Mirroring::Horizontal, 0x2C00) => (&ppu.vram[0x400..0x800], &ppu.vram[0..0x400]),
        // Single-screen: both logical nametables resolve to the same page.
        (Mirroring::SingleScreenLower, _) => (&ppu.vram[0..0x400], &ppu.vram[0..0x400]),
        (Mirroring::SingleScreenUpper, _) => (&ppu.vram[0x400..0x800], &ppu.vram[0x400..0x800]),
        (_, _) => {
            panic!("Not supported mirroring {:?}", mirroring);
        }
    };

    // Main nametable, shifted up/left by the scroll so the pixel at (scroll_x,
    // scroll_y) lands at the top-left of the screen.
    render_name_table(
        ppu,
        frame,
        main_nametable,
        Rect::new(scroll_x, scroll_y, 256, 240),
        -(scroll_x as isize),
        -(scroll_y as isize),
    );

    // The second nametable fills whatever the scroll exposed: a vertical strip
    // on the right for horizontal scroll, or a horizontal strip on the bottom
    // for vertical scroll.
    if scroll_x > 0 {
        render_name_table(
            ppu,
            frame,
            second_nametable,
            Rect::new(0, 0, scroll_x, 240),
            (256 - scroll_x) as isize,
            0,
        );
    } else if scroll_y > 0 {
        render_name_table(
            ppu,
            frame,
            second_nametable,
            Rect::new(0, 0, 256, scroll_y),
            0,
            (240 - scroll_y) as isize,
        );
    }

    // Draw sprites from OAM (64 entries of 4 bytes: y, tile, attr, x).
    // Iterate in reverse so lower-index sprites draw on top.
    for i in (0..ppu.oam_data.len()).step_by(4).rev() {
        let tile_idx = ppu.oam_data[i + 1] as u16;
        let tile_x = ppu.oam_data[i + 3] as usize;
        let tile_y = ppu.oam_data[i] as usize;

        let flip_vertical = ppu.oam_data[i + 2] >> 7 & 1 == 1;
        let flip_horizontal = ppu.oam_data[i + 2] >> 6 & 1 == 1;
        let palette_idx = ppu.oam_data[i + 2] & 0b11;
        let sprite_palette = sprite_palette(ppu, palette_idx);

        let bank: u16 = ppu.ctrl.sprt_pattern_addr();
        let tile = read_tile(ppu, bank + tile_idx * 16);

        for y in 0..=7 {
            let mut upper = tile[y];
            let mut lower = tile[y + 8];
            'next: for x in (0..=7).rev() {
                let value = (1 & lower) << 1 | (1 & upper);
                upper = upper >> 1;
                lower = lower >> 1;
                let rgb = match value {
                    0 => continue 'next, // color 0 is transparent for sprites
                    1 => palette::SYSTEM_PALLETE[sprite_palette[1] as usize],
                    2 => palette::SYSTEM_PALLETE[sprite_palette[2] as usize],
                    3 => palette::SYSTEM_PALLETE[sprite_palette[3] as usize],
                    _ => panic!("impossible"),
                };
                match (flip_horizontal, flip_vertical) {
                    (false, false) => frame.set_pixel(tile_x + x, tile_y + y, rgb),
                    (true, false) => frame.set_pixel(tile_x + 7 - x, tile_y + y, rgb),
                    (false, true) => frame.set_pixel(tile_x + x, tile_y + 7 - y, rgb),
                    (true, true) => frame.set_pixel(tile_x + 7 - x, tile_y + 7 - y, rgb),
                }
            }
        }
    }
}

// Fetch a single 8x8 tile's 16 CHR bytes (two bitplanes) through the mapper.
// The renderer only has a `&NesPPU`, so it reaches CHR via the shared mapper's
// interior mutability rather than a direct `chr_rom` slice.
fn read_tile(ppu: &NesPPU, base: u16) -> [u8; 16] {
    let mut tile = [0u8; 16];
    let mut mapper = ppu.mapper.borrow_mut();
    for (i, byte) in tile.iter_mut().enumerate() {
        *byte = mapper.ppu_read(base + i as u16);
    }
    tile
}

// Sprite palettes live at $3F11.. in palette_table (index 0 is transparent).
fn sprite_palette(ppu: &NesPPU, palette_idx: u8) -> [u8; 4] {
    let start = 0x11 + (palette_idx * 4) as usize;
    [
        0,
        ppu.palette_table[start],
        ppu.palette_table[start + 1],
        ppu.palette_table[start + 2],
    ]
}

fn bg_palette(
    ppu: &NesPPU,
    attribute_table: &[u8],
    tile_column: usize,
    tile_row: usize,
) -> [u8; 4] {
    let attr_table_idx = tile_row / 4 * 8 + tile_column / 4;
    let attr_byte = attribute_table[attr_table_idx];

    let palette_idx = match (tile_column % 4 / 2, tile_row % 4 / 2) {
        (0, 0) => attr_byte & 0b11,
        (1, 0) => (attr_byte >> 2) & 0b11,
        (0, 1) => (attr_byte >> 4) & 0b11,
        (1, 1) => (attr_byte >> 6) & 0b11,
        (_, _) => panic!("impossible"),
    };

    let palette_start: usize = 1 + (palette_idx as usize) * 4;
    [
        ppu.palette_table[0],
        ppu.palette_table[palette_start],
        ppu.palette_table[palette_start + 1],
        ppu.palette_table[palette_start + 2],
    ]
}

// ch6.3 tile viewer: decode a single CHR tile into a Frame.
pub fn show_tile(chr_rom: &Vec<u8>, bank: usize, tile_n: usize) -> Frame {
    assert!(bank <= 1);
    let mut frame = Frame::new();
    let bank = (bank * 0x1000) as usize;

    let tile = &chr_rom[(bank + tile_n * 16)..=(bank + tile_n * 16 + 15)];

    for y in 0..=7 {
        let mut upper = tile[y];
        let mut lower = tile[y + 8];

        for x in (0..=7).rev() {
            let value = (1 & upper) << 1 | (1 & lower);
            upper = upper >> 1;
            lower = lower >> 1;
            let rgb = match value {
                0 => palette::SYSTEM_PALLETE[0x01],
                1 => palette::SYSTEM_PALLETE[0x23],
                2 => palette::SYSTEM_PALLETE[0x27],
                3 => palette::SYSTEM_PALLETE[0x30],
                _ => panic!("can't be"),
            };
            frame.set_pixel(x, y, rgb)
        }
    }

    frame
}

struct Rect {
    x1: usize,
    y1: usize,
    x2: usize,
    y2: usize,
}

impl Rect {
    fn new(x1: usize, y1: usize, x2: usize, y2: usize) -> Self {
        Rect {
            x1: x1,
            y1: y1,
            x2: x2,
            y2: y2,
        }
    }
}

fn render_name_table(
    ppu: &NesPPU,
    frame: &mut Frame,
    name_table: &[u8],
    view_port: Rect,
    shift_x: isize,
    shift_y: isize,
) {
    let bank = ppu.ctrl.bknd_pattern_addr();

    let attribute_table = &name_table[0x3c0..0x400];

    for i in 0..0x3c0 {
        let tile_column = i % 32;
        let tile_row = i / 32;
        let tile_idx = name_table[i] as u16;
        let tile = read_tile(ppu, bank + tile_idx * 16);
        let palette = bg_palette(ppu, attribute_table, tile_column, tile_row);

        for y in 0..=7 {
            let mut upper = tile[y];
            let mut lower = tile[y + 8];

            for x in (0..=7).rev() {
                let value = (1 & lower) << 1 | (1 & upper);
                upper = upper >> 1;
                lower = lower >> 1;

                let rgb = match value {
                    0 => palette::SYSTEM_PALLETE[ppu.palette_table[0] as usize],
                    1 => palette::SYSTEM_PALLETE[palette[1] as usize],
                    2 => palette::SYSTEM_PALLETE[palette[2] as usize],
                    3 => palette::SYSTEM_PALLETE[palette[3] as usize],
                    _ => panic!("impossible"),
                };
                let pixel_x = tile_column * 8 + x;
                let pixel_y = tile_row * 8 + y;

                if pixel_x >= view_port.x1
                    && pixel_x < view_port.x2
                    && pixel_y >= view_port.y1
                    && pixel_y < view_port.y2
                {
                    frame.set_pixel(
                        (shift_x + pixel_x as isize) as usize,
                        (shift_y + pixel_y as isize) as usize,
                        rgb,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::cartridge::Mirroring;

    #[test]
    fn render_draws_background_without_panicking() {
        // 2 CHR banks of a non-zero pattern; point a nametable entry at a tile.
        let mut ppu = NesPPU::new(crate::mapper::test_nrom(vec![0x55; 0x2000], Mirroring::Horizontal));
        ppu.vram[0] = 1;
        ppu.vram[959] = 2;

        let mut frame = Frame::new();
        render(&ppu, &mut frame);

        assert_eq!(frame.data.len(), 256 * 240 * 3);
        assert!(frame.data.iter().any(|&b| b != 0), "render wrote no pixels");
    }

    #[test]
    fn render_draws_a_sprite_over_the_background() {
        // Tile 1 = fully solid (value 3 everywhere): both bitplanes all-ones.
        let mut chr = vec![0u8; 0x2000];
        for b in 16..32 {
            chr[b] = 0xFF;
        }
        let mut ppu = NesPPU::new(crate::mapper::test_nrom(chr, Mirroring::Horizontal));

        // Sprite palette 0, color 3 -> palette_table[0x13].
        ppu.palette_table[0x13] = 0x30;
        // OAM sprite 0: y=50, tile=1, attr=0 (palette 0, no flip), x=60.
        ppu.oam_data[0] = 50;
        ppu.oam_data[1] = 1;
        ppu.oam_data[2] = 0;
        ppu.oam_data[3] = 60;

        let mut frame = Frame::new();
        render(&ppu, &mut frame);

        // A pixel inside the 8x8 sprite should carry the sprite's color.
        let (px, py) = (63usize, 53usize);
        let base = py * 3 * 256 + px * 3;
        assert_eq!(
            (frame.data[base], frame.data[base + 1], frame.data[base + 2]),
            palette::SYSTEM_PALLETE[0x30]
        );
    }

    #[test]
    fn bg_palette_selects_quadrant_from_attribute_byte() {
        let mut ppu = NesPPU::new(crate::mapper::test_nrom(vec![0; 0x2000], Mirroring::Horizontal));
        // Distinct palette bytes so each palette is identifiable.
        for k in 0..32 {
            ppu.palette_table[k] = k as u8;
        }
        // Block (0,0): TL=0, TR=1, BL=2, BR=3  =>  0b11_10_01_00
        ppu.vram[0x3c0] = 0b11_10_01_00;
        let attr = ppu.vram[0x3c0..0x400].to_vec();

        // top-left quadrant (cols 0-1, rows 0-1) -> palette 0 -> start 1
        assert_eq!(bg_palette(&ppu, &attr, 0, 0), [0, 1, 2, 3]);
        assert_eq!(bg_palette(&ppu, &attr, 1, 1), [0, 1, 2, 3]);
        // top-right (cols 2-3, rows 0-1) -> palette 1 -> start 5
        assert_eq!(bg_palette(&ppu, &attr, 2, 0), [0, 5, 6, 7]);
        assert_eq!(bg_palette(&ppu, &attr, 3, 1), [0, 5, 6, 7]);
        // bottom-left (cols 0-1, rows 2-3) -> palette 2 -> start 9
        assert_eq!(bg_palette(&ppu, &attr, 0, 2), [0, 9, 10, 11]);
        // bottom-right (cols 2-3, rows 2-3) -> palette 3 -> start 13
        assert_eq!(bg_palette(&ppu, &attr, 3, 3), [0, 13, 14, 15]);
    }
}
