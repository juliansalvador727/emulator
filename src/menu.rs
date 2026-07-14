// A game-selection screen shown at launch. It scans a directory for `.nes`
// ROMs and lets the user pick one with the arrow keys. The menu draws into the
// same 256x240 RGB `Frame` the emulator presents for gameplay, so it reuses the
// frontend's texture upload, letterboxing and fullscreen handling unchanged.

use std::path::{Path, PathBuf};

use sdl3::event::Event;
use sdl3::keyboard::Keycode;
use sdl3::pixels::{Color, PixelFormat};
use sdl3::render::{FRect, ScaleMode};

use crate::render::frame::Frame;

const NES_WIDTH: u32 = 256;
const NES_HEIGHT: u32 = 240;

// 5x7 bitmap font. Each glyph is seven rows; the low five bits of each row are
// the columns, most-significant bit leftmost. Covers the characters that show
// up in ROM filenames plus the symbols the menu draws.
mod font {
    pub const GLYPH_WIDTH: usize = 5;
    pub const GLYPH_HEIGHT: usize = 7;

    // Returns the seven-row bitmap for `c`, or `None` for characters with no
    // glyph (callers render those as blank space).
    pub fn glyph(c: char) -> Option<[u8; GLYPH_HEIGHT]> {
        let g = match c.to_ascii_uppercase() {
            ' ' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
            'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
            'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
            'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
            'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
            'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
            'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0E],
            'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
            'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
            'J' => [0x07, 0x02, 0x02, 0x02, 0x12, 0x12, 0x0C],
            'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
            'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
            'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
            'N' => [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11],
            'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
            'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
            'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
            'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
            'S' => [0x0E, 0x11, 0x10, 0x0E, 0x01, 0x11, 0x0E],
            'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
            'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
            'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
            'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x1B, 0x11],
            'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
            'Y' => [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04],
            'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
            '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
            '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
            '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
            '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
            '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
            '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
            '6' => [0x0E, 0x10, 0x10, 0x1E, 0x11, 0x11, 0x0E],
            '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
            '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
            '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x01, 0x0E],
            '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x06],
            '-' => [0x00, 0x00, 0x00, 0x0E, 0x00, 0x00, 0x00],
            '_' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1F],
            '>' => [0x10, 0x08, 0x04, 0x02, 0x04, 0x08, 0x10],
            ':' => [0x00, 0x06, 0x06, 0x00, 0x06, 0x06, 0x00],
            '/' => [0x01, 0x02, 0x02, 0x04, 0x08, 0x08, 0x10],
            '(' => [0x06, 0x08, 0x08, 0x08, 0x08, 0x08, 0x06],
            ')' => [0x0C, 0x02, 0x02, 0x02, 0x02, 0x02, 0x0C],
            '!' => [0x04, 0x04, 0x04, 0x04, 0x04, 0x00, 0x04],
            _ => return None,
        };
        Some(g)
    }
}

type Rgb = (u8, u8, u8);

const BACKGROUND: Rgb = (0x0B, 0x0C, 0x1A);
const TITLE_COLOR: Rgb = (0xF8, 0xC0, 0x30);
const ITEM_COLOR: Rgb = (0xC8, 0xC8, 0xD0);
const SELECTED_COLOR: Rgb = (0x30, 0x28, 0x18);
const SELECTED_TEXT: Rgb = (0xFF, 0xFF, 0xFF);
const CURSOR_COLOR: Rgb = (0x40, 0xC0, 0x60);
const HINT_COLOR: Rgb = (0x60, 0x64, 0x80);

// One extra column of spacing between glyphs.
const CHAR_ADVANCE: usize = font::GLYPH_WIDTH + 1;

fn fill(frame: &mut Frame, color: Rgb) {
    for y in 0..NES_HEIGHT as usize {
        for x in 0..NES_WIDTH as usize {
            frame.set_pixel(x, y, color);
        }
    }
}

fn fill_rect(frame: &mut Frame, x: usize, y: usize, w: usize, h: usize, color: Rgb) {
    for dy in 0..h {
        for dx in 0..w {
            frame.set_pixel(x + dx, y + dy, color);
        }
    }
}

// Draws `c` with its top-left at (x, y), each font pixel expanded to a
// `scale`x`scale` block.
fn draw_char(frame: &mut Frame, x: usize, y: usize, c: char, scale: usize, color: Rgb) {
    let Some(rows) = font::glyph(c) else { return };
    for (row_index, bits) in rows.iter().enumerate() {
        for col in 0..font::GLYPH_WIDTH {
            // Column 0 is the most-significant of the five used bits.
            if bits & (1 << (font::GLYPH_WIDTH - 1 - col)) != 0 {
                fill_rect(
                    frame,
                    x + col * scale,
                    y + row_index * scale,
                    scale,
                    scale,
                    color,
                );
            }
        }
    }
}

