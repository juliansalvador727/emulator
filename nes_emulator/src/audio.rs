// Low-latency host audio delivery.
//
// WSLg eventually stops consuming SDL2 queued devices and direct PulseAudio
// streams in this environment. The reference C emulator remains stable
// because it uses SDL3's bound audio-stream API, so the Rust frontend uses the
// same transport. Video, input, and audio all share the same process-wide SDL3
// runtime so its device/event state follows the same lifecycle as that frontend.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use sdl3::AudioSubsystem;
use sdl3::audio::{AudioFormat, AudioSpec, AudioStreamOwner};

pub const SAMPLE_RATE: u32 = 48_000;
pub const HOST_SAMPLE_BYTES: u32 = std::mem::size_of::<i16>() as u32;
pub const PUMP_INTERVAL_MS: u64 = 16;
// The stable C frontend checks and feeds its SDL3 stream once per video frame.
// Matching that cadence avoids repeatedly taking the stream lock while the
// WSLg/Pulse device thread is trying to request its next buffer.
const TICK: Duration = Duration::from_millis(PUMP_INTERVAL_MS);
const CONTROL_INTERVAL: Duration = Duration::from_millis(16);
const REOPEN_RETRY: Duration = Duration::from_secs(2);
const STALL_RESUME_AFTER: Duration = Duration::from_millis(250);
const STALL_CLEAR_AFTER: Duration = Duration::from_millis(600);
const STALL_REOPEN_AFTER: Duration = Duration::from_millis(1250);
const STALL_RECOVERY_CONFIRM: Duration = Duration::from_millis(500);

pub const BACKLOG_UNAVAILABLE: u32 = u32::MAX;

pub fn selected_backend_name() -> &'static str {
    "sdl3-unified-frame-pump"
}

fn pcm_s16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|sample| (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect()
}

fn samples_for_ms(ms: u32) -> u32 {
    ((u64::from(SAMPLE_RATE) * u64::from(ms) + 999) / 1000).min(u64::from(u32::MAX)) as u32
}

fn adaptive_output_ratio(average_backlog: f64, target_bytes: u32) -> f32 {
    let error = (average_backlog - f64::from(target_bytes)) / f64::from(target_bytes.max(1));
    (1.0 - 0.025 * error).clamp(0.985, 1.015) as f32
}

/// Tiny streaming resampler used only for host-clock correction. Keeping this
/// outside SDL is important on WSLg: changing a bound stream's frequency ratio
/// repeatedly can leave its input queue alive while device callbacks stop.
struct ClockCorrector {
    source_position: f64,
    output_ratio: f64,
}

impl ClockCorrector {
    fn new() -> Self {
        Self {
            source_position: 0.0,
            output_ratio: 1.0,
        }
    }

    fn set_output_ratio(&mut self, ratio: f32) {
        self.output_ratio = f64::from(ratio);
    }

    fn reset(&mut self) {
        self.source_position = 0.0;
        self.output_ratio = 1.0;
    }

    fn reset_position(&mut self) {
        self.source_position = 0.0;
    }

