pub struct Frame {
    pub data: Vec<u8>,
}

impl Frame {
    const WIDTH: usize = 256;
    const HEIGHT: usize = 240;

    pub fn new() -> Self {
        Frame {
            data: vec![0; (Frame::WIDTH) * (Frame::HEIGHT) * 3],
        }
    }

    pub fn set_pixel(&mut self, x: usize, y: usize, rgb: (u8, u8, u8)) {
        // Clip to the frame. Guarding only the flat buffer length lets an
        // out-of-row x (e.g. a sprite drawn past column 255) roll over into
        // the next scanline's left edge, which shows up as edge garbage.
        if x >= Frame::WIDTH || y >= Frame::HEIGHT {
            return;
        }
        let base = y * 3 * Frame::WIDTH + x * 3;
        self.data[base] = rgb.0;
        self.data[base + 1] = rgb.1;
        self.data[base + 2] = rgb.2;
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn set_pixel_past_row_end_does_not_wrap_to_next_line() {
        let mut frame = Frame::new();
        // x == WIDTH is off the right edge; must be a no-op, not bleed to (0, y+1).
        frame.set_pixel(Frame::WIDTH, 10, (1, 2, 3));
        let next_row_start = 11 * 3 * Frame::WIDTH; // pixel (0, 11)
        assert_eq!(
            (
                frame.data[next_row_start],
                frame.data[next_row_start + 1],
                frame.data[next_row_start + 2],
            ),
            (0, 0, 0)
        );
    }
}
