pub mod apu;
pub mod audio;
pub mod bus;
pub mod cartridge;
pub mod cpu;
pub mod joypad;
pub mod mapper;
pub mod menu;
pub mod opcodes;
pub mod ppu;
pub mod probe;
pub mod region;
pub mod render;
pub mod test_rom;
pub mod trace;

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
use std::path::{Path, PathBuf};

use sdl3::event::Event;
use sdl3::keyboard::Keycode;
use sdl3::pixels::{Color, PixelFormat};
use sdl3::render::{FRect, ScaleMode};

const NES_WIDTH: u32 = 256;
const NES_HEIGHT: u32 = 240;

// How a gameplay session ended, so the caller can decide whether to return to
// the selection screen or close the program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GameOutcome {
    /// The user pressed Escape: hand control back to the menu.
    BackToMenu,
    /// The window was closed: shut the whole program down.
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowMode {
    Windowed,
    /// SDL fullscreen (borderless at desktop resolution; the compositor may
    /// still promote it to exclusive flip on Windows).
    Fullscreen,
    /// A plain borderless window covering the desktop: looks fullscreen but
    /// alt-tabs like any window and never owns the display mode.
    WindowedFullscreen,
}

// Largest 256x240 rectangle that fits the output, centered, so fullscreen
// keeps the NES aspect and fills the remainder with black bars instead of
// stretching. At the 3x windowed size this is exactly the full window.
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

