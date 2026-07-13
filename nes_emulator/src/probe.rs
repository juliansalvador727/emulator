// Headless audio debug probe.
//
// Runs a ROM with scripted joypad input and logs to stderr, per frame, the
// RMS/peak of the APU's sample output and each channel's length counter
// state, interleaved with every APU register write (NesAPU::trace_writes) -
// a timeline of what the game wrote and what came out of the mixer.
//
// Usage: cargo run --release -- probe <rom> "<button@from-to,...>" <frames>
// e.g.   cargo run --release -- probe games/mario.nes "start@120-135,right@350-" 2100
//
// Set PROBE_SHOTS=<dir> (and optionally PROBE_SHOT_EVERY=<n>, default 50) to
// also dump a BMP screenshot every n frames, to see what the game was doing.

use crate::bus::Bus;
use crate::cartridge::Rom;
use crate::cpu::CPU;
use crate::joypad::JoypadButton;
use crate::render::frame::Frame;

// Minimal 24-bit BMP writer so screenshots need no external crates.
fn write_bmp(path: &str, frame: &Frame) {
    let (w, h) = (256usize, 240usize);
    let row = (w * 3 + 3) & !3;
    let size = 54 + row * h;
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(size as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    for y in (0..h).rev() {
        for x in 0..w {
            let i = (y * w + x) * 3;
            out.push(frame.data[i + 2]);
            out.push(frame.data[i + 1]);
            out.push(frame.data[i]);
        }
        out.resize(out.len() + (row - w * 3), 0);
    }
    std::fs::write(path, out).unwrap();
}

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

    let shot_dir = std::env::var("PROBE_SHOTS").ok();
    let shot_every: u32 = std::env::var("PROBE_SHOT_EVERY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    let mut frame_no: u32 = 0;
    let bus = Bus::new(rom, move |ppu, apu, joypad| {
        frame_no += 1;
        if let Some(dir) = &shot_dir {
            if frame_no % shot_every == 0 {
                write_bmp(&format!("{}/f{:05}.bmp", dir, frame_no), ppu.frame());
            }
        }
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
