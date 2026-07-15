//! Deterministic, headless ROM profiling and visual-regression probe.
//!
//! Basic usage remains:
//! `cargo run --release -- probe <rom> "<button@from-to,...>" <frames>`.
//! See `probes/README.md` for screenshot, baseline, report, realtime-audio,
//! and artifact-capture options.

use crate::audio::{self, AudioPump};
use crate::apu;
use crate::bus::Bus;
use crate::cartridge::Rom;
use crate::cpu::CPU;
use crate::joypad::JoypadButton;
use crate::ppu::ProbeDiagnostics;
use crate::render::frame::Frame;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

// Match this emulator's current scanline clock exactly: 1,789,773 CPU Hz,
// three PPU dots per CPU cycle, 341 dots * 262 lines per frame.
const NES_FPS: f64 = 1_789_773.0 * 3.0 / (341.0 * 262.0);

fn bmp_bytes(frame: &Frame) -> Vec<u8> {
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
    out
}

fn frame_hash(frame: &Frame) -> u64 {
    // FNV-1a is intentionally small and stable; this is a regression identity,
    // not a security checksum (ROM identities in probes/cases.txt use SHA-256).
    frame.data.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

#[derive(Clone, Copy)]
struct Press {
    button: JoypadButton,
    from: u32,
    to: u32,
}

fn parse_script(script: &str) -> Result<Vec<Press>, String> {
    script
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|entry| {
            let (name, range) = entry
                .split_once('@')
                .ok_or_else(|| format!("press must be button@from-to: {entry}"))?;
            let button = match name {
                "a" => JoypadButton::BUTTON_A,
                "b" => JoypadButton::BUTTON_B,
                "start" => JoypadButton::START,
                "select" => JoypadButton::SELECT,
                "up" => JoypadButton::UP,
                "down" => JoypadButton::DOWN,
                "left" => JoypadButton::LEFT,
                "right" => JoypadButton::RIGHT,
                _ => return Err(format!("unknown button {name}")),
            };
            let (from, to) = range
                .split_once('-')
                .ok_or_else(|| format!("range must be from-to: {range}"))?;
            Ok(Press {
                button,
                from: from.parse().map_err(|_| format!("bad frame {from}"))?,
                to: if to.is_empty() {
                    u32::MAX
                } else {
                    to.parse().map_err(|_| format!("bad frame {to}"))?
                },
            })
        })
        .collect()
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| value != "0" && value != "false")
}

fn env_frames(name: &str) -> Result<BTreeSet<u32>, String> {
    std::env::var(name)
        .unwrap_or_default()
        .split(',')
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<u32>()
                .map_err(|_| format!("{name} contains invalid frame {value}"))
        })
        .collect()
}

fn env_nonnegative_f64(name: &str) -> Result<Option<f64>, String> {
    parse_nonnegative_f64(name, std::env::var(name).ok())
}

fn parse_nonnegative_f64(name: &str, value: Option<String>) -> Result<Option<f64>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let parsed = value
        .parse::<f64>()
        .map_err(|_| format!("{name} must be a nonnegative number"))?;
    if !parsed.is_finite() || parsed < 0.0 {
        return Err(format!("{name} must be a nonnegative number"));
    }
    Ok(Some(parsed))
}

