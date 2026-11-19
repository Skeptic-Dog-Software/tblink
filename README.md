# tblink

Keeps a two-Mac Thunderbolt RDMA link, and the [oMLX](https://github.com/jundot/omlx) cluster deployment that runs over it, working across cable moves and reboots. No manual `ifconfig`, no admin dialogs, no rebuilding deployments by hand.

## The problem

oMLX's JACCL backend runs tensor parallelism over Thunderbolt RDMA. Each RDMA port needs an IPv4 address. oMLX's link setup assigns it with `ifconfig`, which:

- is lost on reboot;
- stays on the **old** port when the cable moves. The stale copy then owns the subnet route, and the link silently breaks;
- leaves the port inside macOS's Thunderbolt Bridge (`bridge0`). In our testing, JACCL only came up once the active port was out of the bridge.

The deployment also records the RDMA device names (`rdma_en1`, `rdma_en6`, ...). When the cable moves to another port, the recorded deployment no longer matches the hardware.

## What it does

One binary, three subcommands:

| Command | Runs as | Job |
|---|---|---|
| `tblink port` | root LaunchDaemon, **both** Macs | Every 5 s: finds the active RDMA-capable Thunderbolt port. Puts this Mac's RDMA address (`10.0.1.N/24`) and its bridge address (`10.77.0.N/24`) on that port. Removes stale copies from inactive ports and from `bridge0`. Takes the port out of `bridge0` and points the subnet routes at it. |
| `tblink coordinator` | user LaunchAgent, **coordinator Mac only** | Every 15 s: keeps the oMLX brew service up and bound to every configured address. Reads both Macs' active ports (the peer's over SSH via `tblink status`). If the ports changed, it rebuilds the deployment, unloading the old one first, and loads it. If JACCL fails to load twice, it falls back to TCP Ring over the bridge addresses and retries JACCL after 30 min. |
| `tblink status [--json]` | anyone | Read-only view of the ports, addresses and bridge membership. |

The bridge addresses move onto the RDMA port, so SSH, Screen Sharing and API clients keep using `10.77.0.x` after the port leaves the bridge.

## Install

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

Logs: `/var/log/tblink.log` (port daemon) and `~/Library/Logs/tblink.log` (coordinator).

Remove with `sudo ./install.sh uninstall-port` and `./install.sh uninstall-coordinator`.

## Notes

- oMLX's own link check (`link-status`) requires an ICMP echo in both directions. Endpoint-security network filters can drop inbound echo replies on new interfaces while TCP works fine. The coordinator therefore creates deployments directly and treats a successful load as the proof.
- Creating a deployment replaces the previous one for the same model, which is how oMLX stores them.
- The only secret read is the local oMLX admin API key from `settings.json`. It is never logged or passed on a command line.
- Built and tested on Apple M5 Pro Macs, macOS 27.2, oMLX 0.8.0.

## License

MIT
