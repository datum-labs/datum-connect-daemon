# Sandbox image for the runnable Daytona version of ../../ec2-zero-trust-ssh/ --
# same 3-leg wiring as ../../tunnel-demo-daytona/sandbox/Dockerfile (sshd, a
# static web page, this daemon's own dashboard, all advertised outbound over
# Datum peer tunnels), just serving the "subway map" page instead of the
# generic placeholder. Deliberately its own image so it never risks the
# daytona-poc/ or tunnel-demo-daytona/ sandboxes.
#
# Same "immutable image, no secrets baked in" approach as the others: the
# only thing that varies between runs is $SSH_PUBKEY, injected as an env var
# at `daytona.create(...)` time. Key-only auth, no baked authorized_keys.
# Peer tunnels are project-agnostic, so fake-credentials-helper.sh (copied
# unmodified from ../../daytona-poc/) is all this image ever needs -- the
# laptop's home daemon is the only thing that holds real Datum project
# credentials, and it relays the site + dashboard legs out over Cloud
# tunnels from its own end.
#
# Build context expectations (see setup_demo.py): datum-connect-daemon and
# datumctl-connect are the same pre-built Linux binaries already used by
# ../../daytona-poc/ and ../../tunnel-demo-daytona/, and web/index.html is
# copied in from ../web/ -- all copied into this directory before the image
# build, not rebuilt here.
#
# ubuntu:24.04 to match the glibc version those binaries were built against.
FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    curl \
    ca-certificates \
    jq \
    openssh-server \
    python3 \
    && rm -rf /var/lib/apt/lists/* \
    && ssh-keygen -A \
    && useradd -m -s /bin/bash demo \
    && mkdir -p /home/demo/.ssh \
    && chmod 700 /home/demo/.ssh \
    && chown -R demo:demo /home/demo/.ssh \
    && sed -i \
         -e 's/#PasswordAuthentication yes/PasswordAuthentication no/' \
         -e 's/#PermitRootLogin.*/PermitRootLogin no/' \
         /etc/ssh/sshd_config \
    && mkdir -p /run/sshd

RUN mkdir -p /opt/datum /opt/datum/web
COPY datum-connect-daemon /opt/datum/datum-connect-daemon
COPY datumctl-connect /opt/datum/datumctl-connect
COPY fake-credentials-helper.sh /opt/datum/fake-credentials-helper.sh
COPY entrypoint.sh /opt/datum/entrypoint.sh
COPY web/index.html /opt/datum/web/index.html
RUN chmod +x /opt/datum/datum-connect-daemon /opt/datum/datumctl-connect \
    /opt/datum/fake-credentials-helper.sh /opt/datum/entrypoint.sh

# No ENTRYPOINT/CMD -- Daytona sandboxes are long-running environments
# exec'd into, not batch jobs whose image entrypoint runs once as the
# container's main process.