    fn render(&mut self, input: &mut VecDeque<f32>, output: &mut Vec<f32>, max_output: usize) {
        output.clear();
        let source_step = 1.0 / self.output_ratio;
        while output.len() < max_output && input.len() >= 2 {
            let first = input[0];
            let second = input[1];
            output.push(first + (second - first) * self.source_position as f32);

            self.source_position += source_step;
            let consumed = self.source_position.floor() as usize;
            self.source_position -= consumed as f64;
            for _ in 0..consumed {
                input.pop_front();
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StallAction {
    None,
    Resume,
    ClearAndResume,
    Reopen,
}

/// Detects a wedged host sink. WSLg can stop consuming a bound stream while
/// SDL still reports the device active, so the paused-state check never
/// fires. Sustained saturation — tick after tick dropping fresh samples as
/// backpressure — is the reliable signal: a healthy stream shows none at all.
/// A resume kick or queue clear can drain one device buffer and briefly stop
/// the drops without actually recovering, so the stall clock keeps running
/// until `STALL_RECOVERY_CONFIRM` of continuous health confirms recovery.
struct StallWatchdog {
    saturated_since: Option<Instant>,
    healthy_since: Option<Instant>,
    resume_attempted: bool,
    clear_attempted: bool,
}

impl StallWatchdog {
    fn new() -> Self {
        Self {
            saturated_since: None,
            healthy_since: None,
            resume_attempted: false,
            clear_attempted: false,
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn observe(&mut self, saturated: bool, now: Instant) -> StallAction {
        if !saturated {
            if self.saturated_since.is_some() {
                let healthy = *self.healthy_since.get_or_insert(now);
                if now.saturating_duration_since(healthy) >= STALL_RECOVERY_CONFIRM {
                    self.reset();
                }
            }
            return StallAction::None;
        }
        self.healthy_since = None;
        let since = *self.saturated_since.get_or_insert(now);
        let elapsed = now.saturating_duration_since(since);
        if elapsed >= STALL_REOPEN_AFTER {
            StallAction::Reopen
        } else if elapsed >= STALL_CLEAR_AFTER && !self.clear_attempted {
            self.clear_attempted = true;
            StallAction::ClearAndResume
        } else if elapsed >= STALL_RESUME_AFTER && !self.resume_attempted {
            self.resume_attempted = true;
            StallAction::Resume
        } else {
            StallAction::None
        }
    }
}

fn excess_pending_samples(
    pending_samples: usize,
    queued_bytes: u32,
    high_water_samples: u32,
) -> usize {
    let queued_samples = queued_bytes.div_ceil(HOST_SAMPLE_BYTES);
    let pending_budget = high_water_samples.saturating_sub(queued_samples) as usize;
    pending_samples.saturating_sub(pending_budget)
}

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
    /// Maximum number of generated samples sent to SDL3 in one call.
    pub device_samples: u16,
    /// Producer delivery size; 256 samples is 5.3 ms at 48 kHz.
    pub delivery_samples: usize,
    /// SDL3 input-stream queue target. Kept under the old field name so the
    /// existing CLI/environment variables remain compatible.
    pub pulse_latency_ms: u32,
}

impl AudioConfig {
    pub fn for_profile(profile: AudioProfile) -> Self {
        match profile {
            AudioProfile::LowLatency => Self {
                profile,
                device_samples: 1024,
                delivery_samples: 256,
                pulse_latency_ms: 40,
            },
            AudioProfile::Balanced => Self {
                profile,
                device_samples: 1024,
                delivery_samples: 256,
                pulse_latency_ms: 80,
            },
        }
    }

    pub fn default_for_host() -> Self {
        // WSL_DISTRO_NAME leaks into Windows processes launched from a WSL
        // shell, so a native Windows build must not use it: WASAPI has none
        // of the WSLg bridge problems the Balanced profile compensates for.
        let profile = if cfg!(windows) {
            AudioProfile::LowLatency
        } else if std::env::var_os("WSL_DISTRO_NAME").is_some() {
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
        samples_for_ms(self.pulse_latency_ms)
    }

    pub fn high_water_samples(&self) -> u32 {
        self.target_queued_samples()
            .saturating_add(4 * u32::from(self.device_samples))
    }
}

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
    pub rate_adjust_ppm: AtomicI32,
    pub backpressure_events: AtomicU64,
    pub device_resumes: AtomicU64,
}

#[derive(Clone)]
pub struct AudioPump {
    pub stats: Arc<AudioStats>,
    tx: Sender<Vec<f32>>,
}

impl AudioPump {
    pub fn start() -> Self {
        let config = AudioConfig::from_env().unwrap_or_else(|err| {
            eprintln!("invalid audio configuration ({err}); using host default");
            AudioConfig::default_for_host()
        });
        Self::start_with_config(config)
    }

    pub fn start_with_config(config: AudioConfig) -> Self {
        let stats = Arc::new(AudioStats {
            backlog_bytes: AtomicU32::new(BACKLOG_UNAVAILABLE),
            queued_bytes: AtomicU32::new(BACKLOG_UNAVAILABLE),
            pending_bytes: AtomicU32::new(0),
            target_samples: AtomicU32::new(config.target_queued_samples()),
            device_samples: AtomicU32::new(u32::from(config.device_samples)),
            reopens: AtomicU64::new(0),
            dropped_samples: AtomicU64::new(0),
            underflow_samples: AtomicU64::new(0),
            lock_miss_samples: AtomicU64::new(0),
            rate_adjust_ppm: AtomicI32::new(0),
            backpressure_events: AtomicU64::new(0),
            device_resumes: AtomicU64::new(0),
        });
        // Acquire the audio subsystem on the caller's thread: the safe sdl3
        // context is main-thread-only, and its init is refcounted so this
        // coexists with the frontend's own sdl3::init().
        let subsystem = match sdl3::init().and_then(|sdl| sdl.audio()) {
            Ok(subsystem) => Some(SendAudioSubsystem(subsystem)),
            Err(error) => {
                eprintln!("SDL3 audio subsystem unavailable ({error}); running without audio");
                None
            }
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let thread_stats = Arc::clone(&stats);
        std::thread::Builder::new()
            .name("audio-pump".into())
            .spawn(move || pump_thread(rx, thread_stats, config, subsystem))
            .expect("failed to spawn audio pump thread");
        Self { stats, tx }
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

    pub fn target_queued_bytes(&self) -> u32 {
        self.stats.target_samples.load(Ordering::Relaxed) * HOST_SAMPLE_BYTES
    }
}

/// Spread emulation across the exact host-sample timeline. Audio queue depth
/// is deliberately not a clock input: the earlier feedback controller ran the
/// game too fast and created audible discontinuities by dropping samples.
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
        if pump.backlog_bytes() == BACKLOG_UNAVAILABLE {
            self.active = false;
            self.next_chunk = Instant::now();
            return;
        }

        let now = Instant::now();
        if !self.active || now.saturating_duration_since(self.next_chunk).as_millis() > 50 {
            self.next_chunk = now;
            self.active = true;
        }
        self.next_chunk += Duration::from_secs_f64(samples as f64 / SAMPLE_RATE as f64);
        let now = Instant::now();
        if now < self.next_chunk {
            std::thread::sleep(self.next_chunk - now);
        }
    }
}

// The sdl3 crate conservatively marks its audio types !Send because they hold
// raw SDL pointers. SDL3 audio streams carry their own lock and their
// functions are documented as callable from any thread; these wrappers exist
// only so ownership can move between the pump and opener threads, and they
// are the only unsafe code in this crate.
struct SendAudioSubsystem(AudioSubsystem);
unsafe impl Send for SendAudioSubsystem {}

struct SendStream(AudioStreamOwner);
unsafe impl Send for SendStream {}

fn open_stream(subsystem: &SendAudioSubsystem) -> Result<SendStream, String> {
    let spec = AudioSpec {
        freq: Some(SAMPLE_RATE as i32),
        channels: Some(1),
        format: Some(AudioFormat::s16_sys()),
    };
    subsystem
        .0
        .default_playback_device()
        .open_device_stream(Some(&spec))
        .map(SendStream)
        .map_err(|error| error.to_string())
}

fn publish_stats(stats: &AudioStats, queued: u32, pending_samples: usize) {
    let pending = (pending_samples as u32).saturating_mul(HOST_SAMPLE_BYTES);
    stats.queued_bytes.store(queued, Ordering::Relaxed);
    stats.pending_bytes.store(pending, Ordering::Relaxed);
    stats
        .backlog_bytes
        .store(queued.saturating_add(pending), Ordering::Relaxed);
}

fn mark_unavailable(stats: &AudioStats) {
    stats
        .backlog_bytes
        .store(BACKLOG_UNAVAILABLE, Ordering::Relaxed);
    stats
        .queued_bytes
        .store(BACKLOG_UNAVAILABLE, Ordering::Relaxed);
}

fn pump_thread(
    rx: Receiver<Vec<f32>>,
    stats: Arc<AudioStats>,
    config: AudioConfig,
    subsystem: Option<SendAudioSubsystem>,
) {
    // Without a host audio subsystem there is nothing to open: stay alive to
    // discard producer samples so gameplay continues unaffected.
    let Some(subsystem) = subsystem else {
        mark_unavailable(&stats);
        loop {
            loop {
                match rx.try_recv() {
                    Ok(samples) => {
                        stats
                            .dropped_samples
                            .fetch_add(samples.len() as u64, Ordering::Relaxed);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
            std::thread::sleep(TICK);
        }
    };

    let target_samples = config.target_queued_samples();
    let high_water_samples = config.high_water_samples();
    let target_bytes = target_samples.saturating_mul(HOST_SAMPLE_BYTES);
    let mut stream: Option<SendStream> = None;
    let mut pending = VecDeque::new();
    let mut chunk = Vec::with_capacity(config.device_samples as usize);
    let mut corrector = ClockCorrector::new();
    let mut reopen_at = Instant::now();
    let mut open_failures = 0u32;
    let mut started = false;
    let mut underflow_at: Option<Instant> = None;
    let mut average_backlog = target_bytes as f64;
    let mut next_control = Instant::now();
    let mut watchdog = StallWatchdog::new();

    // SDL_OpenAudioDeviceStream and SDL_DestroyAudioStream go through the
    // WSLg/Pulse server and can block indefinitely once it stops responding.
    // Both run on a helper thread so a hung call leaves this thread alive to
    // drain the producer channel and keep `pending` bounded.
    let (open_request_tx, open_request_rx) = std::sync::mpsc::channel::<Option<SendStream>>();
    let (open_result_tx, open_result_rx) =
        std::sync::mpsc::channel::<Result<SendStream, String>>();
    std::thread::Builder::new()
        .name("audio-open".into())
        .spawn(move || {
            while let Ok(old_stream) = open_request_rx.recv() {
                drop(old_stream);
                if open_result_tx.send(open_stream(&subsystem)).is_err() {
                    return;
                }
            }
        })
        .expect("failed to spawn audio open thread");
    let mut open_pending = false;

    loop {
        loop {
            match rx.try_recv() {
                Ok(samples) => pending.extend(samples),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        if stream.is_none() && pending.len() > high_water_samples as usize {
            let excess = pending.len() - high_water_samples as usize;
            pending.drain(..excess);
            stats
                .dropped_samples
                .fetch_add(excess as u64, Ordering::Relaxed);
        }

        if open_pending {
            match open_result_rx.try_recv() {
                Ok(Ok(opened)) => {
                    open_pending = false;
                    stream = Some(opened);
                    open_failures = 0;
                    started = false;
                    underflow_at = None;
                    average_backlog = target_bytes as f64;
                    next_control = Instant::now();
                    watchdog.reset();
                    corrector.reset();
                    stats.rate_adjust_ppm.store(0, Ordering::Relaxed);
                    publish_stats(&stats, 0, pending.len());
                }
                Ok(Err(error)) => {
                    open_pending = false;
                    open_failures += 1;
                    if open_failures <= 3 {
                        eprintln!("SDL3 audio stream open failed ({error}); retrying");
                    }
                    mark_unavailable(&stats);
                    reopen_at = Instant::now() + REOPEN_RETRY;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    // The opener thread is gone; audio stays off while
                    // emulation continues. open_pending stays set so no
                    // further requests are attempted.
                }
            }
        }

        if stream.is_none() && !open_pending && Instant::now() >= reopen_at {
            let _ = open_request_tx.send(None);
            open_pending = true;
        }

        if let Some(active) = &stream {
            let mut queued = match active.0.queued_bytes() {
                Ok(queued) => queued.max(0) as u32,
                Err(error) => {
                    eprintln!("SDL3 audio queue query failed ({error}); reopening stream");
                    let _ = open_request_tx.send(stream.take());
                    open_pending = true;
                    stats.reopens.fetch_add(1, Ordering::Relaxed);
                    mark_unavailable(&stats);
                    watchdog.reset();
                    continue;
                }
            };
            let queued_raw = queued;

            // Never clear data already accepted by SDL: WSLg can pause its
            // sink for a callback period, and clearing during that pause turns
            // a harmless queue spike into repeated audible discontinuities.
            // Bound total latency by dropping only the oldest samples that
            // have not yet entered SDL, then let the live stream drain.
            let excess = excess_pending_samples(pending.len(), queued, high_water_samples);
            let backpressured = excess > 0;
            if excess > 0 {
                pending.drain(..excess);
                corrector.reset_position();
                stats
                    .dropped_samples
                    .fetch_add(excess as u64, Ordering::Relaxed);
                stats.backpressure_events.fetch_add(1, Ordering::Relaxed);
            }

            // Make at most one SDL write per pump interval, like the C
            // frontend. The application-side pending queue absorbs a short
            // host pause without inflating SDL's requested latency.
            let mut queue_failed = false;
            if !pending.is_empty() && queued < target_bytes {
                chunk.clear();
                let room_samples = ((target_bytes - queued) / HOST_SAMPLE_BYTES) as usize;
                let take = (config.device_samples as usize)
                    .min(pending.len())
                    .min(room_samples);
                if take > 0 {
                    corrector.render(&mut pending, &mut chunk, take);
                }
                if let Err(error) = (!chunk.is_empty())
                    .then(|| active.0.put_data_i16(&pcm_s16(&chunk)))
                    .unwrap_or(Ok(()))
                {
                    eprintln!("SDL3 audio queue failed ({error}); reopening stream");
                    stats
                        .dropped_samples
                        .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                    queue_failed = true;
                } else {
                    // Avoid a second SDL queue query in the same interval. The
                    // next tick replaces this estimate with the real value.
                    queued = queued
                        .saturating_add((chunk.len() as u32).saturating_mul(HOST_SAMPLE_BYTES));
                }
            }

            if queue_failed {
                let _ = open_request_tx.send(stream.take());
                open_pending = true;
                stats.reopens.fetch_add(1, Ordering::Relaxed);
                mark_unavailable(&stats);
                watchdog.reset();
                continue;
            }

            if let Some(active) = &stream {
                if !started && queued >= target_bytes {
                    match active.0.resume() {
                        Ok(()) => started = true,
                        Err(error) => eprintln!("SDL3 audio resume failed ({error})"),
                    }
                }

                // Device changes and RDP reconnects can leave an otherwise
                // valid bound stream paused. Recover it in place; destroying
                // and reopening the stream is much more disruptive on WSLg.
                if started
                    && backpressured
                    && active.0.device_paused().unwrap_or(false)
                    && active.0.resume().is_ok()
                {
                    stats.device_resumes.fetch_add(1, Ordering::Relaxed);
                }

                // WSLg can also wedge the sink while SDL still reports it
                // active: the queue freezes at target with the write gate
                // shut while every fresh sample drops as backpressure.
                // Escalate in place first; reopen only if saturation holds.
                let saturated = started && backpressured;
                match watchdog.observe(saturated, Instant::now()) {
                    StallAction::None => {}
                    StallAction::Resume => {
                        if active.0.resume().is_ok() {
                            stats.device_resumes.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    StallAction::ClearAndResume => {
                        // After 600ms of continuous drops the audio is already
                        // discontinuous; discarding the frozen queue restarts
                        // writes from empty and, when it works, avoids the far
                        // more hazardous destroy/reopen path.
                        if active.0.clear().is_ok() {
                            stats.dropped_samples.fetch_add(
                                u64::from(queued_raw / HOST_SAMPLE_BYTES),
                                Ordering::Relaxed,
                            );
                            corrector.reset_position();
                        }
                        if active.0.resume().is_ok() {
                            stats.device_resumes.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    StallAction::Reopen => {
                        eprintln!("SDL3 audio device stalled (queue frozen); reopening stream");
                        let _ = open_request_tx.send(stream.take());
                        open_pending = true;
                        stats.reopens.fetch_add(1, Ordering::Relaxed);
                        mark_unavailable(&stats);
                        watchdog.reset();
                        continue;
                    }
                }

                if started && Instant::now() >= next_control {
                    // Equivalent to the C emulator's adaptive sampler. A high
                    // backlog emits slightly fewer host samples; a low backlog
                    // emits slightly more. This correction is performed before
                    // SDL so the bound stream itself remains fixed at 48 kHz.
                    let pending_bytes = (pending.len() as u32).saturating_mul(HOST_SAMPLE_BYTES);
                    let backlog = queued.saturating_add(pending_bytes);
                    average_backlog = average_backlog * 0.9 + f64::from(backlog) * 0.1;
                    let ratio = adaptive_output_ratio(average_backlog, target_bytes);
                    corrector.set_output_ratio(ratio);
                    stats.rate_adjust_ppm.store(
                        ((ratio - 1.0) * 1_000_000.0).round() as i32,
                        Ordering::Relaxed,
                    );
                    next_control = Instant::now() + CONTROL_INTERVAL;
                }

                if started && queued == 0 && pending.is_empty() {
                    underflow_at.get_or_insert_with(Instant::now);
                } else if queued > 0 {
                    if let Some(since) = underflow_at.take() {
                        let missed = (since.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as u64;
                        stats
                            .underflow_samples
                            .fetch_add(missed.max(1), Ordering::Relaxed);
                    }
                }

                publish_stats(&stats, queued, pending.len());
            }
        }

        stats.pending_bytes.store(
            (pending.len() as u32).saturating_mul(HOST_SAMPLE_BYTES),
            Ordering::Relaxed,
        );
        std::thread::sleep(TICK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_profile_uses_subframe_delivery_and_40ms_stream_target() {
        let config = AudioConfig::for_profile(AudioProfile::LowLatency);
        assert_eq!(config.delivery_samples, 256);
        assert_eq!(config.target_queued_samples(), 1920);
        assert_eq!(config.high_water_samples(), 6016);
    }

    #[test]
    fn balanced_profile_uses_an_80ms_stream_target() {
        let config = AudioConfig::for_profile(AudioProfile::Balanced);
        assert_eq!(config.device_samples, 1024);
        assert_eq!(config.delivery_samples, 256);
        assert_eq!(config.target_queued_samples(), 3840);
    }

    #[test]
    fn requested_latency_controls_the_sdl3_queue() {
        let mut config = AudioConfig::for_profile(AudioProfile::LowLatency);
        config.pulse_latency_ms = 60;
        assert_eq!(config.target_queued_samples(), 2880);
        assert_eq!(config.target_queued_samples() * HOST_SAMPLE_BYTES, 5760);
    }

    #[test]
    fn profile_parser_rejects_unknown_values() {
        assert_eq!(
            AudioProfile::parse("low").unwrap(),
            AudioProfile::LowLatency
        );
        assert!(AudioProfile::parse("turbo").is_err());
    }

    #[test]
    fn host_pcm_is_native_48khz_signed_16_bit() {
        assert_eq!(SAMPLE_RATE, 48_000);
        assert_eq!(HOST_SAMPLE_BYTES, 2);
        assert_eq!(
            pcm_s16(&[-2.0, -1.0, 0.0, 0.5, 1.0, 2.0]),
            vec![-32767, -32767, 0, 16383, 32767, 32767]
        );
    }

    #[test]
    fn adaptive_ratio_is_small_and_reduces_output_for_a_high_queue() {
        assert_eq!(adaptive_output_ratio(5760.0, 5760), 1.0);
        assert!(adaptive_output_ratio(8000.0, 5760) < 1.0);
        assert!(adaptive_output_ratio(3000.0, 5760) > 1.0);
        assert_eq!(adaptive_output_ratio(100_000.0, 5760), 0.985);
        assert_eq!(adaptive_output_ratio(0.0, 5760), 1.015);
    }

    #[test]
    fn backpressure_preserves_the_live_stream_and_trims_only_pending_audio() {
        let high_water = 6_976;
        assert_eq!(excess_pending_samples(2_000, 5_760, high_water), 0);
        assert_eq!(excess_pending_samples(5_000, 5_760, high_water), 904);
        assert_eq!(
            excess_pending_samples(1_000, high_water * 2, high_water),
            1_000
        );
    }

    #[test]
    fn stall_watchdog_escalates_resume_clear_then_reopen() {
        let mut watchdog = StallWatchdog::new();
        let start = Instant::now();
        let ms = Duration::from_millis;

        assert_eq!(watchdog.observe(true, start), StallAction::None);
        assert_eq!(watchdog.observe(true, start + ms(100)), StallAction::None);
        assert_eq!(watchdog.observe(true, start + ms(260)), StallAction::Resume);
        // Each in-place recovery is attempted only once per stall.
        assert_eq!(watchdog.observe(true, start + ms(400)), StallAction::None);
        assert_eq!(
            watchdog.observe(true, start + ms(700)),
            StallAction::ClearAndResume
        );
        assert_eq!(watchdog.observe(true, start + ms(900)), StallAction::None);
        assert_eq!(
            watchdog.observe(true, start + ms(1_300)),
            StallAction::Reopen
        );
    }

    #[test]
    fn stall_watchdog_keeps_the_clock_through_brief_remissions() {
        // A resume kick or queue clear drains one device buffer and briefly
        // stops the drops without real recovery; the stall clock must keep
        // running through the gap instead of restarting from zero.
        let mut watchdog = StallWatchdog::new();
        let start = Instant::now();
        let ms = Duration::from_millis;

        watchdog.observe(true, start);
        assert_eq!(watchdog.observe(true, start + ms(260)), StallAction::Resume);
        assert_eq!(watchdog.observe(false, start + ms(300)), StallAction::None);
        assert_eq!(watchdog.observe(false, start + ms(500)), StallAction::None);
        assert_eq!(
            watchdog.observe(true, start + ms(650)),
            StallAction::ClearAndResume
        );
        assert_eq!(watchdog.observe(false, start + ms(700)), StallAction::None);
        assert_eq!(
            watchdog.observe(true, start + ms(1_300)),
            StallAction::Reopen
        );
    }

    #[test]
    fn stall_watchdog_confirms_recovery_before_rearming() {
        let mut watchdog = StallWatchdog::new();
        let start = Instant::now();
        let ms = Duration::from_millis;

        watchdog.observe(true, start);
        assert_eq!(watchdog.observe(true, start + ms(260)), StallAction::Resume);
        // 500ms of continuous health confirms recovery and clears escalation.
        assert_eq!(watchdog.observe(false, start + ms(300)), StallAction::None);
        assert_eq!(watchdog.observe(false, start + ms(850)), StallAction::None);
        // A later stall escalates from scratch instead of reopening at once.
        assert_eq!(watchdog.observe(true, start + ms(900)), StallAction::None);
        assert_eq!(
            watchdog.observe(true, start + ms(1_000)),
            StallAction::None
        );
        assert_eq!(
            watchdog.observe(true, start + ms(1_200)),
            StallAction::Resume
        );
    }

    #[test]
    fn stall_watchdog_stays_quiet_on_a_healthy_stream() {
        let mut watchdog = StallWatchdog::new();
        let start = Instant::now();
        for tick in 0..200u64 {
            assert_eq!(
                watchdog.observe(false, start + Duration::from_millis(16 * tick)),
                StallAction::None
            );
        }
    }

    #[test]
    fn clock_corrector_changes_sample_count_without_touching_sdl() {
        let mut corrector = ClockCorrector::new();
        let source: Vec<f32> = (0..10_002).map(|sample| sample as f32).collect();
        let mut output = Vec::new();

        let mut expanded = VecDeque::from(source.clone());
        corrector.set_output_ratio(1.01);
        corrector.render(&mut expanded, &mut output, 20_000);
        assert!(
            (10_095..=10_105).contains(&output.len()),
            "{}",
            output.len()
        );

        let mut contracted = VecDeque::from(source);
        corrector.reset();
        corrector.set_output_ratio(0.99);
        corrector.render(&mut contracted, &mut output, 20_000);
        assert!((9_895..=9_905).contains(&output.len()), "{}", output.len());
    }
}
