#!/bin/bash
# Run all hardware integration tests, recording captures to capture/ directories.
# Usage: ./scripts/capture.sh

set -e

TESTS=(
    "connection"
    "force_recalibration"
    "panel_test_mode"
    "reenable_auto_lights"
    "config_get_set"
    "platform_lights"
    "sensor_test_mode"
    "panel_lights"
    "panel_animation"
    "animation_upload"
    "zzz_factory_reset:factory_reset"
)

for entry in "${TESTS[@]}"; do
    test="${entry%%:*}"
    dir="${entry##*:}"
    echo "=== hardware_${test} ==="
    SMX_CAPTURE_DIR="capture/${dir}" cargo test --test hardware "hardware_${test}" -- --ignored --nocapture
done

echo "=== All captures complete ==="
