// Low-latency host audio delivery.
//
// SDL callback mode repeatedly stops being scheduled under WSLg/PulseAudio, so
// this uses SDL's queued-audio API instead. The pump thread owns all SDL audio
// calls, keeps the queued depth small, drops old pending samples if the host
// falls behind, and reopens the device if queued audio stops being consumed.

use std::collections::VecDeque;
use std::ffi::CStr;
use std::os::raw::{c_int, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

pub const SAMPLE_RATE: u32 = 44100;
const SAMPLE_BYTES: u32 = std::mem::size_of::<f32>() as u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioProfile {
    LowLatency,
    Balanced,
}

impl AudioProfile {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "low" | "low-latency" => Ok(Self::LowLatency),
            "balanced" | "safe" => Ok(Self::Balanced),
            _ => Err(format!(
                "unknown audio profile {value:?}; expected low or balanced"
            )),
        }
    }
}

#[derive(Clone, Debug)]
pub struct AudioConfig {
    pub profile: AudioProfile,
    pub device_samples: u16,
    pub delivery_samples: usize,
    start_queued_samples: u32,
    target_queued_samples: u32,
    high_water_samples: u32,
    pending_cap_samples: usize,
    pub pulse_latency_ms: u32,
}

impl AudioConfig {
    pub fn for_profile(profile: AudioProfile) -> Self {
        match profile {
            // Small, frequent producer chunks make a sub-frame queue practical:
            // 256 samples = 5.8 ms, target 768 = 17.4 ms, and the 1280-sample
            // hard ceiling is 29.0 ms at 44.1 kHz.
            AudioProfile::LowLatency => Self {
                profile,
                device_samples: 256,
                delivery_samples: 256,
                start_queued_samples: 1024,
                target_queued_samples: 768,
                high_water_samples: 1280,
                pending_cap_samples: 1280,
                pulse_latency_ms: 40,
            },
            // WSLg/Pulse can stop consuming very small device buffers. Preserve
            // the established safe envelope as an explicit fallback profile,
            // while still delivering producer chunks during each video frame.
            AudioProfile::Balanced => Self {
                profile,
                device_samples: 1024,
                delivery_samples: 256,
                start_queued_samples: 1536,
                target_queued_samples: 1536,
                high_water_samples: 2560,
                pending_cap_samples: SAMPLE_RATE as usize / 10,
                pulse_latency_ms: 80,
            },
        }
    }

    pub fn default_for_host() -> Self {
        let profile = if std::env::var_os("WSL_DISTRO_NAME").is_some() {
            AudioProfile::Balanced
        } else {
            AudioProfile::LowLatency
        };
        Self::for_profile(profile)
    }

    pub fn from_env() -> Result<Self, String> {
        let mut config = Self::default_for_host();
        if let Ok(value) = std::env::var("NES_AUDIO_PROFILE") {
            config = Self::for_profile(AudioProfile::parse(&value)?);
        }
        if let Ok(value) = std::env::var("NES_AUDIO_LATENCY_MS") {
            config.pulse_latency_ms = value
                .parse::<u32>()
                .map_err(|_| "NES_AUDIO_LATENCY_MS must be an integer".to_string())?;
            if config.pulse_latency_ms == 0 {
                return Err("NES_AUDIO_LATENCY_MS must be greater than zero".into());
            }
        }
        Ok(config)
    }

    pub fn target_queued_samples(&self) -> u32 {
        self.target_queued_samples
    }

    pub fn high_water_samples(&self) -> u32 {
        self.high_water_samples
    }
}

const REOPEN_RETRY: Duration = Duration::from_secs(2);
const STALL_TIMEOUT: Duration = Duration::from_secs(3);
const TICK: Duration = Duration::from_millis(2);

pub const BACKLOG_UNAVAILABLE: u32 = u32::MAX;

pub struct AudioStats {
    pub backlog_bytes: AtomicU32,
    pub queued_bytes: AtomicU32,
    pub pending_bytes: AtomicU32,
    pub target_samples: AtomicU32,
    pub device_samples: AtomicU32,
    pub reopens: AtomicU64,
    pub dropped_samples: AtomicU64,
    pub underflow_samples: AtomicU64,
    pub lock_miss_samples: AtomicU64,
}

#[derive(Clone)]
pub struct AudioPump {
    pub stats: Arc<AudioStats>,
    tx: Sender<Vec<f32>>,
}

impl AudioPump {
    pub fn start() -> AudioPump {
        let config = AudioConfig::from_env().unwrap_or_else(|err| {
            eprintln!("invalid audio configuration ({err}); using host default");
            AudioConfig::default_for_host()
        });
        Self::start_with_config(config)
    }

