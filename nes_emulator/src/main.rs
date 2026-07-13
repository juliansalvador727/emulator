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
pub mod test_rom;

#[macro_use]
extern crate lazy_static;

#[macro_use]
extern crate bitflags;

use bus::Bus;
use cartridge::Rom;
use cpu::CPU;
use joypad::JoypadButton;
use render::show_tile;
use trace::trace;

use std::collections::HashMap;
use std::mem;
use std::path::{Path, PathBuf};

use sdl2::event::Event;
use sdl2::keyboard::Keycode;
use sdl2::pixels::PixelFormatEnum;

// Runs a game ROM, presenting the PPU's frame to an SDL2 window. The frontend
// stops the CPU at each vblank boundary, samples input before the game's NMI
// handler, and advances audio in small wall-clock-paced chunks.
fn run_game(
    rom_path: &str,
    audio_config: audio::AudioConfig,
    latency_debug: bool,
    run_ahead_frames: u8,
) {
    // Keep PulseAudio's server-side buffer bounded. The profile selects 40 ms
    // natively and the established 80 ms WSLg-safe fallback unless overridden.
    if std::env::var_os("PULSE_LATENCY_MSEC").is_none() {
        // SAFETY: called before SDL init spawns any threads.
        unsafe {
            std::env::set_var(
                "PULSE_LATENCY_MSEC",
                audio_config.pulse_latency_ms.to_string(),
            )
        };
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
    let audio_pump = audio::AudioPump::start_with_config(audio_config.clone());
    let audio_chunk_pump = audio_pump.clone();
    let audio_chunk_samples = audio_config.delivery_samples;
    let sample_rate = audio::SAMPLE_RATE;

    let rom_path = resolve_rom_path(rom_path);
    let bytes: Vec<u8> = std::fs::read(&rom_path)
        .unwrap_or_else(|err| panic!("failed to read ROM {}: {}", rom_path.display(), err));
    let rom = Rom::new(&bytes).unwrap();

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

    // Frame pacing: this scanline model runs at 1/60.0984867 s per frame.
    // When audio is available, a tiny backlog correction keeps long-run
    // emulation speed locked to the host DAC without touching the APU sample
    // clock, so pitch stays stable.
    let frame_duration = std::time::Duration::from_nanos(16_639_354);
    let mut next_frame = std::time::Instant::now();

    // NES_AUDIO_DEBUG=1: log the audio pipeline state once per second to
    // stderr, for chasing pacing/latency drift (the SDL queue depth is the
    // host-side audio latency).
    let debug_audio = latency_debug
        || std::env::var("NES_AUDIO_DEBUG").is_ok()
        || std::env::var("NES_LATENCY_DEBUG").is_ok();
    let run_start = std::time::Instant::now();
    let mut frames: u64 = 0;
    let samples_produced = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let chunk_samples_produced = samples_produced.clone();
    let audio_pacer = std::rc::Rc::new(std::cell::RefCell::new(audio::AudioPacer::new()));
    let chunk_audio_pacer = audio_pacer.clone();

    if debug_audio {
        eprintln!(
            "LATENCY audio_profile={:?} pulse={}ms device_request={} samples delivery={} target={} samples high_water={} samples run_ahead={}",
            audio_config.profile,
            audio_config.pulse_latency_ms,
            audio_config.device_samples,
            audio_config.delivery_samples,
            audio_config.target_queued_samples(),
            audio_config.high_water_samples(),
            run_ahead_frames,
        );
    }

    // Host presentation is driven explicitly below so a snapshot can be
    // advanced speculatively without recursively entering an SDL callback.
    let bus = Bus::new_with_audio(
        rom,
        |_, _, _| {},
        audio_chunk_samples,
        move |samples| {
            let sample_count = samples.len();
            chunk_samples_produced.fetch_add(
                sample_count as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
            audio_chunk_pump.push(samples);
            chunk_audio_pacer
                .borrow_mut()
                .pace(sample_count, &audio_chunk_pump);
        },
    );

    let mut cpu = CPU::new(bus);
    cpu.bus.apu.set_sample_rate(sample_rate);
    cpu.reset();
    cpu.run_until_frame_ready();
    let mut run_ahead_frame: Option<render::frame::Frame> = None;

    loop {
        let present_started = std::time::Instant::now();
        let frame = run_ahead_frame
            .as_ref()
            .unwrap_or_else(|| cpu.bus.ppu().frame());
        texture.update(None, &frame.data, 256 * 3).unwrap();
        canvas.copy(&texture, None, None).unwrap();
        canvas.present();
        let present_us = present_started.elapsed().as_micros();

        // Forward the sub-chunk remainder at vblank. Most samples have already
        // reached the pump through Bus::new_with_audio.
        let samples = cpu.bus.apu.drain_samples();
        samples_produced.fetch_add(
            samples.len() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        let residual_samples = samples.len();
        audio_pump.push(samples);
        audio_pacer
            .borrow_mut()
            .pace(residual_samples, &audio_pump);
        let backlog = audio_pump.backlog_bytes();
        frames += 1;

        if debug_audio && frames % 60 == 0 {
            let elapsed = run_start.elapsed().as_secs_f64();
            let produced = samples_produced.load(std::sync::atomic::Ordering::Relaxed);
            eprintln!(
                "LATENCY t={:7.2}s frames={} fps={:.4} present={}us audio_total={}B ({:.1}ms) queued={}B pending={}B target={}B device={} samples input_to_poll={}us produced={} ({:.1}/s) reopens={} dropped={} underflows={} lock_misses={}",
                elapsed,
                frames,
                frames as f64 / elapsed,
                present_us,
                backlog,
                backlog as f64 / (sample_rate as f64 * sample_bytes as f64) * 1000.0,
                audio_pump.queued_bytes(),
                audio_pump.pending_bytes(),
                audio_pump.pace_target_queued_bytes(),
                audio_pump.stats.device_samples.load(std::sync::atomic::Ordering::Relaxed),
                cpu.bus.joypad().last_input_to_poll_us().unwrap_or(0),
                produced,
                produced as f64 / elapsed,
                audio_pump.stats.reopens.load(std::sync::atomic::Ordering::Relaxed),
                audio_pump.stats.dropped_samples.load(std::sync::atomic::Ordering::Relaxed),
                audio_pump.stats.underflow_samples.load(std::sync::atomic::Ordering::Relaxed),
                audio_pump.stats.lock_miss_samples.load(std::sync::atomic::Ordering::Relaxed),
            );
        }

        if backlog == audio::BACKLOG_UNAVAILABLE {
            next_frame += frame_duration;
            let now = std::time::Instant::now();
            if now < next_frame {
                std::thread::sleep(next_frame - now);
            } else {
                next_frame = now;
            }
        } else {
            // Sub-frame audio delivery is the master clock once a device is
            // available. Avoid an additional whole-frame sleep here.
            next_frame = std::time::Instant::now();
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
                        cpu.bus
                            .joypad_mut()
                            .set_button_pressed_status(*button, true);
                    }
                }
                Event::KeyUp { keycode, .. } => {
                    if let Some(button) = keycode.and_then(|k| key_map.get(&k)) {
                        cpu.bus
                            .joypad_mut()
                            .set_button_pressed_status(*button, false);
                    }
                }
                _ => {}
            }
        }

        if run_ahead_frames == 1 {
            let snapshot = cpu.snapshot();
            cpu.bus.set_audio_delivery_enabled(false);
            cpu.run_until_frame_ready();
            cpu.run_until_frame_ready();
            run_ahead_frame = Some(cpu.bus.ppu().frame().clone());
            cpu.restore(snapshot);
        } else {
            run_ahead_frame = None;
        }

        // Advance the canonical machine exactly one frame. Speculative audio
        // was retained only in the discarded snapshot branch, so playback is
        // never duplicated.
        cpu.run_until_frame_ready();
    }
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

fn parse_game_options(args: &[String]) -> Result<(audio::AudioConfig, bool, u8), String> {
    let mut profile = None;
    let mut pulse_latency_ms = None;
    let mut latency_debug = false;
    let mut run_ahead_frames = std::env::var("NES_RUN_AHEAD_FRAMES")
        .ok()
        .map(|value| {
            value
                .parse::<u8>()
                .map_err(|_| "NES_RUN_AHEAD_FRAMES must be 0 or 1".to_string())
        })
        .transpose()?
        .unwrap_or(0);
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if let Some(value) = arg.strip_prefix("--audio-profile=") {
            profile = Some(audio::AudioProfile::parse(value)?);
        } else if arg == "--audio-profile" {
            i += 1;
            let value = args
                .get(i)
                .ok_or_else(|| "--audio-profile needs low or balanced".to_string())?;
            profile = Some(audio::AudioProfile::parse(value)?);
        } else if let Some(value) = arg.strip_prefix("--audio-latency-ms=") {
            pulse_latency_ms = Some(
                value
                    .parse::<u32>()
                    .map_err(|_| "--audio-latency-ms must be an integer".to_string())?,
            );
        } else if arg == "--audio-latency-ms" {
            i += 1;
            let value = args
                .get(i)
                .ok_or_else(|| "--audio-latency-ms needs a value".to_string())?;
            pulse_latency_ms = Some(
                value
                    .parse::<u32>()
                    .map_err(|_| "--audio-latency-ms must be an integer".to_string())?,
            );
        } else if arg == "--latency-debug" {
            latency_debug = true;
        } else if let Some(value) = arg.strip_prefix("--run-ahead=") {
            run_ahead_frames = value
                .parse::<u8>()
                .map_err(|_| "--run-ahead must be 0 or 1".to_string())?;
        } else if arg == "--run-ahead" {
            i += 1;
            let value = args
                .get(i)
                .ok_or_else(|| "--run-ahead needs 0 or 1".to_string())?;
            run_ahead_frames = value
                .parse::<u8>()
                .map_err(|_| "--run-ahead must be 0 or 1".to_string())?;
        } else {
            return Err(format!("unknown game option {arg}"));
        }
        i += 1;
    }

    let mut config = audio::AudioConfig::from_env()?;
    if let Some(profile) = profile {
        config = audio::AudioConfig::for_profile(profile);
    }
    if let Some(latency_ms) = pulse_latency_ms {
        if latency_ms == 0 {
            return Err("--audio-latency-ms must be greater than zero".into());
        }
        config.pulse_latency_ms = latency_ms;
    }
    if run_ahead_frames > 1 {
        return Err("run-ahead currently supports only 0 or 1 frame".into());
    }
    Ok((config, latency_debug, run_ahead_frames))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("nestest") => run_nestest(),
        Some("probe") => {
            if let Err(err) = probe::run_probe(
                args.get(2).expect("probe needs a ROM path"),
                args.get(3).map(|s| s.as_str()).unwrap_or(""),
                args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1800),
            ) {
                eprintln!("probe failed: {err}");
                std::process::exit(1);
            }
        }
        Some("test-rom") => {
            if let Err(err) = test_rom::run(
                args.get(2).expect("test-rom needs a ROM path"),
                args.get(3)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(50_000_000),
            ) {
                eprintln!("test ROM failed: {err}");
                std::process::exit(1);
            }
        }
        Some("tiles") => run_tiles(args.get(2).map(|s| s.as_str()).unwrap_or("nestest.nes")),
        Some(rom_path) => match parse_game_options(&args[2..]) {
            Ok((audio_config, latency_debug, run_ahead_frames)) => {
                run_game(rom_path, audio_config, latency_debug, run_ahead_frames)
            }
            Err(err) => {
                eprintln!("game options: {err}");
                std::process::exit(2);
            }
        },
        None => match parse_game_options(&[]) {
            Ok((audio_config, latency_debug, run_ahead_frames)) => {
                run_game(
                    "games/pacman.nes",
                    audio_config,
                    latency_debug,
                    run_ahead_frames,
                )
            }
            Err(err) => {
                eprintln!("audio configuration: {err}");
                std::process::exit(2);
            }
        },
    }
}

#[cfg(test)]
mod frontend_tests {
    use super::*;

    #[test]
    fn game_options_select_low_latency_and_one_frame_run_ahead() {
        let args = vec![
            "--audio-profile".to_string(),
            "low".to_string(),
            "--audio-latency-ms=25".to_string(),
            "--run-ahead".to_string(),
            "1".to_string(),
            "--latency-debug".to_string(),
        ];
        let (config, debug, run_ahead) = parse_game_options(&args).unwrap();
        assert_eq!(config.profile, audio::AudioProfile::LowLatency);
        assert_eq!(config.pulse_latency_ms, 25);
        assert!(debug);
        assert_eq!(run_ahead, 1);
    }

    #[test]
    fn game_options_reject_more_than_one_run_ahead_frame() {
        let args = vec!["--run-ahead=2".to_string()];
        assert!(parse_game_options(&args).is_err());
    }
}
