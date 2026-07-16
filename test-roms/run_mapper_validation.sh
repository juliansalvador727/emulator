#!/bin/sh
set -eu

cd "$(dirname "$0")/.."
python3 test-roms/src/build_mmc1_fixtures.py
exec test-roms/run_p0_validation.sh test-roms/mapper-cases.txt