struct Config {
    shot_dir: Option<PathBuf>,
    shot_frames: BTreeSet<u32>,
    shot_every: Option<u32>,
    baselines: Option<PathBuf>,
    report: Option<PathBuf>,
    capture_frame: Option<u32>,
    capture_radius: u32,
    realtime: bool,
    verbose: bool,
    max_sample_drift: Option<f64>,
    require_healthy_audio: bool,
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let shot_dir = std::env::var_os("PROBE_SHOTS").map(PathBuf::from);
        let shot_frames = env_frames("PROBE_SHOT_FRAMES")?;
        let shot_every = std::env::var("PROBE_SHOT_EVERY")
            .ok()
            .map(|value| value.parse::<u32>())
            .transpose()
            .map_err(|_| "PROBE_SHOT_EVERY must be a positive integer".to_string())?
            .or_else(|| (shot_dir.is_some() && shot_frames.is_empty()).then_some(50));
        if shot_every == Some(0) {
            return Err("PROBE_SHOT_EVERY must be greater than zero".into());
        }
        Ok(Self {
            shot_dir,
            shot_frames,
            shot_every,
            baselines: std::env::var_os("PROBE_BASELINES").map(PathBuf::from),
            report: std::env::var_os("PROBE_REPORT").map(PathBuf::from),
            capture_frame: std::env::var("PROBE_CAPTURE_FRAME")
                .ok()
                .map(|value| value.parse::<u32>())
                .transpose()
                .map_err(|_| "PROBE_CAPTURE_FRAME must be an integer".to_string())?,
            capture_radius: std::env::var("PROBE_CAPTURE_RADIUS")
                .ok()
                .map(|value| value.parse::<u32>())
                .transpose()
                .map_err(|_| "PROBE_CAPTURE_RADIUS must be an integer".to_string())?
                .unwrap_or(2),
            realtime: env_flag("PROBE_REALTIME"),
            verbose: env_flag("PROBE_VERBOSE"),
            max_sample_drift: env_nonnegative_f64("PROBE_MAX_SAMPLE_DRIFT")?,
            require_healthy_audio: env_flag("PROBE_REQUIRE_HEALTHY_AUDIO"),
        })
    }
}

#[derive(Clone)]
struct FrameRow {
    frame: u32,
    host_ms: f64,
    samples: usize,
    cumulative_samples: u64,
    cpu_cycles: u64,
    hash: u64,
    audio_backlog: u32,
    audio_queued: u32,
    audio_pending: u32,
    oam_dmas: u64,
    visible_writes: u64,
    last_register: u16,
    last_scanline: u16,
    last_dot: usize,
}

struct State {
    rows: Vec<FrameRow>,
    previous_time: Instant,
    previous_diag: ProbeDiagnostics,
    ring: VecDeque<(u32, Frame)>,
    baseline_failures: Vec<String>,
    error: Option<String>,
}

fn write_shot(
    dir: &Path,
    baselines: Option<&Path>,
    frame_no: u32,
    frame: &Frame,
    failures: &mut Vec<String>,
) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|err| format!("create {}: {err}", dir.display()))?;
    let name = format!("f{frame_no:05}.bmp");
    let bytes = bmp_bytes(frame);
    std::fs::write(dir.join(&name), &bytes)
        .map_err(|err| format!("write {}: {err}", dir.join(&name).display()))?;
    if let Some(baseline_dir) = baselines {
        match std::fs::read(baseline_dir.join(&name)) {
            Ok(expected) if expected == bytes => {}
            Ok(_) => failures.push(format!("{name}: pixels differ")),
            Err(err) => failures.push(format!("{name}: baseline unavailable ({err})")),
        }
    }
    Ok(())
}

fn write_report(path: &Path, rows: &[FrameRow]) -> Result<(), String> {
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    let mut out = String::from(
        "frame,host_ms,samples,cumulative_samples,cpu_cycles,frame_hash,audio_backlog_bytes,audio_queued_bytes,audio_pending_bytes,oam_dmas,visible_ppu_writes,last_register,last_scanline,last_dot\n",
    );
    for row in rows {
        out.push_str(&format!(
            "{},{:.6},{},{},{},{:016x},{},{},{},{},{},0x{:04x},{},{}\n",
            row.frame,
            row.host_ms,
            row.samples,
            row.cumulative_samples,
            row.cpu_cycles,
            row.hash,
            row.audio_backlog,
            row.audio_queued,
            row.audio_pending,
            row.oam_dmas,
            row.visible_writes,
            row.last_register,
            row.last_scanline,
            row.last_dot,
        ));
    }
    std::fs::write(path, out).map_err(|err| format!("write {}: {err}", path.display()))
}

