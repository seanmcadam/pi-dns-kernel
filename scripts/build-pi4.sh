#!/bin/bash
set -e

cd "$(dirname "$0")/.."
exec ./scripts/build.sh pi4 "$@"
