#!/bin/bash
set -euo pipefail

# Build script for the datum-connect plugin
# Usage: ./scripts/build.sh [--test]
#   --test  Run E2E tests after build (default: skip)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONNECT_DIR="$(dirname "$SCRIPT_DIR")"

cd "$CONNECT_DIR"

echo "Building datum-connect plugin..."

if go build -o datumctl-connect . ; then
    echo "Build successful: $CONNECT_DIR/datumctl-connect"
else
    echo "Build failed" >&2
    exit 1
fi

# Run E2E tests if --test flag is provided
if [[ "${1:-}" == "--test" ]]; then
    echo ""
    echo "Running E2E tests..."
    if go test -count=1 ./e2e_test.go; then
        echo "E2E tests passed"
    else
        echo "E2E tests failed" >&2
        exit 1
    fi
fi
