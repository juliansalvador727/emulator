#!/bin/sh
set -eu

cd "$(dirname "$0")/.."
manifest=${1:-test-roms/p0-cases.txt}

if [ ! -f "$manifest" ]; then
    echo "missing test-ROM manifest: $manifest" >&2
    exit 2
fi

cargo build --release
revision=$(git rev-parse --verify HEAD 2>/dev/null || echo unknown)
results=${P0_RESULTS:-"${TMPDIR:-/tmp}/nes-p0-results-$$.tsv"}
configuration=ntsc-default
printf 'revision\tconfiguration\tcase\tsha256\tresult\tdetail\n' >"$results"
case_count=0

while IFS='|' read -r id rom expected_hash max_instructions; do
    case "$id" in
        ''|'#'*) continue ;;
    esac
    case_count=$((case_count + 1))

    rom_path=$rom
    if [ ! -f "$rom_path" ] && [ -n "${NES_TEST_ROMS_ROOT:-}" ]; then
        relative_rom=${rom#test-roms/local/}
        rom_path=${NES_TEST_ROMS_ROOT%/}/$relative_rom
    fi

    if [ ! -f "$rom_path" ]; then
        printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$revision" "$configuration" "$id" "$expected_hash" MISSING "$rom_path" >>"$results"
        echo "$id: missing $rom_path" >&2
        continue
    fi

    actual_hash=$(sha256sum "$rom_path" | cut -d ' ' -f 1)
    if [ "$actual_hash" != "$expected_hash" ]; then
        printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$revision" "$configuration" "$id" "$actual_hash" HASH_MISMATCH "$rom_path" >>"$results"
        echo "$id: hash mismatch ($actual_hash, expected $expected_hash)" >&2
        continue
    fi

    output=${TMPDIR:-/tmp}/nes-p0-$id-$$.log
    if target/release/julian_nes_emulator test-rom "$rom_path" "$max_instructions" >"$output" 2>&1; then
        result=PASS
    else
        result=FAIL
    fi
    detail=$(tr '\t\r\n' '   ' <"$output")
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$revision" "$configuration" "$id" "$actual_hash" "$result" "$detail" >>"$results"
    rm -f "$output"
done <"$manifest"

echo "P0 test-ROM results: $results"
if [ "$case_count" -eq 0 ]; then
    echo "no test ROMs configured in $manifest" >&2
    exit 2
fi
if awk -F '\t' 'NR > 1 && $5 != "PASS" { bad = 1 } END { exit bad }' "$results"; then
    exit 0
fi
exit 1
