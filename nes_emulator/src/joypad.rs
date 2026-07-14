bitflags! {
    pub struct JoypadButton: u8 {
           const RIGHT             = 0b10000000;
           const LEFT              = 0b01000000;
           const DOWN              = 0b00100000;
           const UP                = 0b00010000;
           const START             = 0b00001000;
           const SELECT            = 0b00000100;
           const BUTTON_B          = 0b00000010;
           const BUTTON_A          = 0b00000001;

    }
}

#[derive(Clone)]
pub struct Joypad {
    strobe: bool,
    button_index: u8,
    button_status: JoypadButton,
    last_input_sampled_at: Option<std::time::Instant>,
    last_input_to_poll_us: Option<u64>,
}

impl Joypad {
    pub fn new() -> Self {
        Joypad {
            strobe: false,
            button_index: 0,
            button_status: JoypadButton::from_bits_truncate(0),
            last_input_sampled_at: None,
            last_input_to_poll_us: None,
        }
    }
    pub fn write(&mut self, data: u8) {
        self.strobe = data & 1 == 1;
        if self.strobe {
            self.button_index = 0
        }
    }

    pub fn read(&mut self) -> u8 {
        if !self.strobe && self.button_index == 0 {
            if let Some(sampled_at) = self.last_input_sampled_at.take() {
                self.last_input_to_poll_us =
                    Some(sampled_at.elapsed().as_micros().min(u128::from(u64::MAX)) as u64);
            }
        }
        if self.button_index > 7 {
            return 1;
        }
        let response = (self.button_status.bits & (1 << self.button_index)) >> self.button_index;
        if !self.strobe && self.button_index <= 7 {
            self.button_index += 1;
        }
        response
    }

    pub fn set_button_pressed_status(&mut self, button: JoypadButton, pressed: bool) {
        if self.button_status.contains(button) != pressed {
            self.last_input_sampled_at = Some(std::time::Instant::now());
        }
        self.button_status.set(button, pressed);
    }

    pub fn last_input_to_poll_us(&self) -> Option<u64> {
        self.last_input_to_poll_us
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_strobe_mode() {
        // With strobe on, reads keep returning the A button's state.
        let mut joypad = Joypad::new();
        joypad.write(1);
        joypad.set_button_pressed_status(JoypadButton::BUTTON_A, true);
        for _ in 0..10 {
            assert_eq!(joypad.read(), 1);
        }
    }

    #[test]
    fn test_strobe_off_reads_all_buttons_in_order() {
        // Strobe off: reads walk A,B,Select,Start,Up,Down,Left,Right, then 1s.
        let mut joypad = Joypad::new();
        joypad.write(0);
        joypad.set_button_pressed_status(JoypadButton::RIGHT, true);
        joypad.set_button_pressed_status(JoypadButton::LEFT, true);
        joypad.set_button_pressed_status(JoypadButton::SELECT, true);
        joypad.set_button_pressed_status(JoypadButton::BUTTON_B, true);

        // A, B, Select, Start, Up, Down, Left, Right
        assert_eq!(joypad.read(), 0); // A
        assert_eq!(joypad.read(), 1); // B
        assert_eq!(joypad.read(), 1); // Select
        assert_eq!(joypad.read(), 0); // Start
        assert_eq!(joypad.read(), 0); // Up
        assert_eq!(joypad.read(), 0); // Down
        assert_eq!(joypad.read(), 1); // Left
        assert_eq!(joypad.read(), 1); // Right

        // Reads past the 8th button return 1.
        for _ in 0..3 {
            assert_eq!(joypad.read(), 1);
        }
    }

    #[test]
    fn input_latency_is_recorded_only_for_the_first_following_poll() {
        let mut joypad = Joypad::new();
        joypad.set_button_pressed_status(JoypadButton::BUTTON_A, true);
        joypad.write(0);
        assert_eq!(joypad.read(), 1);
        let first = joypad.last_input_to_poll_us();

        joypad.write(1);
        joypad.write(0);
        assert_eq!(joypad.read(), 1);
        assert_eq!(joypad.last_input_to_poll_us(), first);
    }
}
