pub mod apu;
pub mod bus;
pub mod cartridge;
pub mod cpu;
pub mod joypad;
pub mod opcodes;
pub mod ppu;
pub mod probe;
pub mod render;
pub mod trace;

#[macro_use]
extern crate lazy_static;

#[macro_use]
extern crate bitflags;

use apu::NesAPU;
use bus::Bus;
use cartridge::Rom;
use cpu::CPU;
use joypad::Joypad;
use joypad::JoypadButton;
use ppu::NesPPU;
use render::frame::Frame;
use render::show_tile;
use trace::trace;

use std::collections::HashMap;

use sdl2::audio::{AudioQueue, AudioSpecDesired};
use sdl2::event::Event;
use sdl2::keyboard::Keycode;
use sdl2::pixels::PixelFormatEnum;

// Runs a game ROM, rendering the PPU background to an SDL2 window.
// The bus fires the callback once per frame (at vblank); we render the
// nametable into a Frame and blit it.
fn run_game(rom_path: &str) {
    let sdl_context = sdl2::init().unwrap();
    let video_subsystem = sdl_context.video().unwrap();
    let window = video_subsystem
        .window("NES game", (256.0 * 3.0) as u32, (240.0 * 3.0) as u32)
        .position_centered()
        .build()
        .unwrap();

    // No present_vsync: the emulator is paced by the audio queue below (or a
    // frame timer when there's no audio device). Pacing by vsync instead
    // would tie the game speed to the display's refresh rate, which never
    // quite matches the NES's 60.0988 fps - the mismatch slowly drains or
    // overfills the audio queue until the sound breaks up.
    let mut canvas = window.into_canvas().build().unwrap();
    let mut event_pump = sdl_context.event_pump().unwrap();
    canvas.set_scale(3.0, 3.0).unwrap();

    let creator = canvas.texture_creator();
    let mut texture = creator
        .create_texture_target(PixelFormatEnum::RGB24, 256, 240)
        .unwrap();

    // Mono f32 queue for the APU output. Some hosts (e.g. WSL without a
    // sound server) have no audio device; run silently in that case.
    let audio_device: Option<AudioQueue<f32>> = sdl_context.audio().ok().and_then(|audio| {
        let desired = AudioSpecDesired {
            freq: Some(44100),
            channels: Some(1),
            samples: Some(1024),
        };
        audio.open_queue(None, &desired).ok()
    });
    let sample_rate = audio_device
        .as_ref()
        .map(|device| device.spec().freq as u32)
        .unwrap_or(44100);
    if let Some(device) = &audio_device {
        device.resume();
    }

    let bytes: Vec<u8> = std::fs::read(rom_path).unwrap();
    let rom = Rom::new(&bytes).unwrap();

    let mut frame = Frame::new();

    // Keyboard -> NES controller button mapping.
    let mut key_map = HashMap::new();
    key_map.insert(Keycode::Down, JoypadButton::DOWN);
    key_map.insert(Keycode::Up, JoypadButton::UP);
    key_map.insert(Keycode::Right, JoypadButton::RIGHT);
    key_map.insert(Keycode::Left, JoypadButton::LEFT);
    key_map.insert(Keycode::Space, JoypadButton::SELECT);
    key_map.insert(Keycode::Return, JoypadButton::START);
    key_map.insert(Keycode::A, JoypadButton::BUTTON_A);
    key_map.insert(Keycode::S, JoypadButton::BUTTON_B);

    // Audio-clock pacing: keep ~67 ms of audio queued (in bytes; 4 per f32
    // sample). The DAC drains the queue at exactly the sample rate, so
    // sleeping until the backlog falls back to this target locks emulation
    // to real time and bounds audio latency, with no samples ever dropped.
    let target_queued_bytes = sample_rate / 15 * 4;
    // Fallback pacing when there is no audio device: one NTSC NES frame
    // (29780.5 CPU cycles) is 1/60.0988 s.
    let frame_duration = std::time::Duration::from_nanos(16_639_267);
    let mut next_frame = std::time::Instant::now();

    // Called by the bus at each vblank: draw the background, present it,
    // queue the frame's audio, pace the loop, and drain the SDL event queue
    // (updating the joypad) so the window stays responsive.
    let bus = Bus::new(rom, move |ppu: &NesPPU, apu: &mut NesAPU, joypad: &mut Joypad| {
        render::render(ppu, &mut frame);
        texture.update(None, &frame.data, 256 * 3).unwrap();
        canvas.copy(&texture, None, None).unwrap();
        canvas.present();

        let samples = apu.drain_samples();
        if let Some(device) = &audio_device {
            device.queue(&samples);
            // Sleep off the surplus. If the host sink stalls (size() stops
            // shrinking), bail out after ~250 ms and drop the backlog so the
            // game doesn't hang and latency can't accumulate.
            let mut waited_ms = 0;
            while device.size() > target_queued_bytes {
                std::thread::sleep(std::time::Duration::from_millis(1));
                waited_ms += 1;
                if waited_ms > 250 {
                    device.clear();
                    break;
                }
            }
        } else {
            next_frame += frame_duration;
            let now = std::time::Instant::now();
            if now < next_frame {
                std::thread::sleep(next_frame - now);
            } else {
                // Fell behind (e.g. the window was dragged); resync rather
                // than fast-forwarding to catch up.
                next_frame = now;
            }
        }

        for event in event_pump.poll_iter() {
            match event {
                Event::Quit { .. }
                | Event::KeyDown {
                    keycode: Some(Keycode::Escape),
                    ..
                } => std::process::exit(0),
                Event::KeyDown { keycode, .. } => {
                    if let Some(button) = keycode.and_then(|k| key_map.get(&k)) {
                        joypad.set_button_pressed_status(*button, true);
                    }
                }
                Event::KeyUp { keycode, .. } => {
                    if let Some(button) = keycode.and_then(|k| key_map.get(&k)) {
                        joypad.set_button_pressed_status(*button, false);
                    }
                }
                _ => { /* do nothing */ }
            }
        }
    });

    let mut cpu = CPU::new(bus);
    cpu.bus.apu.set_sample_rate(sample_rate);
    cpu.reset();
    cpu.run();
}