// Runs a game ROM, presenting the PPU's frame to an SDL3 window. The frontend
// stops the CPU at each vblank boundary, samples input before the game's NMI
// handler, and advances audio in small wall-clock-paced chunks.
fn run_game(
    rom_path: &str,
    audio_config: audio::AudioConfig,
    latency_debug: bool,
    run_ahead_frames: u8,
    window_mode: WindowMode,
    region_override: Option<region::Region>,
) -> GameOutcome {
    let sdl_context = sdl3::init().unwrap();
    let video_subsystem = sdl_context.video().unwrap();
    let window_builder = match window_mode {
        WindowMode::WindowedFullscreen => {
            let bounds = video_subsystem
                .get_primary_display()
                .and_then(|display| display.get_bounds())
                .unwrap();
            let mut builder = video_subsystem.window("NES game", bounds.width(), bounds.height());
            builder.borderless().position(bounds.x(), bounds.y());
            builder
        }
        WindowMode::Windowed | WindowMode::Fullscreen => {
            let mut builder = video_subsystem.window("NES game", NES_WIDTH * 3, NES_HEIGHT * 3);
            builder.position_centered();
            if window_mode == WindowMode::Fullscreen {
                builder.fullscreen();
            }
            builder
        }
    };
    let window = window_builder.build().unwrap();
    let mut fullscreen = window_mode == WindowMode::Fullscreen;

    // No present_vsync: the emulator is paced by the NES frame timer below.
    // Pacing by vsync instead would tie the game speed to the display's
    // refresh rate, which never quite matches the NES's 60.0988 fps.
    let mut canvas = window.into_canvas();
    let mut event_pump = sdl_context.event_pump().unwrap();

    let creator = canvas.texture_creator();
    let mut texture = creator
        .create_texture_target(PixelFormat::RGB24, NES_WIDTH, NES_HEIGHT)
        .unwrap();
    // Nearest keeps pixels crisp at fullscreen's non-integer scales and is
    // identical to linear at the exact 3x windowed scale.
    texture.set_scale_mode(ScaleMode::Nearest);

    // All audio-device work happens on the pump's own thread (see
    // src/audio.rs); the game loop just pushes samples and reads the
    // backlog gauge, so a wedged sound server can never stall gameplay.
    let audio_pump = audio::AudioPump::start_with_config(audio_config.clone());
    let audio_chunk_pump = audio_pump.clone();
    let audio_chunk_samples = audio_config.delivery_samples;
    let sample_rate = audio::SAMPLE_RATE;

    let rom_path = resolve_rom_path(rom_path);
    let rom = Rom::from_file(&rom_path).unwrap_or_else(|err| panic!("{err}"));
    let region = region_override.unwrap_or_else(|| rom.metadata.timing.default_region());

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

    let sample_bytes = audio::HOST_SAMPLE_BYTES;

    // Frame pacing: this scanline model runs at 1/60.0984867 s per frame.
    // Audio-capable runs are paced on the exact 48 kHz sample timeline;
    // queue depth is diagnostic state, not a second clock controller.
    let frame_duration = std::time::Duration::from_secs_f64(1.0 / region.frames_per_second());
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
            "LATENCY audio_backend={} audio_profile={:?} rate={}Hz format=s16 latency_target={}ms pump_interval={}ms write_chunk={} samples delivery={} target={} samples high_water={} samples run_ahead={}",
            audio::selected_backend_name(),
            audio_config.profile,
            sample_rate,
            audio_config.pulse_latency_ms,
            audio::PUMP_INTERVAL_MS,
            audio_config.device_samples,
            audio_config.delivery_samples,
            audio_config.target_queued_samples(),
            audio_config.high_water_samples(),
            run_ahead_frames,
        );
    }

    // Host presentation is driven explicitly below so a snapshot can be
    // advanced speculatively without recursively entering an SDL callback.
    let bus = Bus::new_with_audio_region(
        rom,
        region,
        |_, _, _| {},
        audio_chunk_samples,
        move |samples| {
            let sample_count = samples.len();
            chunk_samples_produced
                .fetch_add(sample_count as u64, std::sync::atomic::Ordering::Relaxed);
            audio_chunk_pump.push(samples);
            chunk_audio_pacer
                .borrow_mut()
                .pace(sample_count, &audio_chunk_pump);
        },
    );

    let mut cpu = CPU::new(bus);
    cpu.bus.apu.set_sample_rate(sample_rate);
    cpu.power_on();
    cpu.run_until_frame_ready();
    let mut run_ahead_frame: Option<render::frame::Frame> = None;

    loop {
        let present_started = std::time::Instant::now();
        let frame = run_ahead_frame
            .as_ref()
            .unwrap_or_else(|| cpu.bus.ppu().frame());
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
        let present_us = present_started.elapsed().as_micros();

        // Forward the sub-chunk remainder at vblank. Most samples have already
        // reached the pump through Bus::new_with_audio.
        let samples = cpu.bus.apu.drain_samples();
        samples_produced.fetch_add(samples.len() as u64, std::sync::atomic::Ordering::Relaxed);
        let residual_samples = samples.len();
        audio_pump.push(samples);
        audio_pacer.borrow_mut().pace(residual_samples, &audio_pump);
        let backlog = audio_pump.backlog_bytes();
        frames += 1;

        if debug_audio && frames % 60 == 0 {
            let elapsed = run_start.elapsed().as_secs_f64();
            let produced = samples_produced.load(std::sync::atomic::Ordering::Relaxed);
            let backlog_label = if backlog == audio::BACKLOG_UNAVAILABLE {
                "unavailable".to_string()
            } else {
                format!(
                    "{}B/{:.1}ms",
                    backlog,
                    backlog as f64 / (sample_rate as f64 * sample_bytes as f64) * 1000.0
                )
            };
            let queued = audio_pump.queued_bytes();
            let queued_label = if queued == audio::BACKLOG_UNAVAILABLE {
                "unavailable".to_string()
            } else {
                format!("{queued}B")
            };
            eprintln!(
                "LATENCY t={:7.2}s frames={} fps={:.4} present={}us audio_total={} queued={} pending={}B target={}B device={} samples rate_adjust={:+}ppm input_to_poll={}us produced={} ({:.1}/s) reopens={} resumes={} backpressure={} dropped={} underflows={} lock_misses={}",
                elapsed,
                frames,
                frames as f64 / elapsed,
                present_us,
                backlog_label,
                queued_label,
                audio_pump.pending_bytes(),
                audio_pump.target_queued_bytes(),
                audio_pump
                    .stats
                    .device_samples
                    .load(std::sync::atomic::Ordering::Relaxed),
                audio_pump
                    .stats
                    .rate_adjust_ppm
                    .load(std::sync::atomic::Ordering::Relaxed),
                cpu.bus.joypad().last_input_to_poll_us().unwrap_or(0),
                produced,
                produced as f64 / elapsed,
                audio_pump
                    .stats
                    .reopens
                    .load(std::sync::atomic::Ordering::Relaxed),
                audio_pump
                    .stats
                    .device_resumes
                    .load(std::sync::atomic::Ordering::Relaxed),
                audio_pump
                    .stats
                    .backpressure_events
                    .load(std::sync::atomic::Ordering::Relaxed),
                audio_pump
                    .stats
                    .dropped_samples
                    .load(std::sync::atomic::Ordering::Relaxed),
                audio_pump
                    .stats
                    .underflow_samples
                    .load(std::sync::atomic::Ordering::Relaxed),
                audio_pump
                    .stats
                    .lock_miss_samples
                    .load(std::sync::atomic::Ordering::Relaxed),
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
                Event::Quit { .. } => {
                    if let Err(err) = cpu.bus.flush_battery_ram() {
                        eprintln!("warning: {err}");
                    }
                    return GameOutcome::Quit;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::Escape),
                    ..
                } => {
                    if let Err(err) = cpu.bus.flush_battery_ram() {
                        eprintln!("warning: {err}");
                    }
                    return GameOutcome::BackToMenu;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::F11),
                    repeat: false,
                    ..
                } => {
                    fullscreen = !fullscreen;
                    if let Err(error) = canvas.window_mut().set_fullscreen(fullscreen) {
                        eprintln!("fullscreen toggle failed: {error}");
                        fullscreen = !fullscreen;
                    }
                }
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
    cpu.power_on();
    cpu.program_counter = 0xC000;

    cpu.run_with_callback(move |cpu| {
        println!("{}", trace(cpu));
    });
}

