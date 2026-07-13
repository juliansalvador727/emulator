pub mod apu;
pub mod audio;
pub mod bus;
pub mod cartridge;
pub mod cpu;
pub mod joypad;
pub mod mapper;
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
use std::mem;
use std::path::{Path, PathBuf};

use sdl2::event::Event;
use sdl2::keyboard::Keycode;
use sdl2::pixels::PixelFormatEnum;

// Runs a game ROM, rendering the PPU background to an SDL2 window.
// The bus fires the callback once per frame (at vblank); we render the
// nametable into a Frame and blit it.
fn run_game(rom_path: &str) {
    // Keep PulseAudio's server-side buffer bounded. Very small values make
    // WSLg's sink stop calling SDL's callback after a short run; 80 ms is
    // below Pulse's default while leaving the sink breathing room.
    if std::env::var_os("PULSE_LATENCY_MSEC").is_none() {
        // SAFETY: called before SDL init spawns any threads.
        unsafe { std::env::set_var("PULSE_LATENCY_MSEC", "80") };
    }

    let sdl_context = sdl2::init().unwrap();
    let video_subsystem = sdl_context.video().unwrap();
    let window = video_subsystem
        .window("NES game", (256.0 * 3.0) as u32, (240.0 * 3.0) as u32)
        .position_centered()
        .build()
        .unwrap();

    // No present_vsync: the emulator is paced by the NES frame timer below.
    // Pacing by vsync instead would tie the game speed to the display's
    // refresh rate, which never quite matches the NES's 60.0988 fps.
    let mut canvas = window.into_canvas().build().unwrap();
    let mut event_pump = sdl_context.event_pump().unwrap();
    canvas.set_scale(3.0, 3.0).unwrap();

    let creator = canvas.texture_creator();
    let mut texture = creator
        .create_texture_target(PixelFormatEnum::RGB24, 256, 240)
        .unwrap();

    // All audio-device work happens on the pump's own thread (see
    // src/audio.rs); the game loop just pushes samples and reads the
    // backlog gauge, so a wedged sound server can never stall gameplay.
    let audio_pump = audio::AudioPump::start();
    let sample_rate = audio::SAMPLE_RATE;

    let rom_path = resolve_rom_path(rom_path);
    let bytes: Vec<u8> = std::fs::read(&rom_path)
        .unwrap_or_else(|err| panic!("failed to read ROM {}: {}", rom_path.display(), err));
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

    let sample_bytes = mem::size_of::<f32>() as u32;

    // Frame pacing: one NTSC NES frame (29780.5 CPU cycles) is 1/60.0988 s.
    // When audio is available, a tiny backlog correction keeps long-run
    // emulation speed locked to the host DAC without touching the APU sample
    // clock, so pitch stays stable.
    let frame_duration = std::time::Duration::from_nanos(16_639_267);
    let frame_duration_secs = frame_duration.as_secs_f64();
    let bytes_per_second = sample_rate as f64 * sample_bytes as f64;
    let mut next_frame = std::time::Instant::now();

    // NES_AUDIO_DEBUG=1: log the audio pipeline state once per second to
    // stderr, for chasing pacing/latency drift (the SDL queue depth is the
    // host-side audio latency).
    let debug_audio = std::env::var("NES_AUDIO_DEBUG").is_ok();
    let run_start = std::time::Instant::now();
    let mut frames: u64 = 0;
    let mut samples_produced: u64 = 0;

    // Called by the bus at each vblank: draw the background, present it,
    // queue the frame's audio, pace the loop, and drain the SDL event queue
    // (updating the joypad) so the window stays responsive.
    let bus = Bus::new(
        rom,
        move |ppu: &NesPPU, apu: &mut NesAPU, joypad: &mut Joypad| {
            render::render(ppu, &mut frame);
            texture.update(None, &frame.data, 256 * 3).unwrap();
            canvas.copy(&texture, None, None).unwrap();
            canvas.present();

            let samples = apu.drain_samples();
            frames += 1;
            samples_produced += samples.len() as u64;
            let backlog = audio_pump.backlog_bytes();
            audio_pump.push(samples);

            if debug_audio && frames % 60 == 0 {
                let elapsed = run_start.elapsed().as_secs_f64();
                eprintln!(
                    "AUDIO t={:7.2}s frames={} fps={:.4} backlog={}B ({:.1}ms) produced={} ({:.1}/s) reopens={} dropped={} underflows={} lock_misses={}",
                    elapsed,
                    frames,
                    frames as f64 / elapsed,
                    backlog,
                    backlog as f64 / (sample_rate as f64 * sample_bytes as f64) * 1000.0,
                    samples_produced,
                    samples_produced as f64 / elapsed,
                    audio_pump.stats.reopens.load(std::sync::atomic::Ordering::Relaxed),
                    audio_pump.stats.dropped_samples.load(std::sync::atomic::Ordering::Relaxed),
                    audio_pump.stats.underflow_samples.load(std::sync::atomic::Ordering::Relaxed),
                    audio_pump.stats.lock_miss_samples.load(std::sync::atomic::Ordering::Relaxed),
                );
            }

            let pace_duration = if backlog == audio::BACKLOG_UNAVAILABLE {
                frame_duration
            } else {
                let error_secs =
                    (backlog as f64 - audio::PACE_TARGET_QUEUED_BYTES as f64) / bytes_per_second;
                let correction_secs = (error_secs * 0.02).clamp(-0.00075, 0.00075);
                std::time::Duration::from_secs_f64(frame_duration_secs + correction_secs)
            };

            next_frame += pace_duration;
            let now = std::time::Instant::now();
            if now < next_frame {
                std::thread::sleep(next_frame - now);
            } else {
                // Fell behind (e.g. the window was dragged); resync rather than
                // fast-forwarding to catch up.
                next_frame = now;
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
        },
    );

    let mut cpu = CPU::new(bus);
    cpu.bus.apu.set_sample_rate(sample_rate);
    cpu.reset();
    cpu.run();
}

fn resolve_rom_path(rom_path: &str) -> PathBuf {
    let direct = Path::new(rom_path);
    if direct.exists() {
        return direct.to_path_buf();
    }

    let games_path = Path::new("games").join(rom_path);
    if games_path.exists() {
        return games_path;
    }

    direct.to_path_buf()
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
