#!/bin/sh
set -eu

cd "$(dirname "$0")/.."
cargo build --release

tail -n +2 probes/cases.txt | while IFS='|' read -r id rom expected mapper script frames shots; do
    actual=$(sha256sum "$rom" | cut -d ' ' -f 1)
    if [ "$actual" != "$expected" ]; then
        echo "$id: ROM hash mismatch ($actual, expected $expected)" >&2
        exit 1
    fi

    output="${TMPDIR:-/tmp}/nes-probe-$id-$$"
    mkdir -p "$output"
    echo "$id: mapper $mapper, $frames frames, shots $shots"
    PROBE_SHOTS="$output" \
    PROBE_SHOT_FRAMES="$shots" \
    PROBE_BASELINES="probes/baselines/$id" \
    PROBE_REPORT="$output/report.csv" \
        target/release/julian_nes_emulator probe "$rom" "$script" "$frames"
    rm -rf "$output"
done
