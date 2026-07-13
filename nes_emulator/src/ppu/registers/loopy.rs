// The PPU's shared scrolling/addressing state (usually called the "loopy"
// registers). $2000, $2005 and $2006 all feed the temporary address `t`; the
// renderer walks the current address `v` and uses `x` as the fine-X offset.
pub struct LoopyRegister {
    v: u16,
    t: u16,
    x: u8,
    write_latch: bool,
}

impl LoopyRegister {
    pub fn new() -> Self {
        Self {
            v: 0,
            t: 0,
            x: 0,
            write_latch: false,
        }
    }

    pub fn current(&self) -> u16 {
        self.v
    }

    pub fn fine_x(&self) -> u8 {
        self.x
    }

    pub fn write_ctrl(&mut self, value: u8) {
        self.t = (self.t & !0x0c00) | (((value as u16) & 0x03) << 10);
    }

    pub fn write_scroll(&mut self, value: u8) {
        if !self.write_latch {
            self.t = (self.t & !0x001f) | ((value as u16) >> 3);
            self.x = value & 0x07;
        } else {
            self.t = (self.t & !0x73e0)
                | (((value as u16) & 0x07) << 12)
                | (((value as u16) & 0xf8) << 2);
        }
        self.write_latch = !self.write_latch;
    }

    /// Apply one $2006 write. Returns true when the low-byte write completed
    /// the address and copied `t` to the PPU's current address bus value `v`.
    pub fn write_addr(&mut self, value: u8) -> bool {
        let completed = self.write_latch;
        if !self.write_latch {
            self.t = (self.t & 0x00ff) | (((value as u16) & 0x3f) << 8);
        } else {
            self.t = (self.t & 0x7f00) | value as u16;
            self.v = self.t;
        }
        self.write_latch = !self.write_latch;
        completed
    }

    pub fn increment(&mut self, amount: u8) {
        self.v = self.v.wrapping_add(amount as u16) & 0x3fff;
    }

    pub fn reset_latch(&mut self) {
        self.write_latch = false;
    }

    // End-of-scanline vertical increment, including the nametable toggle at
    // coarse Y 29. This is the same state transition as the C PPU's dot 257.
    pub fn increment_y(&mut self) {
        if self.v & 0x7000 != 0x7000 {
            self.v += 0x1000;
            return;
        }

        self.v &= !0x7000;
        let coarse_y = (self.v & 0x03e0) >> 5;
        let next = match coarse_y {
            29 => {
                self.v ^= 0x0800;
                0
            }
            31 => 0,
            y => y + 1,
        };
        self.v = (self.v & !0x03e0) | (next << 5);
    }

    // The fetch pipeline advances coarse X after every tile. Wrapping column
    // 31 also selects the horizontally adjacent logical nametable.
    pub fn increment_x(&mut self) {
        if self.v & 0x001f == 31 {
            self.v &= !0x001f;
            self.v ^= 0x0400;
        } else {
            self.v += 1;
        }
    }

    // Horizontal bits are reloaded from `t` at the end of every rendered line.
    pub fn copy_horizontal(&mut self) {
        self.v = (self.v & !0x041f) | (self.t & 0x041f);
    }

    // Vertical bits are reloaded from `t` during the pre-render line.
    pub fn copy_vertical(&mut self) {
        self.v = (self.v & !0x7be0) | (self.t & 0x7be0);
    }

    #[cfg(test)]
    pub fn copy_all_for_test(&mut self) {
        self.copy_horizontal();
        self.copy_vertical();
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn scroll_writes_fill_temp_address_and_fine_x() {
        let mut loopy = LoopyRegister::new();
        loopy.write_ctrl(0b0000_0011);
        loopy.write_scroll(0x1d); // coarse X 3, fine X 5
        loopy.write_scroll(0x2a); // coarse Y 5, fine Y 2
        loopy.copy_all_for_test();

        assert_eq!(loopy.fine_x(), 5);
        assert_eq!(loopy.current() & 0x001f, 3);
        assert_eq!((loopy.current() >> 5) & 0x001f, 5);
        assert_eq!((loopy.current() >> 10) & 0x03, 3);
        assert_eq!((loopy.current() >> 12) & 0x07, 2);
    }

    #[test]
    fn increment_y_wraps_coarse_y_and_switches_vertical_nametable() {
        let mut loopy = LoopyRegister::new();
        loopy.write_ctrl(0b0000_0010); // nametable 2
        loopy.write_scroll(0);
        loopy.write_scroll(0xef); // coarse Y 29, fine Y 7
        loopy.copy_all_for_test();
        loopy.increment_y();
        assert_eq!((loopy.current() >> 5) & 0x1f, 0);
        assert_eq!((loopy.current() >> 11) & 1, 0);
    }


    #[test]
    fn increment_x_wraps_and_switches_horizontal_nametable() {
        let mut loopy = LoopyRegister::new();
        loopy.write_scroll(31 * 8);
        loopy.write_scroll(0);
        loopy.copy_all_for_test();
        loopy.increment_x();
        assert_eq!(loopy.current() & 0x001f, 0);
        assert_eq!((loopy.current() >> 10) & 1, 1);
    }
}
