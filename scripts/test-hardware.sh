#!/bin/bash
# Run all hardware integration tests (no capture recording).
# Usage: ./scripts/test-hardware.sh

set -e
cargo test --test hardware -- --ignored --test-threads=1 --nocapture