    pub fn start_with_config(config: AudioConfig) -> AudioPump {
        let stats = Arc::new(AudioStats {
            backlog_bytes: AtomicU32::new(BACKLOG_UNAVAILABLE),
            queued_bytes: AtomicU32::new(BACKLOG_UNAVAILABLE),
            pending_bytes: AtomicU32::new(0),
            target_samples: AtomicU32::new(config.target_queued_samples),
            device_samples: AtomicU32::new(config.device_samples as u32),
            reopens: AtomicU64::new(0),
            dropped_samples: AtomicU64::new(0),
            underflow_samples: AtomicU64::new(0),
            lock_miss_samples: AtomicU64::new(0),
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let thread_stats = stats.clone();
        std::thread::Builder::new()
            .name("audio-pump".into())
            .spawn(move || pump_thread(rx, thread_stats, config))
            .expect("failed to spawn audio pump thread");
        AudioPump { stats, tx }
    }

    pub fn push(&self, samples: Vec<f32>) {
        if !samples.is_empty() {
            let _ = self.tx.send(samples);
        }
    }

    pub fn backlog_bytes(&self) -> u32 {
        self.stats.backlog_bytes.load(Ordering::Relaxed)
    }

    pub fn queued_bytes(&self) -> u32 {
        self.stats.queued_bytes.load(Ordering::Relaxed)
    }

    pub fn pending_bytes(&self) -> u32 {
        self.stats.pending_bytes.load(Ordering::Relaxed)
    }

    pub fn pace_target_queued_bytes(&self) -> u32 {
        self.stats.target_samples.load(Ordering::Relaxed) * SAMPLE_BYTES
    }
}

/// Spreads emulation across the wall-clock audio timeline. Delivering small
/// chunks alone is not enough when a fast emulator produces a whole frame in
/// a burst and then sleeps; this pacer waits at each chunk boundary so a
/// 12-20 ms queue can remain fed without requiring a full frame of cushion.
pub struct AudioPacer {
    next_chunk: Instant,
    active: bool,
}

impl AudioPacer {
    pub fn new() -> Self {
        Self {
            next_chunk: Instant::now(),
            active: false,
        }
    }

    pub fn pace(&mut self, samples: usize, pump: &AudioPump) {
        if samples == 0 {
            return;
        }
        let backlog = pump.backlog_bytes();
        if backlog == BACKLOG_UNAVAILABLE {
            self.active = false;
            self.next_chunk = Instant::now();
            return;
        }

        let now = Instant::now();
        if !self.active || now.saturating_duration_since(self.next_chunk).as_millis() > 50 {
            self.next_chunk = now;
            self.active = true;
        }
        let bytes_per_second = SAMPLE_RATE as f64 * SAMPLE_BYTES as f64;
        let error_secs =
            (backlog as f64 - pump.pace_target_queued_bytes() as f64) / bytes_per_second;
        let nominal = samples as f64 / SAMPLE_RATE as f64;
        let correction = (error_secs * 0.025).clamp(-0.001, 0.001);
        self.next_chunk += Duration::from_secs_f64(nominal + correction);
        let now = Instant::now();
        if now < self.next_chunk {
            std::thread::sleep(self.next_chunk - now);
        }
    }
}

struct RawAudioQueue {
    dev: sdl2::sys::SDL_AudioDeviceID,
}

impl RawAudioQueue {
    fn open(config: &AudioConfig, stats: &AudioStats) -> Result<RawAudioQueue, String> {
        let desired = sdl2::sys::SDL_AudioSpec {
            freq: SAMPLE_RATE as c_int,
            format: sdl2::sys::AUDIO_F32SYS as sdl2::sys::SDL_AudioFormat,
            channels: 1,
            silence: 0,
            samples: config.device_samples,
            padding: 0,
            size: 0,
            callback: None,
            userdata: std::ptr::null_mut(),
        };
        let mut obtained = desired;

        unsafe {
            if sdl2::sys::SDL_InitSubSystem(sdl2::sys::SDL_INIT_AUDIO) != 0 {
                return Err(sdl_error());
            }
            let dev =
                sdl2::sys::SDL_OpenAudioDevice(std::ptr::null(), 0, &desired, &mut obtained, 0);
            if dev == 0 {
                return Err(sdl_error());
            }
            stats
                .device_samples
                .store(obtained.samples as u32, Ordering::Relaxed);
            Ok(RawAudioQueue { dev })
        }
    }

    fn queue(&self, samples: &[f32]) -> bool {
        unsafe {
            sdl2::sys::SDL_QueueAudio(
                self.dev,
                samples.as_ptr() as *const c_void,
                std::mem::size_of_val(samples) as u32,
            ) == 0
        }
    }

    fn size(&self) -> u32 {
        unsafe { sdl2::sys::SDL_GetQueuedAudioSize(self.dev) }
    }

    fn clear(&self) {
        unsafe { sdl2::sys::SDL_ClearQueuedAudio(self.dev) }
    }

    fn resume(&self) {
        unsafe { sdl2::sys::SDL_PauseAudioDevice(self.dev, 0) }
    }
}

impl Drop for RawAudioQueue {
    fn drop(&mut self) {
        unsafe { sdl2::sys::SDL_CloseAudioDevice(self.dev) }
    }
}

fn sdl_error() -> String {
    unsafe {
        CStr::from_ptr(sdl2::sys::SDL_GetError())
            .to_string_lossy()
            .into_owned()
    }
}

fn pump_thread(rx: Receiver<Vec<f32>>, stats: Arc<AudioStats>, config: AudioConfig) {
    let mut device: Option<RawAudioQueue> = None;
    let mut pending: VecDeque<f32> = VecDeque::new();
    let mut chunk: Vec<f32> = Vec::with_capacity(config.device_samples as usize);
    let mut open_failures = 0u32;
    let mut started = false;
    let mut reopen_at = Some(Instant::now());
    let mut bytes_queued = 0u64;
    let mut last_consumed = 0u64;
    let mut last_progress = Instant::now();
    let mut underflow_at: Option<Instant> = None;
    let mut adaptive_target = config.target_queued_samples;
    let mut last_target_change = Instant::now();

    loop {
        loop {
            match rx.try_recv() {
                Ok(samples) => pending.extend(samples),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        if pending.len() > config.pending_cap_samples {
            let excess = pending.len() - config.pending_cap_samples;
            pending.drain(..excess);
            stats
                .dropped_samples
                .fetch_add(excess as u64, Ordering::Relaxed);
        }
        stats.pending_bytes.store(
            (pending.len() as u32).saturating_mul(SAMPLE_BYTES),
            Ordering::Relaxed,
        );

        if let Some(dev) = &device {
            let mut queued = dev.size();
            let now = Instant::now();
            if started && queued == 0 {
                if underflow_at.is_none() {
                    underflow_at = Some(now);
                    let adaptive_max = config
                        .high_water_samples
                        .saturating_sub(config.device_samples as u32)
                        .max(config.target_queued_samples);
                    adaptive_target = adaptive_target
                        .saturating_add(config.device_samples as u32)
                        .min(adaptive_max);
                    stats
                        .target_samples
                        .store(adaptive_target, Ordering::Relaxed);
                    last_target_change = now;
                }
            } else if queued > 0 {
                if let Some(since) = underflow_at.take() {
                    let missed = (since.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as u64;
                    stats
                        .underflow_samples
                        .fetch_add(missed.max(1), Ordering::Relaxed);
                }
                // After ten quiet seconds, cautiously return an inflated
                // target toward the selected profile's latency budget.
                if adaptive_target > config.target_queued_samples
                    && last_target_change.elapsed() >= Duration::from_secs(10)
                {
                    adaptive_target = adaptive_target
                        .saturating_sub(config.device_samples as u32)
                        .max(config.target_queued_samples);
                    stats
                        .target_samples
                        .store(adaptive_target, Ordering::Relaxed);
                    last_target_change = now;
                }
            }
            let consumed = bytes_queued.saturating_sub(queued as u64);
            if consumed > last_consumed || queued == 0 {
                last_consumed = consumed;
                last_progress = Instant::now();
            }

            if queued > config.high_water_samples * SAMPLE_BYTES {
                let discarded = queued / SAMPLE_BYTES;
                dev.clear();
                bytes_queued = 0;
                last_consumed = 0;
                queued = 0;
                stats
                    .dropped_samples
                    .fetch_add(discarded as u64, Ordering::Relaxed);
            }

            // Bound total application-side latency, not only SDL's queue. If
            // the producer outruns the sink, discard the oldest pending audio
            // so newly generated sound remains close to the current frame.
            let queued_samples = queued / SAMPLE_BYTES;
            let allowed_pending = config
                .high_water_samples
                .saturating_sub(queued_samples) as usize;
            if pending.len() > allowed_pending {
                let excess = pending.len() - allowed_pending;
                pending.drain(..excess);
                stats
                    .dropped_samples
                    .fetch_add(excess as u64, Ordering::Relaxed);
            }

            let queue_target = if started {
                adaptive_target
            } else {
                config.start_queued_samples
            };
            while queued < queue_target * SAMPLE_BYTES && !pending.is_empty() {
                let room_samples =
                    ((queue_target * SAMPLE_BYTES - queued) / SAMPLE_BYTES) as usize;
                let take = room_samples
                    .min(config.device_samples as usize)
                    .min(pending.len());
                chunk.clear();
                chunk.extend(pending.drain(..take));

                if !dev.queue(&chunk) {
                    eprintln!("audio queue failed; reopening device");
                    device = None;
                    started = false;
                    underflow_at = None;
                    stats.reopens.fetch_add(1, Ordering::Relaxed);
                    stats
                        .backlog_bytes
                        .store(BACKLOG_UNAVAILABLE, Ordering::Relaxed);
                    stats
                        .queued_bytes
                        .store(BACKLOG_UNAVAILABLE, Ordering::Relaxed);
                    reopen_at = Some(Instant::now() + REOPEN_RETRY);
                    break;
                }

                bytes_queued += std::mem::size_of_val(chunk.as_slice()) as u64;
                queued = dev.size();
            }

            if let Some(dev) = &device {
                if !started && queued >= config.start_queued_samples * SAMPLE_BYTES {
                    dev.resume();
                    started = true;
                    last_progress = Instant::now();
                }

                let backlog =
                    queued.saturating_add((pending.len() as u32).saturating_mul(SAMPLE_BYTES));
                stats.queued_bytes.store(queued, Ordering::Relaxed);
                stats.pending_bytes.store(
                    (pending.len() as u32).saturating_mul(SAMPLE_BYTES),
                    Ordering::Relaxed,
                );
                stats.backlog_bytes.store(backlog, Ordering::Relaxed);

                if started && queued > 0 && last_progress.elapsed() > STALL_TIMEOUT {
                    eprintln!("audio queue stalled; reopening device");
                    dev.clear();
                    device = None;
                    started = false;
                    underflow_at = None;
                    bytes_queued = 0;
                    last_consumed = 0;
                    pending.clear();
                    stats.reopens.fetch_add(1, Ordering::Relaxed);
                    stats
                        .backlog_bytes
                        .store(BACKLOG_UNAVAILABLE, Ordering::Relaxed);
                    stats
                        .queued_bytes
                        .store(BACKLOG_UNAVAILABLE, Ordering::Relaxed);
                    stats.pending_bytes.store(0, Ordering::Relaxed);
                    reopen_at = Some(Instant::now() + REOPEN_RETRY);
                }
            }
        } else if reopen_at.is_some_and(|at| Instant::now() >= at) {
            match RawAudioQueue::open(&config, &stats) {
                Ok(dev) => {
                    device = Some(dev);
                    started = false;
                    underflow_at = None;
                    bytes_queued = 0;
                    last_consumed = 0;
                    last_progress = Instant::now();
                    open_failures = 0;
                    reopen_at = None;
                }
                Err(err) => {
                    open_failures += 1;
                    if open_failures <= 3 {
                        eprintln!("audio device open failed ({}); retrying", err);
                    }
                    stats
                        .backlog_bytes
                        .store(BACKLOG_UNAVAILABLE, Ordering::Relaxed);
                    stats
                        .queued_bytes
                        .store(BACKLOG_UNAVAILABLE, Ordering::Relaxed);
                    reopen_at = Some(Instant::now() + REOPEN_RETRY);
                }
            }
        }

        std::thread::sleep(TICK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_latency_profile_has_a_sub_frame_queue_budget() {
        let config = AudioConfig::for_profile(AudioProfile::LowLatency);
        assert_eq!(config.delivery_samples, 256);
        assert_eq!(config.target_queued_samples(), 768);
        assert_eq!(config.high_water_samples(), 1280);
        assert!(config.high_water_samples() < SAMPLE_RATE / 30);
    }

    #[test]
    fn balanced_profile_preserves_the_wsl_safe_device_period() {
        let config = AudioConfig::for_profile(AudioProfile::Balanced);
        assert_eq!(config.device_samples, 1024);
        assert_eq!(config.pulse_latency_ms, 80);
        assert_eq!(config.delivery_samples, 256);
    }

    #[test]
    fn profile_parser_rejects_unknown_values() {
        assert_eq!(AudioProfile::parse("low").unwrap(), AudioProfile::LowLatency);
        assert!(AudioProfile::parse("turbo").is_err());
    }
}
