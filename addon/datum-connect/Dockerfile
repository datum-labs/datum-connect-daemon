ARG BUILD_FROM
FROM ${BUILD_FROM}

# Deliberately NOT a multi-stage Rust build.
#
# The Supervisor builds a local add-on *on the device*. The Home Assistant
# Green has 4GB of RAM and eMMC storage, and the daemon is 565 crates
# including aws-lc-sys — a compile there is not something the box can do, and
# it is the reason this project builds arm64 in CI at all.
#
# So the binary is built ahead of time by
# .github/workflows/addon-daemon-arm64.yml, against bookworm's glibc and gated
# so it cannot link anything newer, and is simply copied in here. See DOCS.md
# for how to place it.
COPY bin/datum-connect-daemon /usr/bin/datum-connect-daemon
COPY rootfs/ /

RUN chmod a+x /usr/bin/datum-connect-daemon /usr/bin/run.sh

# The daemon is the main process so the Supervisor's restart-on-failure and
# the watchdog in config.yaml both act on the thing that actually matters.
CMD [ "/usr/bin/run.sh" ]