fn text_width(text: &str, scale: usize) -> usize {
    if text.is_empty() {
        return 0;
    }
    // n glyphs use (n-1) advances plus one glyph width, all scaled.
    (text.chars().count() * CHAR_ADVANCE - 1) * scale
}

fn draw_text(frame: &mut Frame, x: usize, y: usize, text: &str, scale: usize, color: Rgb) {
    let mut cursor_x = x;
    for c in text.chars() {
        draw_char(frame, cursor_x, y, c, scale, color);
        cursor_x += CHAR_ADVANCE * scale;
    }
}

fn draw_text_centered(frame: &mut Frame, y: usize, text: &str, scale: usize, color: Rgb) {
    let width = text_width(text, scale);
    let x = (NES_WIDTH as usize).saturating_sub(width) / 2;
    draw_text(frame, x, y, text, scale, color);
}

// Collects the `.nes` files in `dir`, sorted by lowercased filename so the list
// order is stable regardless of how the filesystem returns entries.
pub fn scan_roms(dir: &Path) -> Vec<PathBuf> {
    let mut roms: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("nes"))
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    roms.sort_by_key(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
    });
    roms
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("<unknown>")
        .to_string()
}

/// What the user chose on the selection screen.
pub enum MenuChoice {
    /// Launch this ROM.
    Play(PathBuf),
    /// Close the whole program (window closed or Escape pressed).
    Quit,
}

const TITLE_SCALE: usize = 3;
const ITEM_SCALE: usize = 2;
const LIST_TOP: usize = 64;
const ROW_HEIGHT: usize = 20;
const LIST_LEFT: usize = 28;
const BOTTOM_MARGIN: usize = 24;

// How many rows fit between the list top and the hint line at the bottom.
fn visible_rows() -> usize {
    let usable = (NES_HEIGHT as usize).saturating_sub(LIST_TOP + BOTTOM_MARGIN + ROW_HEIGHT);
    (usable / ROW_HEIGHT) + 1
}

fn render_menu(frame: &mut Frame, roms: &[PathBuf], selected: usize, scroll: usize) {
    fill(frame, BACKGROUND);
    draw_text_centered(frame, 18, "NES", TITLE_SCALE, TITLE_COLOR);

    if roms.is_empty() {
        draw_text_centered(frame, 110, "NO ROMS IN GAMES/", ITEM_SCALE, ITEM_COLOR);
        draw_text_centered(frame, 210, "ESC TO QUIT", 1, HINT_COLOR);
        return;
    }

    let per_page = visible_rows();
    let end = (scroll + per_page).min(roms.len());
    for (row, index) in (scroll..end).enumerate() {
        let y = LIST_TOP + row * ROW_HEIGHT;
        let label = file_label(&roms[index]);
        if index == selected {
            fill_rect(
                frame,
                LIST_LEFT - 6,
                y - 3,
                NES_WIDTH as usize - 2 * (LIST_LEFT - 6),
                ROW_HEIGHT - 2,
                SELECTED_COLOR,
            );
            draw_text(frame, LIST_LEFT - 4, y, ">", ITEM_SCALE, CURSOR_COLOR);
            draw_text(
                frame,
                LIST_LEFT + CHAR_ADVANCE * ITEM_SCALE,
                y,
                &label,
                ITEM_SCALE,
                SELECTED_TEXT,
            );
        } else {
            draw_text(
                frame,
                LIST_LEFT + CHAR_ADVANCE * ITEM_SCALE,
                y,
                &label,
                ITEM_SCALE,
                ITEM_COLOR,
            );
        }
    }

    // Scroll indicators when the list overflows the page.
    if scroll > 0 {
        draw_text_centered(frame, LIST_TOP - 12, "..", 1, HINT_COLOR);
    }
    if end < roms.len() {
        draw_text_centered(frame, LIST_TOP + per_page * ROW_HEIGHT - 4, "..", 1, HINT_COLOR);
    }

    draw_text_centered(
        frame,
        NES_HEIGHT as usize - 14,
        "UP/DOWN SELECT   ENTER PLAY   ESC QUIT",
        1,
        HINT_COLOR,
    );
}

// Keeps the selected row inside the visible page by nudging the scroll offset.
fn clamp_scroll(selected: usize, scroll: usize, len: usize) -> usize {
    let per_page = visible_rows();
    if len <= per_page {
        return 0;
    }
    let mut scroll = scroll.min(len - per_page);
    if selected < scroll {
        scroll = selected;
    } else if selected >= scroll + per_page {
        scroll = selected + 1 - per_page;
    }
    scroll
}