// Renders a single CHR tile to an SDL3 window (the ch6.3 deliverable).
// Pass any iNES ROM that ships CHR ROM (e.g. pacman.nes); defaults to
// nestest.nes. snake.nes has 0 bytes of CHR and will panic.
fn run_tiles(rom_path: &str) {
    let sdl_context = sdl3::init().unwrap();
    let video_subsystem = sdl_context.video().unwrap();
    let window = video_subsystem
        .window("Tile viewer", (256.0 * 3.0) as u32, (240.0 * 3.0) as u32)
        .position_centered()
        .build()
        .unwrap();

    let mut canvas = window.into_canvas();
    let mut event_pump = sdl_context.event_pump().unwrap();
    canvas.set_scale(3.0, 3.0).unwrap();

    let creator = canvas.texture_creator();
    let mut texture = creator
        .create_texture_target(PixelFormat::RGB24, 256, 240)
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

fn parse_game_options(
    args: &[String],
) -> Result<
    (
        audio::AudioConfig,
        bool,
        u8,
        WindowMode,
        Option<region::Region>,
    ),
    String,
> {
    let mut profile = None;
    let mut pulse_latency_ms = None;
    let mut latency_debug = false;
    let mut window_mode = WindowMode::Windowed;
    let mut region_override = std::env::var("NES_REGION")
        .ok()
        .map(|value| region::Region::parse(&value))
        .transpose()?;
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
        } else if arg == "--fullscreen" {
            window_mode = WindowMode::Fullscreen;
        } else if arg == "--windowed-fullscreen" || arg == "--borderless" {
            window_mode = WindowMode::WindowedFullscreen;
        } else if let Some(value) = arg.strip_prefix("--region=") {
            region_override = Some(region::Region::parse(value)?);
        } else if arg == "--region" {
            i += 1;
            let value = args
                .get(i)
                .ok_or_else(|| "--region needs ntsc, pal, or dendy".to_string())?;
            region_override = Some(region::Region::parse(value)?);
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
    Ok((
        config,
        latency_debug,
        run_ahead_frames,
        window_mode,
        region_override,
    ))
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
            Ok((audio_config, latency_debug, run_ahead_frames, window_mode, region_override)) => {
                run_game(
                    rom_path,
                    audio_config,
                    latency_debug,
                    run_ahead_frames,
                    window_mode,
                    region_override,
                );
            }
            Err(err) => {
                eprintln!("game options: {err}");
                std::process::exit(2);
            }
        },
        // No ROM given: open the selection screen and loop back to it whenever a
        // game is exited with Escape, so the picker is the program's home base.
        None => match parse_game_options(&[]) {
            Ok((audio_config, latency_debug, run_ahead_frames, window_mode, region_override)) => {
                let roms_dir = Path::new("games");
                loop {
                    match menu::run_menu(roms_dir) {
                        menu::MenuChoice::Play(path) => {
                            let outcome = run_game(
                                &path.to_string_lossy(),
                                audio_config.clone(),
                                latency_debug,
                                run_ahead_frames,
                                window_mode,
                                region_override,
                            );
                            if outcome == GameOutcome::Quit {
                                break;
                            }
                        }
                        menu::MenuChoice::Quit => break,
                    }
                }
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
        let (config, debug, run_ahead, window_mode, _) = parse_game_options(&args).unwrap();
        assert_eq!(config.profile, audio::AudioProfile::LowLatency);
        assert_eq!(config.pulse_latency_ms, 25);
        assert!(debug);
        assert_eq!(run_ahead, 1);
        assert_eq!(window_mode, WindowMode::Windowed);
    }

    #[test]
    fn game_options_reject_more_than_one_run_ahead_frame() {
        let args = vec!["--run-ahead=2".to_string()];
        assert!(parse_game_options(&args).is_err());
    }

    #[test]
    fn game_options_accept_fullscreen() {
        let args = vec!["--fullscreen".to_string()];
        let (_, _, _, window_mode, _) = parse_game_options(&args).unwrap();
        assert_eq!(window_mode, WindowMode::Fullscreen);
    }

    #[test]
    fn game_options_accept_windowed_fullscreen_and_its_alias() {
        for flag in ["--windowed-fullscreen", "--borderless"] {
            let args = vec![flag.to_string()];
            let (_, _, _, window_mode, _) = parse_game_options(&args).unwrap();
            assert_eq!(window_mode, WindowMode::WindowedFullscreen);
        }
    }

    #[test]
    fn game_options_accept_region_override() {
        let args = vec!["--region=dendy".to_string()];
        let (_, _, _, _, region) = parse_game_options(&args).unwrap();
        assert_eq!(region, Some(region::Region::Dendy));
        assert!(parse_game_options(&["--region=secam".to_string()]).is_err());
    }

    #[test]
    fn letterbox_pillarboxes_a_16_by_9_display() {
        // 1920x1080: height limits the scale (4.5x), bars split the leftover
        // width evenly.
        let rect = letterbox_rect(1920, 1080);
        assert_eq!(rect.x, 384.0);
        assert_eq!(rect.y, 0.0);
        assert_eq!(rect.w, 1152.0);
        assert_eq!(rect.h, 1080.0);
    }

    #[test]
    fn letterbox_fills_the_exact_3x_window() {
        let rect = letterbox_rect(768, 720);
        assert_eq!(rect.x, 0.0);
        assert_eq!(rect.y, 0.0);
        assert_eq!(rect.w, 768.0);
        assert_eq!(rect.h, 720.0);
    }

    #[test]
    fn letterbox_bars_top_and_bottom_on_a_tall_display() {
        // Portrait 1080x1920: width limits the scale, bars go above/below.
        let rect = letterbox_rect(1080, 1920);
        assert_eq!(rect.x, 0.0);
        assert_eq!(rect.w, 1080.0);
        assert_eq!(rect.h, 1012.5);
        assert_eq!(rect.y, (1920.0 - 1012.5) / 2.0);
    }
}
