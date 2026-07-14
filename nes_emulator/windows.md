# Running natively on Windows

Running the emulator as a native Windows executable removes all latency problems —
fps, audio, and input are all clean. This is the preferred way to run.

## Why

Under WSL2, audio travels emulator → SDL3 → PulseAudio → WSLg's RDP bridge → the
Windows audio stack. That bridge nondeterministically wedges mid-session (see
`fix_apu.md`), and even when healthy it adds latency to audio, video present, and
input. A native Windows build lets SDL3 talk directly to WASAPI and the Windows
compositor, bypassing the bridge entirely. The emulator core is unaffected either
way — this is purely a host-transport issue.

## How to run

Cross-compile from the WSL shell and launch through Windows interop — the `.exe`
runs as a real Windows process even though it's started from WSL:

```
cargo win -- ../games/mario.nes
```

`cargo win` is an alias defined in `.cargo/config.toml` for
`run --release --target x86_64-pc-windows-gnu`. There is also `cargo win-build`
to build without launching. The binary lands at
`target/x86_64-pc-windows-gnu/release/julian_nes_emulator.exe` and can be copied
to and run from the Windows side directly.

## One-time setup

```
sudo apt-get install -y gcc-mingw-w64-x86-64 g++-mingw-w64-x86-64
rustup target add x86_64-pc-windows-gnu
```

The first build is slow because SDL3 compiles from source for the Windows target.

## Notes

- On native Windows the audio profile defaults to LowLatency at compile time
  (`AudioConfig::default_for_host`). The runtime `WSL_DISTRO_NAME` check is skipped
  because that variable leaks into Windows processes launched from a WSL shell.
- Lower latency targets work fine on WASAPI — `--audio-latency-ms 20` is realistic,
  versus the 60ms+ needed to keep WSLg stable.
- The audio stall watchdog in `src/audio.rs` stays in the build as a safety net for
  WSLg runs; on a healthy WASAPI backend it never triggers.
