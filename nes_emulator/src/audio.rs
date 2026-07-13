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

// The game produces ~735 samples per video frame. Keep the SDL queue at roughly
// 35-55 ms so one frame of jitter cannot underflow, but latency cannot grow.
const START_QUEUED_SAMPLES: u32 = 1536;
const TARGET_QUEUED_SAMPLES: u32 = 1536;
const HIGH_WATER_SAMPLES: u32 = 2560;
const PENDING_CAP_SAMPLES: usize = SAMPLE_RATE as usize / 10;
const CHUNK_SAMPLES: usize = 1024;

pub const TARGET_QUEUED_BYTES: u32 = TARGET_QUEUED_SAMPLES * SAMPLE_BYTES;
pub const PACE_TARGET_QUEUED_BYTES: u32 = TARGET_QUEUED_BYTES;

const REOPEN_RETRY: Duration = Duration::from_secs(2);
const STALL_TIMEOUT: Duration = Duration::from_secs(3);
const TICK: Duration = Duration::from_millis(2);

pub const BACKLOG_UNAVAILABLE: u32 = u32::MAX;

pub struct AudioStats {
    pub backlog_bytes: AtomicU32,
    pub reopens: AtomicU64,
    pub dropped_samples: AtomicU64,
    pub underflow_samples: AtomicU64,
    pub lock_miss_samples: AtomicU64,
}

pub struct AudioPump {
    pub stats: Arc<AudioStats>,
    tx: Sender<Vec<f32>>,
}

impl AudioPump {
    pub fn start() -> AudioPump {
        let stats = Arc::new(AudioStats {
            backlog_bytes: AtomicU32::new(BACKLOG_UNAVAILABLE),
            reopens: AtomicU64::new(0),
            dropped_samples: AtomicU64::new(0),
            underflow_samples: AtomicU64::new(0),
            lock_miss_samples: AtomicU64::new(0),
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let thread_stats = stats.clone();
        std::thread::Builder::new()
            .name("audio-pump".into())
            .spawn(move || pump_thread(rx, thread_stats))
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
}

struct RawAudioQueue {
    dev: sdl2::sys::SDL_AudioDeviceID,
}

impl RawAudioQueue {
    fn open() -> Result<RawAudioQueue, String> {
        let desired = sdl2::sys::SDL_AudioSpec {
            freq: SAMPLE_RATE as c_int,
            format: sdl2::sys::AUDIO_F32SYS as sdl2::sys::SDL_AudioFormat,
            channels: 1,
            silence: 0,
            samples: CHUNK_SAMPLES as u16,
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

fn pump_thread(rx: Receiver<Vec<f32>>, stats: Arc<AudioStats>) {
    let mut device: Option<RawAudioQueue> = None;
    let mut pending: VecDeque<f32> = VecDeque::new();
    let mut chunk: Vec<f32> = Vec::with_capacity(CHUNK_SAMPLES);
    let mut open_failures = 0u32;
    let mut started = false;
    let mut reopen_at = Some(Instant::now());
    let mut bytes_queued = 0u64;
    let mut last_consumed = 0u64;
    let mut last_progress = Instant::now();

    loop {
        loop {
            match rx.try_recv() {
                Ok(samples) => pending.extend(samples),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        if pending.len() > PENDING_CAP_SAMPLES {
            let excess = pending.len() - PENDING_CAP_SAMPLES;
            pending.drain(..excess);
            stats
                .dropped_samples
                .fetch_add(excess as u64, Ordering::Relaxed);
        }

        if let Some(dev) = &device {
            let mut queued = dev.size();
            let consumed = bytes_queued.saturating_sub(queued as u64);
            if consumed > last_consumed || queued == 0 {
                last_consumed = consumed;
                last_progress = Instant::now();
            }

            if queued > HIGH_WATER_SAMPLES * SAMPLE_BYTES {
                dev.clear();
                bytes_queued = 0;
                last_consumed = 0;
                queued = 0;
                stats.dropped_samples.fetch_add(
                    (HIGH_WATER_SAMPLES - TARGET_QUEUED_SAMPLES) as u64,
                    Ordering::Relaxed,
                );
            }

            while queued < HIGH_WATER_SAMPLES * SAMPLE_BYTES && !pending.is_empty() {
                let room_samples =
                    ((HIGH_WATER_SAMPLES * SAMPLE_BYTES - queued) / SAMPLE_BYTES) as usize;
                let take = room_samples.min(CHUNK_SAMPLES).min(pending.len());
                chunk.clear();
                chunk.extend(pending.drain(..take));

                if !dev.queue(&chunk) {
                    eprintln!("audio queue failed; reopening device");
                    device = None;
                    started = false;
                    stats.reopens.fetch_add(1, Ordering::Relaxed);
                    stats
                        .backlog_bytes
                        .store(BACKLOG_UNAVAILABLE, Ordering::Relaxed);
                    reopen_at = Some(Instant::now() + REOPEN_RETRY);
                    break;
                }

                bytes_queued += std::mem::size_of_val(chunk.as_slice()) as u64;
                queued = dev.size();
            }

            if let Some(dev) = &device {
                if !started && queued >= START_QUEUED_SAMPLES * SAMPLE_BYTES {
                    dev.resume();
                    started = true;
                    last_progress = Instant::now();
                }

                let backlog =
                    queued.saturating_add((pending.len() as u32).saturating_mul(SAMPLE_BYTES));
                stats.backlog_bytes.store(backlog, Ordering::Relaxed);

                if started && queued > 0 && last_progress.elapsed() > STALL_TIMEOUT {
                    eprintln!("audio queue stalled; reopening device");
                    dev.clear();
                    device = None;
                    started = false;
                    bytes_queued = 0;
                    last_consumed = 0;
                    pending.clear();
                    stats.reopens.fetch_add(1, Ordering::Relaxed);
                    stats
                        .backlog_bytes
                        .store(BACKLOG_UNAVAILABLE, Ordering::Relaxed);
                    reopen_at = Some(Instant::now() + REOPEN_RETRY);
                }
            }
        } else if reopen_at.is_some_and(|at| Instant::now() >= at) {
            match RawAudioQueue::open() {
                Ok(dev) => {
                    device = Some(dev);
                    started = false;
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
                    reopen_at = Some(Instant::now() + REOPEN_RETRY);
                }
            }
        }

        std::thread::sleep(TICK);
    }
}
