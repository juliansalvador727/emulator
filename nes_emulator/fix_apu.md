# Fix: audio stalls permanently after ~13s (not an APU bug)

## Symptom

Running with `--latency-debug`, playback dies partway through a session and the log
locks into this state forever:

```
queued=5760B pending=8192B target=5760B rate_adjust=-15000ppm
backpressure=<climbing> dropped=<climbing> underflows=0 reopens=0 resumes=0
```

## Diagnosis

The APU is fine — `produced` keeps advancing at ~48kHz the whole time. The failure is
in the host audio path: the WSLg/Pulse device thread behind the SDL3 bound stream stops
consuming data, while SDL continues to report the device as active.

Why no existing recovery path fires (`src/audio.rs`):

- **Reopen on queue-query error** (`audio.rs:463`): `SDL_GetAudioStreamQueued` keeps
  succeeding — it just returns the same frozen value.
- **Reopen on write error** (`audio.rs:504`): writes are gated on
  `queued < target_bytes`. Once the queue freezes exactly at target (5760B), no write
  ever happens again, so the put path is never exercised.
- **In-place resume** (`audio.rs:541`): gated on `SDL_AudioStreamDevicePaused()`,
  which keeps returning `false` even though the sink is wedged.

Result: `pending` fills to its high-water budget (8192B), and every new sample is
dropped as backpressure, forever.

## Fix: stall watchdog in `pump_thread` (`src/audio.rs`)

Detect the frozen queue directly instead of trusting SDL's paused flag, with two-stage
escalation.

### 1. `StallWatchdog` — pure, unit-testable helper

```rust
enum StallAction { None, Resume, Reopen }

struct StallWatchdog {
    last_queued: u32,
    stall_since: Option<Instant>,
    resume_attempted: bool,
}
```

`observe(queued, pending_empty, started, target_bytes, now) -> StallAction`

- **Stall condition:** `started && !pending_empty && queued == last_queued &&
  queued >= target_bytes`. A healthy device drains its queue every buffer period
  (~21ms at 1024 samples), so an unchanged reading across pump ticks with data waiting
  means the sink is not consuming.
- **Escalation:**
  - stall ≥ 250ms → `Resume` (once) — handles SDL's false "active" state
  - stall ≥ 1s → `Reopen`
- Any tick where the condition doesn't hold fully resets the watchdog; `reset()` is
  also called on every fresh stream open.

Constants `STALL_RESUME_AFTER = 250ms` and `STALL_REOPEN_AFTER = 1s` live next to the
existing `REOPEN_RETRY`.

### 2. Wiring

- Capture the raw queue reading immediately after `active.queued_bytes()` — before the
  post-write estimate at `audio.rs:516` mutates `queued` — and feed it to the watchdog
  in the `started` block near the existing resume logic.
- `Resume` → `active.resume()`, bump `stats.device_resumes`.
- `Reopen` → same shape as the existing `queue_failed` path (`audio.rs:521-527`): set a
  flag inside the stream borrow, then outside it log
  `"SDL3 audio device stalled (queue frozen); reopening stream"`, set `stream = None`,
  bump `stats.reopens`, `mark_unavailable`, `reopen_at = now`, `watchdog.reset()`,
  `continue`.
- Keep `pending` across the reopen — the high-water trim (`audio.rs:430`) bounds it,
  and the reopen path already re-primes to target before resuming.
- The existing `device_paused()`-gated resume stays; it catches the cheap case earlier.

### 3. Tests

Unit tests for `StallWatchdog` with synthetic `Instant`s:

- frozen at target with pending data → `None` until 250ms, `Resume` once, `Reopen` at 1s
- queued changing between observations → never escalates
- queued below target (normal drain) → never escalates
- pending empty (emulator paused / no producer) → never escalates
- reset clears escalation state

## Verification

1. `cargo test` in `nes_emulator/`.
2. `cargo run --release -- ../games/mario.nes --audio-profile low --audio-latency-ms 60
   --latency-debug` for several minutes (the wedge is nondeterministic). Confirm in the
   LATENCY lines:
   - on a stall: `reopens` increments, `queued` starts moving again, `dropped` stops
     climbing (no more permanent `-15000ppm`)
   - during healthy playback: `reopens` stays 0 (no spurious recoveries)

## Round 2: findings from the first live wedge (2026-07-14)

The v1 watchdog (frozen-queue equality check, resume at 250ms, reopen at 1s) fired but
exposed two deeper problems:

1. **Escalation took ~9.5s instead of 1s.** Each resume kicked WSLg into draining one
   device buffer, the queue reading changed, and that micro-drain fully reset the
   watchdog (`resumes` climbed ~2/s while `dropped` kept growing). The strict
   "unchanged reading" condition kept restarting the clock.
2. **The reopen itself hung.** After destroy, `SDL_OpenAudioDeviceStream` never
   returned — the WSLg Pulse server is wedged process-wide. The pump thread blocked
   inside the call forever: stats froze at `unavailable`, and the unbounded producer
   channel started leaking memory while the game kept running silently.

### v2 design (implemented)

- **Stall signal = sustained backpressure**, not queue equality: `started && excess
  pending dropped this tick`. A healthy stream shows zero backpressure events, and the
  signal held every tick through the entire wedge, including the resume micro-drains.
  Recovery only counts after 500ms of continuous health (`STALL_RECOVERY_CONFIRM`), so
  brief remissions no longer restart the clock.
- **Three-stage escalation**: resume at 250ms, `SDL_ClearAudioStream` + resume at
  600ms (in-place recovery without touching the hazardous device open path), destroy +
  reopen at 1250ms of cumulative saturation.
- **Open/destroy moved to a dedicated `audio-open` helper thread** (streams get an
  `unsafe impl Send`; SDL3 stream calls are documented thread-safe). The pump thread
  sends the old stream over a channel for disposal and polls for the open result, so a
  hung SDL call can no longer freeze the pump — it keeps draining the producer channel
  and trimming `pending` to the high-water mark (bounded memory), and retries the open
  every `REOPEN_RETRY` if the attempt returns an error.

Residual limitation: if the WSLg server is wedged so hard that `SDL_OpenAudioDeviceStream`
blocks forever, audio stays off for the rest of the session (one stuck helper thread),
but emulation, video, and input continue normally with bounded memory. Only a WSLg
restart (`wsl --shutdown`) recovers audio in that case.

## Notes / alternatives considered

- Flush-and-resume alone may not help if the WSLg device thread itself is dead; the
  watchdog's reopen stage covers that.
- Callback-driven audio or a cpal/WASAPI backend would sidestep SDL's input queue, but
  still needs a stall watchdog under WSLg, so the watchdog is the right first fix.
- Environmental workaround until fixed: restart the emulator, or `wsl --shutdown` from
  Windows PowerShell to restart WSLg.