// Runs the nestest ROM in automation mode, printing a CPU trace per instruction.
// Redirect stdout to a file and diff against nestest.log to validate the CPU.
fn run_nestest() {
    let bytes: Vec<u8> = std::fs::read("nestest.nes").unwrap();
    let rom = Rom::new(&bytes).unwrap();

    let bus = Bus::new(rom, |_, _, _| {});
    let mut cpu = CPU::new(bus);
    cpu.reset();
    cpu.program_counter = 0xC000;

    cpu.run_with_callback(move |cpu| {
        println!("{}", trace(cpu));
    });
}

// Renders a single CHR tile to an SDL2 window (the ch6.3 deliverable).
// Pass any iNES ROM that ships CHR ROM (e.g. pacman.nes); defaults to
// nestest.nes. snake.nes has 0 bytes of CHR and will panic.
fn run_tiles(rom_path: &str) {
    let sdl_context = sdl2::init().unwrap();
    let video_subsystem = sdl_context.video().unwrap();
    let window = video_subsystem
        .window("Tile viewer", (256.0 * 3.0) as u32, (240.0 * 3.0) as u32)
        .position_centered()
        .build()
        .unwrap();

    let mut canvas = window.into_canvas().present_vsync().build().unwrap();
    let mut event_pump = sdl_context.event_pump().unwrap();
    canvas.set_scale(3.0, 3.0).unwrap();

    let creator = canvas.texture_creator();
    let mut texture = creator
        .create_texture_target(PixelFormatEnum::RGB24, 256, 240)
        .unwrap();

    let bytes: Vec<u8> = std::fs::read(rom_path).unwrap();
    let rom = Rom::new(&bytes).unwrap();

    let tile_frame = show_tile(&rom.chr_rom, 1, 0);

    texture.update(None, &tile_frame.data, 256 * 3).unwrap();
    canvas.copy(&texture, None, None).unwrap();
    canvas.present();

    loop {
        for event in event_pump.poll_iter() {
            match event {
                Event::Quit { .. }
                | Event::KeyDown {
                    keycode: Some(Keycode::Escape),
                    ..
                } => std::process::exit(0),
                _ => { /* do nothing */ }
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("nestest") => run_nestest(),
        Some("probe") => probe::run_probe(
            args.get(2).expect("probe needs a ROM path"),
            args.get(3).map(|s| s.as_str()).unwrap_or(""),
            args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1800),
        ),
        Some("tiles") => run_tiles(args.get(2).map(|s| s.as_str()).unwrap_or("nestest.nes")),
        Some(rom_path) => run_game(rom_path),
        None => run_game("games/pacman.nes"),
    }
}