pub fn run_probe(rom_path: &str, script: &str, max_frames: u32) -> Result<(), String> {
    if max_frames == 0 {
        return Err("probe frame count must be greater than zero".into());
    }
    let presses = parse_script(script)?;
    let config = Rc::new(Config::from_env()?);
    if config.capture_frame.is_some() && config.shot_dir.is_none() {
        return Err("PROBE_CAPTURE_FRAME requires PROBE_SHOTS".into());
    }
    if config.baselines.is_some() && config.shot_dir.is_none() {
        return Err("PROBE_BASELINES requires PROBE_SHOTS".into());
    }

    let bytes = std::fs::read(rom_path).map_err(|err| format!("read {rom_path}: {err}"))?;
    let rom = Rom::new(&bytes)?;
    let done = Rc::new(Cell::new(false));
    let state = Rc::new(RefCell::new(State {
        rows: Vec::with_capacity(max_frames as usize),
        previous_time: Instant::now(),
        previous_diag: ProbeDiagnostics::default(),
        ring: VecDeque::new(),
        baseline_failures: Vec::new(),
        error: None,
    }));

    let audio_config = audio::AudioConfig::from_env()?;
    let audio_pump = config
        .realtime
        .then(|| AudioPump::start_with_config(audio_config.clone()));
    let audio_stats = audio_pump.as_ref().map(|pump| pump.stats.clone());
    let audio_chunk_pump = audio_pump.clone();
    let audio_pacer = Rc::new(RefCell::new(audio::AudioPacer::new()));
    let callback_audio_pacer = Rc::clone(&audio_pacer);
    let chunk_audio_pacer = Rc::clone(&audio_pacer);
    let audio_chunk_samples = if config.realtime {
        audio_config.delivery_samples
    } else {
        usize::MAX
    };
    let chunk_samples_since_frame = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let callback_chunk_samples = chunk_samples_since_frame.clone();
    let delivered_chunk_samples = chunk_samples_since_frame.clone();
    let callback_state = Rc::clone(&state);
    let callback_done = Rc::clone(&done);
    let callback_config = Rc::clone(&config);
    let started = Instant::now();
    let mut frame_no = 0u32;
    let mut cumulative_samples = 0u64;
    let mut next_frame = Instant::now();
    let frame_duration = Duration::from_secs_f64(1.0 / NES_FPS);

    let bus = Bus::new_with_audio(rom, move |ppu, apu, joypad| {
        frame_no += 1;
        for button in [
            JoypadButton::BUTTON_A,
            JoypadButton::BUTTON_B,
            JoypadButton::START,
            JoypadButton::SELECT,
            JoypadButton::UP,
            JoypadButton::DOWN,
            JoypadButton::LEFT,
            JoypadButton::RIGHT,
        ] {
            joypad.set_button_pressed_status(button, false);
        }
        for press in &presses {
            if frame_no >= press.from && frame_no <= press.to {
                joypad.set_button_pressed_status(press.button, true);
            }
        }

        let now = Instant::now();
        let samples = apu.drain_samples();
        let frame_samples = samples.len() as u64
            + callback_chunk_samples.swap(0, std::sync::atomic::Ordering::Relaxed);
        cumulative_samples += frame_samples;
        let diag = ppu.probe_diagnostics();
        let hash = frame_hash(ppu.frame());
        let backlog = audio_pump
            .as_ref()
            .map_or(audio::BACKLOG_UNAVAILABLE, AudioPump::backlog_bytes);
        let queued = audio_pump
            .as_ref()
            .map_or(audio::BACKLOG_UNAVAILABLE, AudioPump::queued_bytes);
        let pending = audio_pump.as_ref().map_or(0, AudioPump::pending_bytes);

        let mut state = callback_state.borrow_mut();
        let previous_diag = state.previous_diag;
        let host_ms = now.duration_since(state.previous_time).as_secs_f64() * 1000.0;
        state.rows.push(FrameRow {
            frame: frame_no,
            host_ms,
            samples: frame_samples as usize,
            cumulative_samples,
            cpu_cycles: apu.cpu_cycles(),
            hash,
            audio_backlog: backlog,
            audio_queued: queued,
            audio_pending: pending,
            oam_dmas: diag.oam_dma_count - previous_diag.oam_dma_count,
            visible_writes: diag.visible_register_writes - previous_diag.visible_register_writes,
            last_register: diag.last_register,
            last_scanline: diag.last_scanline,
            last_dot: diag.last_dot,
        });
        state.previous_time = now;
        state.previous_diag = diag;

        let scheduled = callback_config.shot_frames.contains(&frame_no)
            || callback_config
                .shot_every
                .is_some_and(|every| frame_no % every == 0);
        if scheduled {
            if let Some(dir) = &callback_config.shot_dir {
                let baselines = callback_config.baselines.as_deref();
                if let Err(err) = write_shot(
                    dir,
                    baselines,
                    frame_no,
                    ppu.frame(),
                    &mut state.baseline_failures,
                ) {
                    state.error = Some(err);
                }
            }
        }

        if let (Some(capture), Some(dir)) =
            (callback_config.capture_frame, &callback_config.shot_dir)
        {
            let radius = callback_config.capture_radius;
            if frame_no == capture {
                let prior: Vec<_> = state.ring.iter().cloned().collect();
                for (prior_no, prior_frame) in prior {
                    if let Err(err) = write_shot(
                        dir,
                        None,
                        prior_no,
                        &prior_frame,
                        &mut state.baseline_failures,
                    ) {
                        state.error = Some(err);
                    }
                }
            }
            if frame_no >= capture && frame_no <= capture.saturating_add(radius) {
                if let Err(err) = write_shot(
                    dir,
                    None,
                    frame_no,
                    ppu.frame(),
                    &mut state.baseline_failures,
                ) {
                    state.error = Some(err);
                }
            }
            state.ring.push_back((frame_no, ppu.frame().clone()));
            while state.ring.len() > radius as usize {
                state.ring.pop_front();
            }
        }

        if callback_config.verbose {
            let row = state.rows.last().unwrap();
            eprintln!(
                "FRAME {:05} host_ms={:.3} samples={} hash={:016x} backlog={} dma={} visible_writes={}",
                frame_no,
                row.host_ms,
                row.samples,
                row.hash,
                row.audio_backlog,
                row.oam_dmas,
                row.visible_writes,
            );
        }
        drop(state);

        if let Some(pump) = &audio_pump {
            let residual_samples = samples.len();
            pump.push(samples);
            callback_audio_pacer
                .borrow_mut()
                .pace(residual_samples, pump);
            if pump.backlog_bytes() == audio::BACKLOG_UNAVAILABLE {
                next_frame += frame_duration;
                let now = Instant::now();
                if now < next_frame {
                    std::thread::sleep(next_frame - now);
                } else {
                    next_frame = now;
                }
            } else {
                next_frame = Instant::now();
            }
        }
        if frame_no >= max_frames || callback_state.borrow().error.is_some() {
            callback_done.set(true);
        }
    }, audio_chunk_samples, move |samples| {
        delivered_chunk_samples.fetch_add(
            samples.len() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        if let Some(pump) = &audio_chunk_pump {
            let sample_count = samples.len();
            pump.push(samples);
            chunk_audio_pacer
                .borrow_mut()
                .pace(sample_count, pump);
        }
    });

    let mut cpu = CPU::new(bus);
    cpu.bus.apu.set_sample_rate(audio::SAMPLE_RATE);
    cpu.bus.apu.trace_writes = env_flag("PROBE_TRACE_WRITES");
    cpu.power_on();
    cpu.run_until(|_| done.get());

    let elapsed = started.elapsed().as_secs_f64();
    let state = state.borrow();
    if let Some(err) = &state.error {
        return Err(err.clone());
    }
    if let Some(path) = &config.report {
        write_report(path, &state.rows)?;
    }
    let mut frame_times: Vec<f64> = state.rows.iter().skip(1).map(|row| row.host_ms).collect();
    frame_times.sort_by(f64::total_cmp);
    let average_ms = frame_times.iter().sum::<f64>() / frame_times.len().max(1) as f64;
    let p95_ms = frame_times
        .get((frame_times.len().saturating_sub(1) * 95) / 100)
        .copied()
        .unwrap_or(0.0);
    let max_ms = frame_times.last().copied().unwrap_or(0.0);
    let actual_samples = state.rows.last().map_or(0, |row| row.cumulative_samples);
    // The first NMI callback may arrive several hardware frames after reset
    // while a game is booting. Treat it as warm-up so accumulated pre-NMI
    // audio does not look like queue drift in an otherwise stable run.
    let warmup_samples = state.rows.first().map_or(0, |row| row.samples as u64);
    let measured_cycles = state
        .rows
        .last()
        .zip(state.rows.first())
        .map_or(0, |(last, first)| last.cpu_cycles - first.cpu_cycles);
    let expected_samples = measured_cycles as f64 * audio::SAMPLE_RATE as f64
        / apu::CPU_HZ as f64;
    let sample_drift = actual_samples.saturating_sub(warmup_samples) as f64 - expected_samples;
    let queue_depths: Vec<u32> = state
        .rows
        .iter()
        .map(|row| row.audio_backlog)
        .filter(|depth| *depth != audio::BACKLOG_UNAVAILABLE)
        .collect();
    let queue_min = queue_depths
        .iter()
        .copied()
        .min()
        .unwrap_or(audio::BACKLOG_UNAVAILABLE);
    let queue_max = queue_depths
        .iter()
        .copied()
        .max()
        .unwrap_or(audio::BACKLOG_UNAVAILABLE);

    let (queue_end, dropped, underflows, reopens) = if let Some(stats) = &audio_stats {
        let stats = stats.as_ref();
        (
            stats
                .backlog_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            stats
                .dropped_samples
                .load(std::sync::atomic::Ordering::Relaxed),
            stats
                .underflow_samples
                .load(std::sync::atomic::Ordering::Relaxed),
            stats.reopens.load(std::sync::atomic::Ordering::Relaxed),
        )
    } else {
        (audio::BACKLOG_UNAVAILABLE, 0, 0, 0)
    };
    println!(
        "PROBE_SUMMARY frames={} wall_s={:.3} emulated_fps={:.3} host_frame_ms_avg={:.3} host_frame_ms_p95={:.3} host_frame_ms_max={:.3} samples={} sample_drift={:+.3} audio_queue_min_bytes={} audio_queue_max_bytes={} audio_queue_end_bytes={} dropped_samples={} underflow_samples={} audio_reopens={} baseline_failures={}",
        state.rows.len(),
        elapsed,
        state.rows.len() as f64 / elapsed,
        average_ms,
        p95_ms,
        max_ms,
        actual_samples,
        sample_drift,
        queue_min,
        queue_max,
        queue_end,
        dropped,
        underflows,
        reopens,
        state.baseline_failures.len(),
    );
    if let Some(max_drift) = config.max_sample_drift {
        if sample_drift.abs() > max_drift {
            return Err(format!(
                "audio sample drift {sample_drift:+.3} exceeds {max_drift:.3} samples"
            ));
        }
    }
    if config.require_healthy_audio {
        if !config.realtime {
            return Err("PROBE_REQUIRE_HEALTHY_AUDIO requires PROBE_REALTIME=1".into());
        }
        if queue_end == audio::BACKLOG_UNAVAILABLE {
            return Err("host audio queue was unavailable during validation".into());
        }
        let high_water_bytes = audio_config
            .high_water_samples()
            .saturating_mul(audio::HOST_SAMPLE_BYTES);
        if queue_max > high_water_bytes {
            return Err(format!(
                "audio backlog exceeded high-water mark: {queue_max} > {high_water_bytes} bytes"
            ));
        }
        if dropped != 0 || underflows != 0 || reopens != 0 {
            return Err(format!(
                "host audio health check failed: dropped={dropped} underflows={underflows} reopens={reopens}"
            ));
        }
    }
    if !state.baseline_failures.is_empty() {
        return Err(format!(
            "visual regression failed: {}",
            state.baseline_failures.join(", ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_support_bounded_and_open_ended_ranges() {
        let presses = parse_script("start@10-12,right@20-").unwrap();
        assert_eq!(presses.len(), 2);
        assert_eq!((presses[0].from, presses[0].to), (10, 12));
        assert_eq!((presses[1].from, presses[1].to), (20, u32::MAX));
    }

    #[test]
    fn validation_threshold_rejects_invalid_numbers() {
        assert!(parse_nonnegative_f64("drift", Some("nope".into())).is_err());
        assert!(parse_nonnegative_f64("drift", Some("-1".into())).is_err());
        assert_eq!(
            parse_nonnegative_f64("drift", Some("0.5".into())).unwrap(),
            Some(0.5)
        );
    }

    #[test]
    fn bmp_encoding_is_deterministic_and_has_expected_size() {
        let frame = Frame::new();
        let first = bmp_bytes(&frame);
        assert_eq!(first, bmp_bytes(&frame));
        assert_eq!(&first[..2], b"BM");
        assert_eq!(first.len(), 54 + 256 * 240 * 3);
    }

    #[test]
    fn frame_hash_changes_with_pixels() {
        let mut frame = Frame::new();
        let before = frame_hash(&frame);
        frame.set_pixel(0, 0, (1, 2, 3));
        assert_ne!(before, frame_hash(&frame));
    }
}