/// Runs the selection screen until the user picks a ROM or quits. Reuses the
/// gameplay presentation path: a 256x240 texture letterboxed into the window.
pub fn run_menu(roms_dir: &Path) -> MenuChoice {
    let sdl_context = sdl3::init().unwrap();
    let video_subsystem = sdl_context.video().unwrap();
    let window = video_subsystem
        .window("NES - select a game", NES_WIDTH * 3, NES_HEIGHT * 3)
        .position_centered()
        .build()
        .unwrap();

    let mut canvas = window.into_canvas();
    let mut event_pump = sdl_context.event_pump().unwrap();
    let creator = canvas.texture_creator();
    let mut texture = creator
        .create_texture_target(PixelFormat::RGB24, NES_WIDTH, NES_HEIGHT)
        .unwrap();
    texture.set_scale_mode(ScaleMode::Nearest);

    let roms = scan_roms(roms_dir);
    let mut selected: usize = 0;
    let mut scroll: usize = 0;
    let mut frame = Frame::new();
    let mut dirty = true;

    loop {
        if dirty {
            scroll = clamp_scroll(selected, scroll, roms.len());
            render_menu(&mut frame, &roms, selected, scroll);
            texture
                .update(None, &frame.data, NES_WIDTH as usize * 3)
                .unwrap();
            let (output_width, output_height) = canvas.output_size().unwrap();
            canvas.set_draw_color(Color::RGB(0, 0, 0));
            canvas.clear();
            canvas
                .copy(&texture, None, letterbox_rect(output_width, output_height))
                .unwrap();
            canvas.present();
            dirty = false;
        }

        // Blocking wait keeps the menu idle instead of spinning a busy loop.
        let event = event_pump.wait_event();
        match event {
            Event::Quit { .. }
            | Event::KeyDown {
                keycode: Some(Keycode::Escape),
                ..
            } => return MenuChoice::Quit,
            Event::KeyDown {
                keycode: Some(Keycode::Up),
                ..
            } if !roms.is_empty() => {
                selected = if selected == 0 {
                    roms.len() - 1
                } else {
                    selected - 1
                };
                dirty = true;
            }
            Event::KeyDown {
                keycode: Some(Keycode::Down),
                ..
            } if !roms.is_empty() => {
                selected = (selected + 1) % roms.len();
                dirty = true;
            }
            Event::KeyDown {
                keycode: Some(Keycode::Return | Keycode::KpEnter | Keycode::Space),
                ..
            } if !roms.is_empty() => {
                return MenuChoice::Play(roms[selected].clone());
            }
            // Any exposure/resize should trigger a redraw.
            Event::Window { .. } => dirty = true,
            _ => {}
        }
    }
}

// Same letterboxing math as the gameplay frontend: largest 256x240 rectangle
// that fits the output, centered.
fn letterbox_rect(output_width: u32, output_height: u32) -> FRect {
    let scale =
        (output_width as f32 / NES_WIDTH as f32).min(output_height as f32 / NES_HEIGHT as f32);
    let width = NES_WIDTH as f32 * scale;
    let height = NES_HEIGHT as f32 * scale;
    FRect::new(
        (output_width as f32 - width) / 2.0,
        (output_height as f32 - height) / 2.0,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_width_matches_glyph_layout() {
        // One glyph: just the glyph width, no trailing advance.
        assert_eq!(text_width("A", 1), font::GLYPH_WIDTH);
        // Three glyphs at scale 2: (3*6 - 1) * 2.
        assert_eq!(text_width("ABC", 2), (3 * CHAR_ADVANCE - 1) * 2);
        assert_eq!(text_width("", 3), 0);
    }

    #[test]
    fn scan_finds_only_nes_files_sorted() {
        let dir = std::env::temp_dir().join(format!("nes_menu_scan_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("zelda.nes"), b"z").unwrap();
        std::fs::write(dir.join("Mario.NES"), b"m").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        std::fs::create_dir_all(dir.join("sub.nes")).unwrap();

        let roms = scan_roms(&dir);
        let names: Vec<String> = roms.iter().map(|p| file_label(p)).collect();
        assert_eq!(names, vec!["Mario.NES".to_string(), "zelda.nes".to_string()]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn scroll_keeps_selection_visible() {
        let per_page = visible_rows();
        let len = per_page + 5;
        // Selecting past the first page scrolls down enough to show it.
        let scroll = clamp_scroll(per_page, 0, len);
        assert!(per_page >= scroll && per_page < scroll + per_page);
        // Selecting the last item never scrolls past the end.
        let scroll = clamp_scroll(len - 1, 0, len);
        assert_eq!(scroll, len - per_page);
    }

    #[test]
    fn every_supported_glyph_renders() {
        // Filenames and menu chrome only use these; make sure none are missing.
        for c in "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 .-_>:/()!".chars() {
            assert!(font::glyph(c).is_some(), "missing glyph for {c:?}");
        }
    }
}
