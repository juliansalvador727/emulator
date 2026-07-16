#!/bin/sh
set -eu

cd "$(dirname "$0")/.."
exec test-roms/run_p0_validation.sh test-roms/mapper-cases.txt
