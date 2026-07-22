#!/bin/sh
set -eu

cd "$(dirname "$0")/.."

frames=${AUDIO_VALIDATION_FRAMES:-7200}
target=x86_64-unknown-linux-gnu
binary="target/$target/release/julian_nes_emulator"

cargo build --release --target "$target"

tail -n +2 probes/cases.txt | while IFS='|' read -r id rom expected mapper script ignored_frames ignored_shots; do
    actual=$(sha256sum "$rom" | cut -d ' ' -f 1)
    if [ "$actual" != "$expected" ]; then
        echo "$id: ROM hash mismatch ($actual, expected $expected)" >&2
        exit 1
    fi

    echo "$id: mapper $mapper, $frames-frame audio clock validation"
    if [ "${PROBE_REALTIME:-0}" = 1 ]; then
        PROBE_REALTIME=1 \
        PROBE_REQUIRE_HEALTHY_AUDIO=1 \
        PROBE_MAX_SAMPLE_DRIFT=1 \
            "$binary" probe "$rom" "$script" "$frames"
    else
        PROBE_MAX_SAMPLE_DRIFT=1 \
            "$binary" probe "$rom" "$script" "$frames"
    fi
done
