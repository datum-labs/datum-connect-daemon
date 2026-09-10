# Immutable sandbox image for the Daytona "call home over a peer tunnel"
# POC -- see NOTES.md's "Immutable sandbox image POC" section.
#
# Deliberately carries zero secrets and zero per-run config. The only
# thing that varies between sandbox instances is $SANDBOX_TICKET, injected
# as an env var at `daytona.create(...)` time, never baked in here.
#
# Build context expectations (see build.sh): datum-connect-daemon and
# datumctl-connect are pre-built Linux binaries copied into this directory
# before `docker build` runs -- this Dockerfile does not compile anything
# itself, on purpose, so the image build is fast and doesn't need the Rust/
# Go toolchains present in the final image at all.
#
# Base image must match the glibc version of whatever machine built the
# daemon binary (found the hard way: debian:12-slim's older glibc can't
# load a binary built against Ubuntu 24.04's glibc 2.39). Ubuntu 24.04
# guarantees a match since that's what actually builds the binary in this
# POC's setup.
FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    curl \
    ca-certificates \
    jq \
    && rm -rf /var/lib/apt/lists/*

RUN mkdir -p /opt/datum
COPY datum-connect-daemon /opt/datum/datum-connect-daemon
COPY datumctl-connect /opt/datum/datumctl-connect
COPY fake-credentials-helper.sh /opt/datum/fake-credentials-helper.sh
COPY entrypoint.sh /opt/datum/entrypoint.sh
RUN chmod +x /opt/datum/datum-connect-daemon /opt/datum/datumctl-connect \
    /opt/datum/fake-credentials-helper.sh /opt/datum/entrypoint.sh

# Deliberately no ENTRYPOINT/CMD here -- Daytona sandboxes are long-running
# environments you exec commands into (sandbox.process.exec), not batch
# jobs whose image entrypoint runs once and exits. Setting a custom
# ENTRYPOINT here caused it to run automatically as the container's main
# process *in addition to* the explicit exec() call in run_demo.py --
# two daemon instances racing for the same port and state directory,
# which is what actually produced the 401 (found the hard way, running
# this for real on Daytona).
