// Host-audio isolation probe: pushes a pure 440 Hz tone into an SDL
// AudioQueue at exactly the device rate, paced by the wall clock, with no
// emulator logic at all. Logs the queue depth once per second.
//
// If the queue depth diverges or consumption collapses here too, the fault
// is in the SDL/PulseAudio/WSLg layer, not the emulator's game loop.
//
// Usage: cargo run --release --example audio_probe [seconds]

use sdl2::audio::{AudioQueue, AudioSpecDesired};
use std::f32::consts::TAU;
use std::time::{Duration, Instant};

fn main() {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(90);

    let sdl = sdl2::init().unwrap();
    let audio = sdl.audio().unwrap();
    let desired = AudioSpecDesired {
        freq: Some(44100),
        channels: Some(1),
        samples: Some(1024),
    };
    let device: AudioQueue<f32> = audio.open_queue(None, &desired).unwrap();
    let rate = device.spec().freq as u64;
    eprintln!("driver={} rate={}", audio.current_audio_driver(), rate);

    // Prime ~100 ms then start playback.
    let mut phase = 0f32;
    let mut tone = |n: usize| -> Vec<f32> {
        (0..n)
            .map(|_| {
                phase = (phase + 440.0 / rate as f32) % 1.0;
                (phase * TAU).sin() * 0.05
            })
            .collect()
    };
    device.queue(&tone((rate / 10) as usize));
    device.resume();

    let start = Instant::now();
    let mut produced: u64 = rate / 10;
    let mut last_log = 0u64;
    loop {
        let elapsed = start.elapsed();
        if elapsed.as_secs() >= seconds {
            break;
        }
        // Top production up to wall-clock rate + the 100 ms priming lead.
        let target = rate / 10 + elapsed.as_micros() as u64 * rate / 1_000_000;
        if target > produced {
            let chunk = tone((target - produced) as usize);
            if !device.queue(&chunk) {
                eprintln!("t={:6.2}s queue() FAILED", elapsed.as_secs_f64());
            }
            produced = target;
        }

        if elapsed.as_secs() > last_log {
            last_log = elapsed.as_secs();
            let queued = device.size();
            eprintln!(
                "t={:6.2}s queued={}B ({:.1}ms) produced={}",
                elapsed.as_secs_f64(),
                queued,
                queued as f64 / (rate as f64 * 4.0) * 1000.0,
                produced,
            );
        }
        std::thread::sleep(Duration::from_millis(4));
    }
}
