# tblink

Keeps a two-Mac Thunderbolt RDMA link, and the [oMLX](https://github.com/jundot/omlx) cluster deployment that runs over it, working across cable moves and reboots. No manual `ifconfig`, no admin dialogs, no rebuilding deployments by hand.

## The problem

oMLX's JACCL backend runs tensor parallelism over Thunderbolt RDMA. Each RDMA port needs an IPv4 address. oMLX's link setup assigns it with `ifconfig`, which:

- is lost on reboot;
- stays on the **old** port when the cable moves. The stale copy then owns the subnet route, and the link silently breaks;
- leaves the port inside macOS's Thunderbolt Bridge (`bridge0`). In our testing, JACCL only came up once the active port was out of the bridge.

The deployment also records the RDMA device names (`rdma_en1`, `rdma_en6`, ...). When the cable moves to another port, the recorded deployment no longer matches the hardware.

## Quick start

You need two Apple Silicon Macs with oMLX installed, joined by one or two Thunderbolt cables, with RDMA enabled on both (once per Mac: start up in Recovery, open Terminal, run `rdma_ctl enable`, restart) and Remote Login on (System Settings > General > Sharing). The model must be at the same path on both Macs.

On **each** Mac, from a clone of this repository:

```sh
cargo build --release
./install.sh setup        # or: ./target/release/tblink setup
```

Answer "the other Mac" on the Mac that does not run the oMLX API, and "the coordinator" on the one that does. Setup detects this Mac's oMLX node id, its Thunderbolt ports and their speeds, and asks only for what it cannot detect: which Mac is the coordinator, the login on the other Mac, and the model. On the coordinator it measures each Mac's memory budget with oMLX (`node-budgets`), writes `~/.config/tblink/coordinator.json`, and offers to set up the SSH key. It installs the port daemon (asking for your password for `sudo`) and the coordinator agent.

Then, on the coordinator:

```sh
tblink doctor
```

It checks both Macs, the cables, the roles, the versions and oMLX, and says what to fix in plain words.

## What it does

One binary:

| Command | Runs as | Job |
|---|---|---|
| `tblink port` | root LaunchDaemon, **both** Macs | Every 5 s: picks the role of each RDMA-capable Thunderbolt port (see [Two cables](#two-cables)). Puts this Mac's RDMA address (`10.0.1.N/24`) and its bridge (management) address (`10.77.0.N/24`) on the ports chosen for them. Removes stale copies from other ports and from `bridge0`. Takes the ports in use out of `bridge0` and points each subnet route at its port. |
| `tblink coordinator` | user LaunchAgent, **coordinator Mac only** | Every 15 s: keeps the oMLX brew service up and bound to every configured address. Reads both Macs' ports (the peer's over SSH via `tblink status`). If the ports changed, it rebuilds the deployment, unloading the old one first, and loads it; while the old deployment is still live it waits until the new port state has held for `rebuild_settle_secs` (90 s), so a flapping cable cannot cause a rebuild storm. If JACCL fails to load twice, it falls back to TCP Ring over the bridge addresses and retries JACCL after 30 min. It logs which link inference uses. |
| `tblink setup` | logged-in user, both Macs | Guided setup (above). |
| `tblink doctor [--config PATH]` | logged-in user | Read-only checks of this Mac, and on the coordinator of the other Mac too: cables and speeds, roles, whether both Macs use the same cable for each role, whether the other Mac's addresses answer over the expected ports, tblink versions on both Macs, oMLX and the deployment. Exits 1 if anything is broken. |
| `tblink status [--json]` | anyone | Read-only view of each port: link speed, the Mac at the other end, role (inference / management / both / unused) and addresses, plus what the port daemon decided. `--json` is what the coordinator reads from the peer. |
| `tblink shutdown [--config PATH]` | coordinator Mac, logged-in user | Pauses the coordinator, unloads the model's deployment and stops the oMLX service, so the machines are free for other work. The port daemons keep running. |
| `tblink resume [--config PATH]` | coordinator Mac, logged-in user | Removes the pause; on its next check the coordinator starts oMLX and reloads the deployment. |
| `tblink version` | anyone | The version and the git commit it was built from. |

The bridge addresses move onto a Thunderbolt port, so SSH, Screen Sharing and API clients keep using `10.77.0.x` after the port leaves the bridge.

## Two cables

With two cables between the Macs (say a TB5 cable that negotiates 80 Gb/s and a TB4 cable at 40 Gb/s), the **fastest cable carries only inference**: the RDMA address `10.0.1.N` goes on it and the JACCL deployment uses its RDMA device. The other cable is the **management link**: the bridge address `10.77.0.N`, so SSH, API clients, model copies, the oMLX API and the Ring fallback all stay off the inference link. A third cable is left unused.

Both Macs must choose the same physical cable for each role, without talking to each other:

- **Ranking.** Cables are ranked by negotiated Thunderbolt speed (from `system_profiler SPThunderboltDataType`; "Thunderbolt N" in `networksetup -listallhardwareports` is receptacle N). Ties, such as two identical cables, are broken by a cable id: the pair of Thunderbolt domain UUIDs the cable joins. Both ends of a cable see the same speed and the same pair, so both Macs rank the cables the same way, whichever ports they are plugged into.
- **Failover.** If the inference cable goes away, the remaining cable carries both addresses at once (exactly the one-cable behaviour), and the coordinator rebuilds JACCL on it. When the faster cable comes back, the roles move back only after it has stayed up for 30 s; each time it drops again soon after, that wait doubles, up to 10 min.
- **Checking.** With two cables, each port daemon checks that the other Mac's RDMA address answers over the inference cable and its management address over the management cable (a TCP connect to port 22, bound to that port). If one answers and the other does not for 45 s, the other Mac evidently uses one cable for both (for example, the faster cable does not work at its end), and this Mac follows it; it moves back as soon as the other Mac does. The Mac with the lower RDMA address leads: it gives up such a fallback after 10 min (doubling each time) to try the dedicated link again, so the two Macs cannot hold each other in a fallback. If the other Mac never answers on port 22 (Remote Login off), the checks are skipped and the ranking alone decides.

`tblink status` shows the role of each port, and `tblink doctor` on the coordinator says whether both Macs agree. The coordinator log says which link inference uses, for example `inference link: en6 (80 Gb/s) here, en1 on Mac-B, dedicated; management on en1 (40 Gb/s)`.

## Install by hand

`tblink setup` runs these for you. To do it yourself:

```sh
cargo build --release

# On each Mac (addresses: own RDMA, own bridge, peer RDMA, peer bridge)
sudo ./install.sh port 10.0.1.1 10.77.0.1 10.0.1.2 10.77.0.2   # coordinator Mac
sudo ./install.sh port 10.0.1.2 10.77.0.2 10.0.1.1 10.77.0.1   # peer Mac

# On the coordinator Mac, as the logged-in user
cp examples/coordinator.json ~/.config/tblink/coordinator.json   # fill in node ids, peer login, model
./install.sh coordinator ~/.config/tblink/coordinator.json
```

Node IDs are in `~/.omlx/cluster/identity.json` on each Mac. `plan_request` is the body oMLX's `POST /admin/api/cluster/plan` takes. `capacity_bytes` should come from `POST /admin/api/cluster/node-budgets` while no model is loaded.

Logs: `/var/log/tblink.log` (port daemon) and `~/Library/Logs/tblink.log` (coordinator). The port daemon's last decision is in `/var/run/tblink.json`.

`--config` defaults to `~/.config/tblink/coordinator.json`. The pause is a file
named `paused` next to that config, so it survives reboots: a paused cluster
stays down until `tblink resume`.

Remove with `sudo ./install.sh uninstall-port` and `./install.sh uninstall-coordinator`.

## Upgrade

Install the same build on both Macs; `tblink doctor` warns when the versions differ. On each Mac, in the repository:

```sh
git pull && cargo build --release
sudo ./install.sh port <the same four addresses as before>   # replaces /usr/local/bin/tblink and restarts the daemon
```

and then on the coordinator Mac `./install.sh coordinator ~/.config/tblink/coordinator.json`, which restarts the coordinator on the new binary. `tblink setup` does the same if you run it again; it keeps your existing config's other settings.

## Notes

- oMLX's own link check (`link-status`) requires an ICMP echo in both directions. Endpoint-security network filters can drop inbound echo replies on new interfaces while TCP works fine. The coordinator therefore creates deployments directly and treats a successful load as the proof.
- Creating a deployment replaces the previous one for the same model, which is how oMLX stores them.
- The only secret read is the local oMLX admin API key from `settings.json`. It is never logged, printed or passed on a command line. `tblink setup` uses it only to ask the local oMLX for the memory budgets.
- The port daemon's checks of the other Mac are TCP connects to port 22 with `nc -b PORT`, so they go out of one specific port. Pass `--probe-port` to `tblink port` if SSH listens elsewhere.
- Built and tested on Apple M5 Pro Macs, macOS 27.2, oMLX 0.8.0.

## License

MIT
