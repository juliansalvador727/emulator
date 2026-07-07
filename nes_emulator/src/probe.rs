// TEMP: headless audio debug probe.
//
// Runs a ROM with scripted joypad input and logs, per frame, the RMS/peak of
// the APU's sample output plus each channel's length counter state, to
// stderr. Combined with NesAPU::trace_writes this gives a timeline of what
// the game wrote and when the audio died.
//
// Usage: cargo run --release -- probe <rom> "<button@from-to,...>" <frames>
// e.g.   cargo run --release -- probe games/mario.nes "start@120-135,right@350-" 2100

use crate::bus::Bus;
use crate::cartridge::Rom;
use crate::cpu::CPU;
use crate::joypad::JoypadButton;

struct Press {
    button: JoypadButton,
    from: u32,
    to: u32,
}

fn parse_script(script: &str) -> Vec<Press> {
    script
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|entry| {
            let (name, range) = entry.split_once('@').expect("press must be button@from-to");
            let button = match name {
                "a" => JoypadButton::BUTTON_A,
                "b" => JoypadButton::BUTTON_B,
                "start" => JoypadButton::START,
                "select" => JoypadButton::SELECT,
                "up" => JoypadButton::UP,
                "down" => JoypadButton::DOWN,
                "left" => JoypadButton::LEFT,
                "right" => JoypadButton::RIGHT,
                _ => panic!("unknown button {}", name),
            };
            let (from, to) = range.split_once('-').expect("range must be from-to");
            Press {
                button,
                from: from.parse().unwrap(),
                to: if to.is_empty() {
                    u32::MAX
                } else {
                    to.parse().unwrap()
                },
            }
        })
        .collect()
}

pub fn run_probe(rom_path: &str, script: &str, max_frames: u32) {
    let presses = parse_script(script);
    let bytes: Vec<u8> = std::fs::read(rom_path).unwrap();
    let rom = Rom::new(&bytes).unwrap();

    let mut frame_no: u32 = 0;
    let bus = Bus::new(rom, move |_ppu, apu, joypad| {
        frame_no += 1;
        for p in &presses {
            joypad.set_button_pressed_status(p.button, frame_no >= p.from && frame_no <= p.to);
        }

        let samples = apu.drain_samples();
        let rms = if samples.is_empty() {
            0.0
        } else {
            (samples.iter().map(|s| (s * s) as f64).sum::<f64>() / samples.len() as f64).sqrt()
        };
        let peak = samples.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        eprintln!(
            "FRAME {:05} n={} rms={:.4} peak={:.4} len(p1={} p2={} tr={} no={}) dmc_bytes={}",
            frame_no,
            samples.len(),
            rms,
            peak,
            apu.pulse1.length.active() as u8,
            apu.pulse2.length.active() as u8,
            apu.triangle.length.active() as u8,
            apu.noise.length.active() as u8,
            apu.dmc.bytes_remaining,
        );

        if frame_no >= max_frames {
            std::process::exit(0);
        }
    });

    let mut cpu = CPU::new(bus);
    cpu.bus.apu.set_sample_rate(44100);
    cpu.bus.apu.trace_writes = true;
    cpu.reset();
    cpu.run();
}
