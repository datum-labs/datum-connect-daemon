#!/bin/sh
# Builds the immutable sandbox image. Run from a machine with Docker and
# the already-built Linux datum-connect-daemon/datumctl-connect binaries
# reachable at the paths below -- adjust if your checkout layout differs.
set -e
cd "$(dirname "$0")"

DAEMON_BIN=${DAEMON_BIN:-../../connect/connect-lib/target/debug/datum-connect-daemon}
CLI_BIN=${CLI_BIN:-../../connect/connect-plugin/datumctl-connect}

cp "$DAEMON_BIN" ./datum-connect-daemon
cp "$CLI_BIN" ./datumctl-connect

docker build -t datum-daytona-poc:latest .

echo "Built datum-daytona-poc:latest"
