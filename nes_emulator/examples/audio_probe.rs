// Host-audio isolation probe: pushes a pure 440 Hz tone through the exact
// SDL3 stream pump used by the emulator, without video initialization or any
// emulation workload.
//
// Usage: cargo run --release --example audio_probe [seconds]

#[allow(dead_code)]
#[path = "../src/audio.rs"]
mod audio;

use std::f32::consts::TAU;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

fn main() {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(90);

    let mut config = audio::AudioConfig::for_profile(audio::AudioProfile::LowLatency);
    config.pulse_latency_ms = 60;
    let chunk_samples = config.delivery_samples;
    let pump = audio::AudioPump::start_with_config(config);
    let mut phase = 0.0f32;
    let chunk_duration = Duration::from_secs_f64(chunk_samples as f64 / audio::SAMPLE_RATE as f64);
    let start = Instant::now();
    let mut next_chunk = start;
    let mut next_log = start + Duration::from_secs(1);
    let mut produced = 0u64;

    while start.elapsed().as_secs() < seconds {
        let samples: Vec<f32> = (0..chunk_samples)
            .map(|_| {
                phase = (phase + 440.0 / audio::SAMPLE_RATE as f32) % 1.0;
                (phase * TAU).sin() * 0.05
            })
            .collect();
        pump.push(samples);
        produced += chunk_samples as u64;
        next_chunk += chunk_duration;

        if Instant::now() >= next_log {
            let queued = pump.queued_bytes();
            let queued_label = if queued == audio::BACKLOG_UNAVAILABLE {
                "unavailable".to_string()
            } else {
                format!("{}B/{:.1}ms", queued, queued as f64 / 96.0)
            };
            eprintln!(
                "t={:6.2}s queued={} pending={}B rate_adjust={:+}ppm produced={} reopens={} resumes={} backpressure={} dropped={} underflows={}",
                start.elapsed().as_secs_f64(),
                queued_label,
                pump.pending_bytes(),
                pump.stats.rate_adjust_ppm.load(Ordering::Relaxed),
                produced,
                pump.stats.reopens.load(Ordering::Relaxed),
                pump.stats.device_resumes.load(Ordering::Relaxed),
                pump.stats.backpressure_events.load(Ordering::Relaxed),
                pump.stats.dropped_samples.load(Ordering::Relaxed),
                pump.stats.underflow_samples.load(Ordering::Relaxed),
            );
            next_log += Duration::from_secs(1);
        }

        let now = Instant::now();
        if now < next_chunk {
            std::thread::sleep(next_chunk - now);
        } else if now.duration_since(next_chunk) > Duration::from_millis(50) {
            next_chunk = now;
        }
    }
}
