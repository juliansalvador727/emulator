pub mod frame;
pub mod palette;

use crate::ppu::NesPPU;
use frame::Frame;

// Renders the first nametable's background (32x30 tiles) into `frame`.
// Reads tile indices straight from VRAM ($2000 nametable) and decodes each
// 8x8 tile's two bitplanes. Palette is a fixed placeholder for now; the
// attribute-table lookup (bg_palette) is the next step.
pub fn render(ppu: &NesPPU, frame: &mut Frame) {
    let bank = ppu.ctrl.bknd_pattern_addr();

    for i in 0..0x03c0 {
        let tile = ppu.vram[i] as u16;
        let tile_x = i % 32;
        let tile_y = i / 32;
        let tile = &ppu.chr_rom[(bank + tile * 16) as usize..=(bank + tile * 16 + 15) as usize];

        let palette = bg_palette(ppu, tile_x, tile_y);
        for y in 0..=7 {
            let mut upper = tile[y];
            let mut lower = tile[y + 8];

            for x in (0..=7).rev() {
                let value = (1 & upper) << 1 | (1 & lower);
                upper = upper >> 1;
                lower = lower >> 1;
                let rgb = match value {
                    0 => palette::SYSTEM_PALLETE[ppu.palette_table[0] as usize],
                    1 => palette::SYSTEM_PALLETE[palette[1] as usize],
                    2 => palette::SYSTEM_PALLETE[palette[2] as usize],
                    3 => palette::SYSTEM_PALLETE[palette[3] as usize],
                    _ => panic!("impossible"),
                };
                frame.set_pixel(tile_x * 8 + x, tile_y * 8 + y, rgb);
            }
        }
    }
}

fn bg_palette(ppu: &NesPPU, tile_column: usize, tile_row: usize) -> [u8; 4] {
    let attr_table_idx = tile_row / 4 * 8 + tile_column / 4;
    let attr_byte = ppu.vram[0x3c0 + attr_table_idx];

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

#[cfg(test)]
mod test {
    use super::*;
    use crate::cartridge::Mirroring;

    #[test]
    fn render_draws_background_without_panicking() {
        // 2 CHR banks of a non-zero pattern; point a nametable entry at a tile.
        let mut ppu = NesPPU::new(vec![0x55; 0x2000], Mirroring::Horizontal);
        ppu.vram[0] = 1;
        ppu.vram[959] = 2;

        let mut frame = Frame::new();
        render(&ppu, &mut frame);

        assert_eq!(frame.data.len(), 256 * 240 * 3);
        assert!(frame.data.iter().any(|&b| b != 0), "render wrote no pixels");
    }

    #[test]
    fn bg_palette_selects_quadrant_from_attribute_byte() {
        let mut ppu = NesPPU::new(vec![0; 0x2000], Mirroring::Horizontal);
        // Distinct palette bytes so each palette is identifiable.
        for k in 0..32 {
            ppu.palette_table[k] = k as u8;
        }
        // Block (0,0): TL=0, TR=1, BL=2, BR=3  =>  0b11_10_01_00
        ppu.vram[0x3c0] = 0b11_10_01_00;

        // top-left quadrant (cols 0-1, rows 0-1) -> palette 0 -> start 1
        assert_eq!(bg_palette(&ppu, 0, 0), [0, 1, 2, 3]);
        assert_eq!(bg_palette(&ppu, 1, 1), [0, 1, 2, 3]);
        // top-right (cols 2-3, rows 0-1) -> palette 1 -> start 5
        assert_eq!(bg_palette(&ppu, 2, 0), [0, 5, 6, 7]);
        assert_eq!(bg_palette(&ppu, 3, 1), [0, 5, 6, 7]);
        // bottom-left (cols 0-1, rows 2-3) -> palette 2 -> start 9
        assert_eq!(bg_palette(&ppu, 0, 2), [0, 9, 10, 11]);
        // bottom-right (cols 2-3, rows 2-3) -> palette 3 -> start 13
        assert_eq!(bg_palette(&ppu, 3, 3), [0, 13, 14, 15]);
    }
}
