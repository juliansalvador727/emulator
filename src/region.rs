#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub enum Region {
    #[default]
    Ntsc,
    Pal,
    Dendy,
}

impl Region {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "ntsc" => Ok(Self::Ntsc),
            "pal" => Ok(Self::Pal),
            "dendy" => Ok(Self::Dendy),
            _ => Err(format!(
                "unknown region {value}; expected ntsc, pal, or dendy"
            )),
        }
    }

    pub const fn cpu_hz(self) -> u64 {
        match self {
            Self::Ntsc => 1_789_773,
            Self::Pal => 1_662_607,
            Self::Dendy => 1_773_448,
        }
    }

    pub const fn ppu_ratio(self) -> (u8, u8) {
        match self {
            Self::Ntsc | Self::Dendy => (3, 1),
            Self::Pal => (16, 5),
        }
    }

    pub const fn scanlines(self) -> u16 {
        match self {
            Self::Ntsc => 262,
            Self::Pal | Self::Dendy => 312,
        }
    }

    pub const fn vblank_start_scanline(self) -> u16 {
        match self {
            Self::Ntsc | Self::Pal => 241,
            Self::Dendy => 291,
        }
    }

    pub const fn has_odd_frame_skip(self) -> bool {
        matches!(self, Self::Ntsc)
    }

    pub fn frames_per_second(self) -> f64 {
        let (ppu_num, ppu_den) = self.ppu_ratio();
        let frame_dots = 341.0 * f64::from(self.scanlines())
            - if self.has_odd_frame_skip() { 0.5 } else { 0.0 };
        self.cpu_hz() as f64 * f64::from(ppu_num) / (f64::from(ppu_den) * frame_dots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_clock_shapes_match_console_families() {
        assert_eq!(Region::Ntsc.ppu_ratio(), (3, 1));
        assert_eq!(Region::Pal.ppu_ratio(), (16, 5));
        assert_eq!(Region::Dendy.ppu_ratio(), (3, 1));
        assert_eq!(Region::Ntsc.scanlines(), 262);
        assert_eq!(Region::Pal.scanlines(), 312);
        assert_eq!(Region::Dendy.vblank_start_scanline(), 291);
        assert!((Region::Pal.frames_per_second() - 50.007).abs() < 0.01);
    }
}
