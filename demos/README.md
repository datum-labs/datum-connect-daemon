# Demos

Three demos of `datum-connect-daemon`'s zero-inbound-firewall story, in increasing order of "runnable as-is":

| Demo | What it shows | Runnable? |
|---|---|---|
| [`ec2-zero-trust-ssh/`](./ec2-zero-trust-ssh/) | A server (originally EC2) reachable with zero inbound firewall rules: public traffic through a Datum Cloud tunnel, SSH through a direct P2P tunnel. | Conceptual template + "subway map" diagram — placeholders to fill in by hand, no scripts. |
| [`tunnel-demo-daytona/`](./tunnel-demo-daytona/) | The same 3-leg pattern (site, dashboard, SSH), but the whole thing — sandbox, tunnels, and teardown — is scripted end-to-end on an ephemeral [Daytona](https://daytona.io) sandbox. | Yes — `python setup_demo.py` / `python teardown_demo.py`. |
| [`ec2-zero-trust-ssh-daytona/`](./ec2-zero-trust-ssh-daytona/) | `ec2-zero-trust-ssh`'s actual "subway map" page, running for real on its own Daytona sandbox via the same automation as `tunnel-demo-daytona` — a second, independent instance (separate tunnel labels) that can run alongside it. | Yes — `python setup_demo.py` / `python teardown_demo.py`. |

All three tell the same story: public traffic is gated through Datum Cloud's proxy, admin/SSH traffic bypasses it entirely over a direct peer-to-peer tunnel — see [`ec2-zero-trust-ssh/README.md`](./ec2-zero-trust-ssh/README.md#the-story-in-one-line) for the full writeup. The two Daytona demos share one home relay daemon and differ only in which page the sandbox serves and which tunnel labels they use, so they don't collide when run at the same time — see each demo's own README for setup.
