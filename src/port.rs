//! `tblink port`: the per-Mac root daemon.
//!
//! Each tick it makes the link look the way the JACCL backend needs it:
//!   1. it picks the roles of the active RDMA-capable Thunderbolt ports (see
//!      `roles`): with one cable, that port carries this Mac's RDMA address
//!      (10.0.1.N/24) and, optionally, its old bridge address (10.77.0.N/24)
//!      so SSH, Screen Sharing and API clients keep working; with two, the
//!      fastest carries only the RDMA address and the other the bridge address;
//!   2. no other port still holds either address (a stale copy owns the
//!      subnet route and silently breaks the link);
//!   3. the ports in use are not bridge0 members, and bridge0 does not hold
//!      the bridge address while its members are gone;
//!   4. the subnet routes to the peer go out the port carrying that subnet.
//!
//! It only ever adds or removes these two addresses, removes the ports in use
//! from bridge0, and fixes those two routes. With more than one cable it also
//! checks, with a TCP connect bound to each port, that the peer's addresses
//! answer where this Mac expects them, and writes what it decided to
//! `net::STATE_PATH` for `tblink status`.

use crate::net::{self, DaemonState, Status, BRIDGE, IFCONFIG, ROUTE};
use crate::roles::{Controller, Layout, Probe, Tuning};
use crate::{log, sleep_secs};
use std::net::Ipv4Addr;
use std::time::Instant;

pub const NC: &str = "/usr/bin/nc";

#[derive(Debug, Clone, PartialEq)]
pub struct PortArgs {
    pub rdma_ip: String,
    pub bridge_ip: Option<String>,
    pub peer_ips: Vec<String>,
    pub interval: u64,
    pub once: bool,
    pub dry_run: bool,
    /// TCP port the peer answers on (Remote Login), for the reachability checks.
    pub probe_port: u16,
}

/// One planned change, as the command that makes it.
#[derive(Debug, PartialEq)]
pub struct Action {
    pub cmd: &'static str,
    pub args: Vec<String>,
    pub why: String,
}

fn act(cmd: &'static str, args: &[&str], why: String) -> Action {
    Action { cmd, args: args.iter().map(|s| s.to_string()).collect(), why }
}

pub fn subnet24(addr: &str) -> String {
    let mut o: Vec<&str> = addr.split('.').take(3).collect();
    o.push("0");
    format!("{}/24", o.join("."))
}

/// The peer's address in the same /24 as `ours`.
pub fn peer_in_subnet<'a>(a: &'a PortArgs, ours: Option<&str>) -> Option<&'a str> {
    let net = subnet24(ours?);
    a.peer_ips.iter().find(|p| subnet24(p) == net).map(String::as_str)
}

/// The lower RDMA address leads when the Macs must break a symmetry (see `roles`).
pub fn is_leader(a: &PortArgs) -> bool {
    let me = a.rdma_ip.parse::<Ipv4Addr>().ok();
    let peer = peer_in_subnet(a, Some(&a.rdma_ip)).and_then(|p| p.parse::<Ipv4Addr>().ok());
    match (me, peer) {
        (Some(m), Some(p)) => m < p,
        _ => true,
    }
}

/// Pure planning step, so it can be unit-tested against fixed snapshots.
pub fn plan_for(st: &Status, a: &PortArgs, layout: &Layout, routes: &dyn Fn(&str) -> Option<String>) -> Vec<Action> {
    let mut out = Vec::new();
    let (Some(rdma_port), Some(bridge_port)) = (layout.rdma_port(), layout.bridge_port()) else { return out };
    let port = |name: &str| st.ports.iter().find(|p| p.name == name);
    // Each of our addresses and the port it belongs on.
    let ours: Vec<(&str, &str)> =
        std::iter::once((a.rdma_ip.as_str(), rdma_port)).chain(a.bridge_ip.as_deref().map(|b| (b, bridge_port))).collect();

    for p in &st.ports {
        for (addr, target) in &ours {
            if p.name != *target && p.inet.iter().any(|x| x == addr) {
                let why = if p.active {
                    format!("{addr} moves from {} to {target}", p.name)
                } else {
                    format!("stale {addr} on inactive {}", p.name)
                };
                out.push(act(IFCONFIG, &[&p.name, "inet", addr, "-alias"], why));
            }
        }
    }
    for name in layout.ports() {
        if port(name).is_some_and(|p| p.bridge_member) {
            out.push(act(IFCONFIG, &[BRIDGE, "deletem", name], format!("take {name} out of {BRIDGE}")));
        }
    }
    if let Some(b) = a.bridge_ip.as_deref() {
        if st.bridge_inet.iter().any(|x| x == b) {
            out.push(act(IFCONFIG, &[BRIDGE, "inet", b, "-alias"], format!("{b} moves from {BRIDGE} to {bridge_port}")));
        }
    }
    let mut added_nets = Vec::new();
    for (addr, target) in &ours {
        if !port(target).is_some_and(|p| p.inet.iter().any(|x| x == addr)) {
            out.push(act(IFCONFIG, &[target, "inet", addr, "netmask", "255.255.255.0", "alias"], format!("add {addr} to {target}")));
            added_nets.push(subnet24(addr));
        }
    }
    // Adding an address installs its subnet route; judge routes on the next tick.
    for peer in a.peer_ips.iter().filter(|p| !added_nets.contains(&subnet24(p))) {
        let net = subnet24(peer);
        let want = ours.iter().find(|(addr, _)| subnet24(addr) == net).map(|(_, t)| *t).unwrap_or(rdma_port);
        match routes(peer) {
            Some(ifc) if ifc == want => {}
            other => {
                if other.is_some() {
                    out.push(act(
                        ROUTE,
                        &["-n", "delete", "-net", &net],
                        format!("route to {peer} uses {}", other.as_deref().unwrap_or("?")),
                    ));
                }
                out.push(act(ROUTE, &["-n", "add", "-net", &net, "-interface", want], format!("route {net} via {want}")));
            }
        }
    }
    out
}

/// Our addresses and routes are where the plan wants them (bridge0 cleanup
/// alone does not stop the peer checks: macOS may keep re-adding a port).
pub fn addresses_settled(actions: &[Action]) -> bool {
    actions.iter().all(|x| x.args.get(1).map(String::as_str) == Some("deletem"))
}

/// Whether an `nc -z` run reached the peer: connected, or refused by its kernel.
pub fn probe_answered(ok: bool, out: &str) -> bool {
    ok || out.contains("Connection refused")
}

/// Does `addr` answer a TCP connect sent out of `port` (and only `port`)?
fn probe_one(port: &str, addr: &str, tcp: u16) -> bool {
    let (ok, out) = net::run(NC, &["-z", "-v", "-G", "1", "-b", port, addr, &tcp.to_string()]);
    probe_answered(ok, &out)
}

/// Check the peer's addresses over the ports `layout` puts ours on.
pub fn probe_layout(layout: &Layout, a: &PortArgs, check: &mut dyn FnMut(&str, &str) -> bool) -> Probe {
    let rdma = layout.rdma_port().zip(peer_in_subnet(a, Some(&a.rdma_ip))).map(|(p, ip)| check(p, ip));
    let bridge = layout.bridge_port().zip(peer_in_subnet(a, a.bridge_ip.as_deref())).map(|(p, ip)| check(p, ip));
    Probe { rdma, bridge }
}

/// The active ports, as one comma-separated name (for the change log line).
pub fn active_names(st: &Status) -> Option<String> {
    let v: Vec<&str> = st.ports.iter().filter(|p| p.active).map(|p| p.name.as_str()).collect();
    (!v.is_empty()).then(|| v.join(", "))
}

/// The log line for a change in which port is active, or `None` if unchanged.
pub fn port_change_message(last: &Option<String>, now: &Option<String>) -> Option<String> {
    if last == now {
        return None;
    }
    Some(match now {
        Some(n) => format!("active Thunderbolt RDMA port: {n}"),
        None => "no active Thunderbolt RDMA port (cable unplugged?)".to_string(),
    })
}

/// Runs one command and returns (success, combined output), like `net::run`.
pub type Runner<'a> = dyn FnMut(&str, &[&str]) -> (bool, String) + 'a;

/// Carry out `actions` with `runner` (or only describe them on a dry run) and
/// return one log line per action. Every action is attempted even if an earlier
/// one fails; the next tick re-plans from the resulting state.
pub fn apply(actions: &[Action], dry_run: bool, runner: &mut Runner) -> Vec<String> {
    actions
        .iter()
        .map(|x| {
            if dry_run {
                return format!("would run: {} {}   ({})", x.cmd, x.args.join(" "), x.why);
            }
            let args: Vec<&str> = x.args.iter().map(String::as_str).collect();
            let (ok, out) = runner(x.cmd, &args);
            format!(
                "{} {}: {}  ({}){}",
                x.cmd,
                x.args.join(" "),
                if ok { "ok" } else { "FAILED" },
                x.why,
                if ok { String::new() } else { format!(": {}", out.trim()) }
            )
        })
        .collect()
}

pub fn main(a: PortArgs) -> i32 {
    if !a.dry_run && unsafe_geteuid() != 0 {
        eprintln!("tblink port must run as root (or pass --dry-run)");
        return 1;
    }
    let leader = is_leader(&a);
    let mut ctl = Controller::new(leader, a.bridge_ip.is_some(), Tuning::default());
    let clock = Instant::now();
    let mut last_active: Option<String> = None;
    let mut settled = false;
    log(&format!(
        "tblink {} port daemon: rdma {}, management {}, {}",
        net::version(),
        a.rdma_ip,
        a.bridge_ip.as_deref().unwrap_or("none"),
        if leader { "leader" } else { "follower" }
    ));
    loop {
        let st = net::port_status();
        let now_active = active_names(&st);
        if let Some(m) = port_change_message(&last_active, &now_active) {
            log(&m);
        }
        last_active = now_active;
        let links = net::links(&st);
        // Only judge the peer once our own addresses have settled where the layout puts them.
        let probe = if settled && links.len() >= 2 {
            probe_layout(&ctl.layout, &a, &mut |port, ip| probe_one(port, ip, a.probe_port))
        } else {
            Probe::default()
        };
        let now = clock.elapsed().as_secs();
        for line in ctl.step(&links, probe, now) {
            log(&line);
        }
        // Planned against a fresh route view: route state can lag the ifconfig change.
        let actions = plan_for(&st, &a, &ctl.layout, &|p| net::route_interface(p));
        settled = addresses_settled(&actions);
        for line in apply(&actions, a.dry_run, &mut |c, args| net::run(c, args)) {
            log(&line);
        }
        let state = DaemonState {
            version: net::version(),
            rdma_ip: a.rdma_ip.clone(),
            bridge_ip: a.bridge_ip.clone(),
            peer_ips: a.peer_ips.clone(),
            layout: ctl.layout.clone(),
            adapt: ctl.adapt.clone(),
            pending: ctl.pending(now),
            probe,
            leader,
            updated: net::unix_now(),
            age_secs: None,
        };
        if a.dry_run {
            if a.once {
                println!("{}", serde_json::to_string_pretty(&state).unwrap_or_default());
            }
        } else if let Err(e) = write_state(std::path::Path::new(net::STATE_PATH), &state) {
            log(&format!("cannot write {}: {e}", net::STATE_PATH));
        }
        if a.once {
            return 0;
        }
        sleep_secs(a.interval);
    }
}

/// Replace the state file in one step, so readers never see half of it.
pub fn write_state(path: &std::path::Path, state: &DaemonState) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(state).unwrap_or_default())?;
    std::fs::rename(&tmp, path)
}

fn unsafe_geteuid() -> u32 {
    extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::Port;
    use std::cell::RefCell;

    fn port(name: &str, active: bool, member: bool, inet: &[&str]) -> Port {
        Port {
            name: name.into(),
            rdma_device: format!("rdma_{name}"),
            active,
            bridge_member: member,
            inet: inet.iter().map(|s| s.to_string()).collect(),
            ..Port::default()
        }
    }
    fn args() -> PortArgs {
        PortArgs {
            rdma_ip: "10.0.1.1".into(),
            bridge_ip: Some("10.77.0.1".into()),
            peer_ips: vec!["10.0.1.2".into()],
            interval: 5,
            once: true,
            dry_run: true,
            probe_port: 22,
        }
    }
    /// The single-cable plan: everything on the active port, as before two-cable support.
    fn plan(st: &Status, a: &PortArgs, routes: &dyn Fn(&str) -> Option<String>) -> Vec<Action> {
        let layout = st.active.as_ref().map(|p| Layout::Single { port: p.name.clone() }).unwrap_or_default();
        plan_for(st, a, &layout, routes)
    }
    fn args_peers(peers: &[&str]) -> PortArgs {
        PortArgs { peer_ips: peers.iter().map(|s| s.to_string()).collect(), ..args() }
    }
    fn st(ports: Vec<Port>, bridge_inet: &[&str]) -> Status {
        Status {
            hostname: "t".into(),
            active: ports.iter().find(|p| p.active).cloned(),
            bridge_members: ports.iter().filter(|p| p.bridge_member).map(|p| p.name.clone()).collect(),
            ports,
            bridge_inet: bridge_inet.iter().map(|s| s.to_string()).collect(),
            ..Status::default()
        }
    }
    fn cmds(v: &[Action]) -> Vec<String> {
        v.iter().map(|a| a.args.join(" ")).collect()
    }
    fn routes_via(ifc: &'static str) -> impl Fn(&str) -> Option<String> {
        move |_| Some(ifc.to_string())
    }
    /// The settled state on the coordinator side.
    fn settled() -> Status {
        st(vec![port("en1", false, true, &[]), port("en6", true, false, &["10.0.1.1", "10.77.0.1"])], &[])
    }

    // ---- subnet24 ----

    #[test]
    fn subnet24_of_addresses() {
        assert_eq!(subnet24("10.0.1.2"), "10.0.1.0/24");
        assert_eq!(subnet24("10.77.0.1"), "10.77.0.0/24");
        assert_eq!(subnet24("192.168.8.217"), "192.168.8.0/24");
    }

    #[test]
    fn subnet24_groups_same_subnet() {
        assert_eq!(subnet24("10.0.1.1"), subnet24("10.0.1.254"));
        assert_ne!(subnet24("10.0.1.1"), subnet24("10.0.2.1"));
    }

    // ---- plan: whole scenarios ----

    #[test]
    fn cable_moved_after_link_setup() {
        // Observed after a cable move: address left on en1, cable now on en6, en6 in the bridge.
        let s = st(vec![port("en1", false, true, &["10.0.1.1"]), port("en6", true, true, &[])], &["10.77.0.1"]);
        let v = plan(&s, &args(), &routes_via("en1"));
        assert_eq!(
            cmds(&v),
            vec![
                "en1 inet 10.0.1.1 -alias",
                "bridge0 deletem en6",
                "bridge0 inet 10.77.0.1 -alias",
                "en6 inet 10.0.1.1 netmask 255.255.255.0 alias",
                "en6 inet 10.77.0.1 netmask 255.255.255.0 alias",
            ]
        );
    }

    #[test]
    fn stale_route_is_replaced() {
        assert_eq!(
            cmds(&plan(&settled(), &args(), &routes_via("en1"))),
            vec!["-n delete -net 10.0.1.0/24", "-n add -net 10.0.1.0/24 -interface en6",]
        );
    }

    #[test]
    fn settled_link_is_left_alone() {
        assert!(plan(&settled(), &args(), &routes_via("en6")).is_empty());
    }

    #[test]
    fn unplugged_does_nothing() {
        let s = st(vec![port("en1", false, true, &["10.0.1.1"]), port("en6", false, false, &[])], &[]);
        assert!(plan(&s, &args(), &|_| None).is_empty());
    }

    #[test]
    fn unplugged_ignores_even_a_bridge_address() {
        // With no active port there is nowhere to move anything to.
        let s = st(vec![port("en1", false, true, &[])], &["10.77.0.1"]);
        assert!(plan(&s, &args(), &routes_via("bridge0")).is_empty());
    }

    #[test]
    fn no_rdma_ports_does_nothing() {
        assert!(plan(&st(vec![], &["10.77.0.1"]), &args(), &|_| None).is_empty());
    }

    #[test]
    fn missing_route_is_added_without_delete() {
        let s = st(vec![port("en6", true, false, &["10.0.1.1", "10.77.0.1"])], &[]);
        assert_eq!(cmds(&plan(&s, &args(), &|_| None)), vec!["-n add -net 10.0.1.0/24 -interface en6"]);
    }

    #[test]
    fn fresh_boot_with_port_in_bridge() {
        // After a reboot: no addresses anywhere, active port still a bridge member.
        let s = st(vec![port("en1", true, true, &[]), port("en2", false, true, &[])], &[]);
        assert_eq!(
            cmds(&plan(&s, &args(), &|_| None)),
            vec!["bridge0 deletem en1", "en1 inet 10.0.1.1 netmask 255.255.255.0 alias", "en1 inet 10.77.0.1 netmask 255.255.255.0 alias",]
        );
    }

    // ---- plan: stale addresses ----

    #[test]
    fn both_addresses_stale_on_one_inactive_port() {
        let s = st(vec![port("en1", false, false, &["10.0.1.1", "10.77.0.1"]), port("en6", true, false, &["10.0.1.1", "10.77.0.1"])], &[]);
        assert_eq!(cmds(&plan(&s, &args(), &routes_via("en6"))), vec!["en1 inet 10.0.1.1 -alias", "en1 inet 10.77.0.1 -alias",]);
    }

    #[test]
    fn stale_copies_on_several_inactive_ports_are_all_removed() {
        let s = st(
            vec![
                port("en1", false, false, &["10.0.1.1"]),
                port("en2", false, false, &["10.77.0.1"]),
                port("en6", true, false, &["10.0.1.1", "10.77.0.1"]),
            ],
            &[],
        );
        assert_eq!(cmds(&plan(&s, &args(), &routes_via("en6"))), vec!["en1 inet 10.0.1.1 -alias", "en2 inet 10.77.0.1 -alias",]);
    }

    #[test]
    fn unrelated_addresses_are_never_touched() {
        // Only this Mac's two configured addresses are managed; anything else stays.
        let s = st(
            vec![
                port("en1", false, false, &["10.0.1.2", "169.254.4.123"]),
                port("en6", true, false, &["10.0.1.1", "10.77.0.1", "192.168.50.9"]),
            ],
            &["10.77.0.2"],
        );
        assert!(plan(&s, &args(), &routes_via("en6")).is_empty());
    }

    #[test]
    fn stale_action_explains_itself() {
        let s = st(vec![port("en1", false, false, &["10.0.1.1"]), port("en6", true, false, &["10.0.1.1", "10.77.0.1"])], &[]);
        let v = plan(&s, &args(), &routes_via("en6"));
        assert_eq!(v[0].cmd, IFCONFIG);
        assert_eq!(v[0].why, "stale 10.0.1.1 on inactive en1");
    }

    // ---- plan: bridge handling ----

    #[test]
    fn active_port_leaves_the_bridge_even_when_addresses_are_right() {
        let s = st(vec![port("en6", true, true, &["10.0.1.1", "10.77.0.1"])], &[]);
        let v = plan(&s, &args(), &routes_via("en6"));
        assert_eq!(cmds(&v), vec!["bridge0 deletem en6"]);
        assert_eq!(v[0].why, "take en6 out of bridge0");
    }

    #[test]
    fn inactive_bridge_members_are_left_in_the_bridge() {
        let s = st(
            vec![port("en1", false, true, &[]), port("en2", false, true, &[]), port("en6", true, false, &["10.0.1.1", "10.77.0.1"])],
            &[],
        );
        assert!(plan(&s, &args(), &routes_via("en6")).is_empty());
    }

    #[test]
    fn bridge_address_moves_off_bridge0() {
        let s = st(vec![port("en6", true, false, &["10.0.1.1", "10.77.0.1"])], &["10.77.0.1"]);
        let v = plan(&s, &args(), &routes_via("en6"));
        assert_eq!(cmds(&v), vec!["bridge0 inet 10.77.0.1 -alias"]);
        assert_eq!(v[0].why, "10.77.0.1 moves from bridge0 to en6");
    }

    #[test]
    fn other_addresses_on_bridge0_stay() {
        let s = st(vec![port("en6", true, false, &["10.0.1.1", "10.77.0.1"])], &["10.77.0.2", "169.254.197.248"]);
        assert!(plan(&s, &args(), &routes_via("en6")).is_empty());
    }

    // ---- plan: without a bridge address ----

    #[test]
    fn rdma_only_mode_adds_just_the_rdma_address() {
        let a = PortArgs { bridge_ip: None, ..args() };
        let s = st(vec![port("en6", true, false, &[])], &[]);
        assert_eq!(cmds(&plan(&s, &a, &|_| None)), vec!["en6 inet 10.0.1.1 netmask 255.255.255.0 alias"]);
    }

    #[test]
    fn rdma_only_mode_leaves_bridge_addresses_alone() {
        let a = PortArgs { bridge_ip: None, ..args() };
        let s = st(vec![port("en1", false, false, &["10.77.0.1"]), port("en6", true, false, &["10.0.1.1"])], &["10.77.0.1"]);
        assert!(plan(&s, &a, &routes_via("en6")).is_empty());
    }

    #[test]
    fn rdma_only_mode_still_takes_the_port_out_of_the_bridge() {
        let a = PortArgs { bridge_ip: None, ..args() };
        let s = st(vec![port("en6", true, true, &["10.0.1.1"])], &[]);
        assert_eq!(cmds(&plan(&s, &a, &routes_via("en6"))), vec!["bridge0 deletem en6"]);
    }

    // ---- plan: adding addresses ----

    #[test]
    fn only_the_missing_address_is_added() {
        let s = st(vec![port("en6", true, false, &["10.0.1.1"])], &[]);
        assert_eq!(cmds(&plan(&s, &args(), &routes_via("en6"))), vec!["en6 inet 10.77.0.1 netmask 255.255.255.0 alias"]);
    }

    #[test]
    fn added_address_uses_a_slash_24_alias() {
        let s = st(vec![port("en6", true, false, &["10.77.0.1"])], &[]);
        let v = plan(&s, &args(), &|_| None);
        assert_eq!(v[0].cmd, IFCONFIG);
        assert_eq!(v[0].args, vec!["en6", "inet", "10.0.1.1", "netmask", "255.255.255.0", "alias"]);
        assert_eq!(v[0].why, "add 10.0.1.1 to en6");
    }

    // ---- plan: routes ----

    #[test]
    fn route_check_is_skipped_for_a_subnet_just_added() {
        // Adding 10.0.1.1 installs 10.0.1.0/24; checking the route now would read stale state.
        let s = st(vec![port("en6", true, false, &["10.77.0.1"])], &[]);
        let asked = RefCell::new(Vec::new());
        let v = plan(&s, &args(), &|p| {
            asked.borrow_mut().push(p.to_string());
            Some("en1".into())
        });
        assert!(asked.borrow().is_empty());
        assert_eq!(cmds(&v), vec!["en6 inet 10.0.1.1 netmask 255.255.255.0 alias"]);
    }

    #[test]
    fn routes_for_each_peer_subnet() {
        let a = args_peers(&["10.0.1.2", "10.77.0.2"]);
        let v = plan(&settled(), &a, &routes_via("bridge0"));
        assert_eq!(
            cmds(&v),
            vec![
                "-n delete -net 10.0.1.0/24",
                "-n add -net 10.0.1.0/24 -interface en6",
                "-n delete -net 10.77.0.0/24",
                "-n add -net 10.77.0.0/24 -interface en6",
            ]
        );
    }

    #[test]
    fn only_the_wrong_route_is_fixed() {
        let a = args_peers(&["10.0.1.2", "10.77.0.2"]);
        let routes = |p: &str| Some(if p.starts_with("10.0.1.") { "en6" } else { "en9" }.to_string());
        assert_eq!(cmds(&plan(&settled(), &a, &routes)), vec!["-n delete -net 10.77.0.0/24", "-n add -net 10.77.0.0/24 -interface en6",]);
    }

    #[test]
    fn route_actions_explain_the_old_interface() {
        let v = plan(&settled(), &args(), &routes_via("en9"));
        assert_eq!(v[0].cmd, ROUTE);
        assert_eq!(v[0].why, "route to 10.0.1.2 uses en9");
        assert_eq!(v[1].why, "route 10.0.1.0/24 via en6");
    }

    #[test]
    fn no_peers_means_no_route_work() {
        let a = args_peers(&[]);
        let asked = RefCell::new(0);
        assert!(plan(&settled(), &a, &|_| {
            *asked.borrow_mut() += 1;
            Some("en1".into())
        })
        .is_empty());
        assert_eq!(*asked.borrow(), 0);
    }

    #[test]
    fn route_is_looked_up_per_peer_address() {
        let a = args_peers(&["10.0.1.2", "10.77.0.2"]);
        let asked = RefCell::new(Vec::new());
        plan(&settled(), &a, &|p| {
            asked.borrow_mut().push(p.to_string());
            Some("en6".into())
        });
        assert_eq!(*asked.borrow(), vec!["10.0.1.2", "10.77.0.2"]);
    }

    #[test]
    fn route_check_skips_only_the_newly_added_subnet() {
        // 10.77.0.1 is being added; the 10.0.1.x route is still checked.
        let a = args_peers(&["10.0.1.2", "10.77.0.2"]);
        let s = st(vec![port("en6", true, false, &["10.0.1.1"])], &[]);
        let asked = RefCell::new(Vec::new());
        plan(&s, &a, &|p| {
            asked.borrow_mut().push(p.to_string());
            Some("en6".into())
        });
        assert_eq!(*asked.borrow(), vec!["10.0.1.2"]);
    }

    // ---- plan: ordering and idempotence ----

    #[test]
    fn actions_run_in_a_safe_order() {
        // Stale copies go first (they own the route), then bridge cleanup, then adds, then routes.
        let a = args_peers(&["10.0.1.2", "10.77.0.2"]);
        let s = st(vec![port("en1", false, true, &["10.0.1.1"]), port("en6", true, true, &["10.77.0.1"])], &["10.77.0.1"]);
        let v = plan(&s, &a, &routes_via("en1"));
        assert_eq!(
            cmds(&v),
            vec![
                "en1 inet 10.0.1.1 -alias",
                "bridge0 deletem en6",
                "bridge0 inet 10.77.0.1 -alias",
                "en6 inet 10.0.1.1 netmask 255.255.255.0 alias",
                "-n delete -net 10.77.0.0/24",
                "-n add -net 10.77.0.0/24 -interface en6",
            ]
        );
    }

    #[test]
    fn plan_converges_once_applied() {
        // Simulate applying the plan to the snapshot; the next tick must plan nothing.
        let s = st(vec![port("en1", false, true, &["10.0.1.1"]), port("en6", true, true, &[])], &["10.77.0.1"]);
        assert!(!plan(&s, &args(), &routes_via("en1")).is_empty());
        let after = st(vec![port("en1", false, true, &[]), port("en6", true, false, &["10.0.1.1", "10.77.0.1"])], &[]);
        assert!(plan(&after, &args(), &routes_via("en6")).is_empty());
    }

    #[test]
    fn plan_is_deterministic() {
        let s = st(vec![port("en1", false, true, &["10.0.1.1"]), port("en6", true, true, &[])], &["10.77.0.1"]);
        assert_eq!(plan(&s, &args(), &routes_via("en1")), plan(&s, &args(), &routes_via("en1")));
    }

    #[test]
    fn every_action_uses_an_absolute_tool_path() {
        let a = args_peers(&["10.0.1.2", "10.77.0.2"]);
        let s = st(vec![port("en1", false, true, &["10.0.1.1"]), port("en6", true, true, &[])], &["10.77.0.1"]);
        for x in plan(&s, &a, &routes_via("en1")).iter().chain(plan(&settled(), &a, &routes_via("en1")).iter()) {
            assert!(x.cmd == "/sbin/ifconfig" || x.cmd == "/sbin/route", "{}", x.cmd);
        }
    }

    // ---- plan_for: two cables ----

    fn split(i: &str, m: &str) -> Layout {
        Layout::Split { inference: i.into(), management: m.into() }
    }
    fn two_peers() -> PortArgs {
        args_peers(&["10.0.1.2", "10.77.0.2"])
    }
    /// Routes as the kernel would have them once the addresses are where `split(i, m)` puts them.
    fn routes_split(i: &'static str, m: &'static str) -> impl Fn(&str) -> Option<String> {
        move |p: &str| Some(if p.starts_with("10.0.1.") { i } else { m }.to_string())
    }

    #[test]
    fn second_cable_takes_over_management() {
        // One cable (en2) had both; a faster one on en1 becomes the inference link.
        let s = st(vec![port("en1", true, true, &[]), port("en2", true, false, &["10.0.1.1", "10.77.0.1"])], &[]);
        let v = plan_for(&s, &two_peers(), &split("en1", "en2"), &routes_via("en2"));
        assert_eq!(cmds(&v), vec!["en2 inet 10.0.1.1 -alias", "bridge0 deletem en1", "en1 inet 10.0.1.1 netmask 255.255.255.0 alias"]);
        assert_eq!(v[0].why, "10.0.1.1 moves from en2 to en1");
    }

    #[test]
    fn settled_split_is_left_alone() {
        let s = st(vec![port("en1", true, false, &["10.0.1.1"]), port("en2", true, false, &["10.77.0.1"])], &[]);
        assert!(plan_for(&s, &two_peers(), &split("en1", "en2"), &routes_split("en1", "en2")).is_empty());
    }

    #[test]
    fn split_routes_each_subnet_over_its_own_cable() {
        let s = st(vec![port("en1", true, false, &["10.0.1.1"]), port("en2", true, false, &["10.77.0.1"])], &[]);
        // Both routes still point at en2 from the single-cable days.
        let v = plan_for(&s, &two_peers(), &split("en1", "en2"), &routes_via("en2"));
        assert_eq!(cmds(&v), vec!["-n delete -net 10.0.1.0/24", "-n add -net 10.0.1.0/24 -interface en1"]);
    }

    #[test]
    fn failover_puts_both_addresses_on_the_remaining_cable() {
        // The inference cable (en1) went away; its address is stale there.
        let s = st(vec![port("en1", false, false, &["10.0.1.1"]), port("en2", true, false, &["10.77.0.1"])], &[]);
        let v = plan_for(&s, &two_peers(), &Layout::Single { port: "en2".into() }, &routes_via("en2"));
        assert_eq!(cmds(&v), vec!["en1 inet 10.0.1.1 -alias", "en2 inet 10.0.1.1 netmask 255.255.255.0 alias"]);
        assert_eq!(v[0].why, "stale 10.0.1.1 on inactive en1");
    }

    #[test]
    fn swapping_roles_moves_both_addresses() {
        let s = st(vec![port("en1", true, false, &["10.0.1.1"]), port("en2", true, false, &["10.77.0.1"])], &[]);
        let v = plan_for(&s, &two_peers(), &split("en2", "en1"), &routes_split("en1", "en2"));
        assert_eq!(
            cmds(&v),
            vec![
                "en1 inet 10.0.1.1 -alias",
                "en2 inet 10.77.0.1 -alias",
                "en2 inet 10.0.1.1 netmask 255.255.255.0 alias",
                "en1 inet 10.77.0.1 netmask 255.255.255.0 alias",
            ]
        );
    }

    #[test]
    fn split_takes_both_ports_out_of_the_bridge_and_moves_the_bridge_address() {
        let s = st(vec![port("en1", true, true, &[]), port("en2", true, true, &[])], &["10.77.0.1"]);
        let v = plan_for(&s, &two_peers(), &split("en1", "en2"), &|_| None);
        assert_eq!(
            cmds(&v),
            vec![
                "bridge0 deletem en1",
                "bridge0 deletem en2",
                "bridge0 inet 10.77.0.1 -alias",
                "en1 inet 10.0.1.1 netmask 255.255.255.0 alias",
                "en2 inet 10.77.0.1 netmask 255.255.255.0 alias",
            ]
        );
        assert_eq!(v[2].why, "10.77.0.1 moves from bridge0 to en2");
    }

    #[test]
    fn split_converges_once_applied() {
        let before = st(
            vec![port("en1", true, true, &[]), port("en2", true, false, &["10.0.1.1", "10.77.0.1"]), port("en6", false, true, &[])],
            &[],
        );
        assert!(!plan_for(&before, &two_peers(), &split("en1", "en2"), &routes_via("en2")).is_empty());
        let after =
            st(vec![port("en1", true, false, &["10.0.1.1"]), port("en2", true, false, &["10.77.0.1"]), port("en6", false, true, &[])], &[]);
        assert!(plan_for(&after, &two_peers(), &split("en1", "en2"), &routes_split("en1", "en2")).is_empty());
    }

    #[test]
    fn third_port_is_cleaned_but_unused() {
        let s = st(
            vec![
                port("en1", true, false, &["10.0.1.1"]),
                port("en2", true, false, &["10.77.0.1"]),
                port("en6", true, true, &["10.77.0.1"]),
            ],
            &[],
        );
        let v = plan_for(&s, &two_peers(), &split("en1", "en2"), &routes_split("en1", "en2"));
        assert_eq!(cmds(&v), vec!["en6 inet 10.77.0.1 -alias"], "en6 stays in the bridge: it is not in use");
    }

    #[test]
    fn no_layout_plans_nothing() {
        assert!(plan_for(&settled(), &args(), &Layout::None, &routes_via("en1")).is_empty());
    }

    #[test]
    fn single_layout_matches_the_old_active_port_plan() {
        // plan() in these tests is the single-cable plan; it must equal plan_for with Single(active).
        let s = st(vec![port("en1", false, true, &["10.0.1.1"]), port("en6", true, true, &[])], &["10.77.0.1"]);
        let a = two_peers();
        assert_eq!(plan(&s, &a, &routes_via("en1")), plan_for(&s, &a, &Layout::Single { port: "en6".into() }, &routes_via("en1")));
    }

    // ---- peers, leader, probes ----

    #[test]
    fn peer_addresses_by_subnet() {
        let a = two_peers();
        assert_eq!(peer_in_subnet(&a, Some("10.0.1.1")), Some("10.0.1.2"));
        assert_eq!(peer_in_subnet(&a, Some("10.77.0.1")), Some("10.77.0.2"));
        assert_eq!(peer_in_subnet(&a, Some("192.168.1.1")), None);
        assert_eq!(peer_in_subnet(&a, None), None);
    }

    #[test]
    fn lower_rdma_address_leads() {
        assert!(is_leader(&two_peers()));
        let b = PortArgs {
            rdma_ip: "10.0.1.2".into(),
            bridge_ip: Some("10.77.0.2".into()),
            peer_ips: vec!["10.0.1.1".into(), "10.77.0.1".into()],
            ..args()
        };
        assert!(!is_leader(&b));
        // Numeric, not text, order.
        let c = PortArgs { rdma_ip: "10.0.1.9".into(), peer_ips: vec!["10.0.1.10".into()], ..args() };
        assert!(is_leader(&c));
        assert!(is_leader(&args_peers(&[])), "no peer known: lead");
    }

    #[test]
    fn probe_answer_rules() {
        assert!(probe_answered(true, "Connection to 10.0.1.2 port 22 [tcp/ssh] succeeded!"));
        assert!(probe_answered(false, "nc: connectx to 10.0.1.2 port 22 (tcp) failed: Connection refused"));
        assert!(!probe_answered(false, "nc: connectx to 10.0.1.1 port 9 (tcp) failed: Operation timed out"));
        assert!(!probe_answered(false, "nc: connectx to 10.0.1.1 port 22 (tcp) failed: Network is unreachable"));
    }

    #[test]
    fn probes_go_out_of_the_port_each_address_lives_on() {
        let mut asked = Vec::new();
        let p = probe_layout(&split("en1", "en2"), &two_peers(), &mut |port, ip| {
            asked.push(format!("{ip} via {port}"));
            port == "en1"
        });
        assert_eq!(asked, vec!["10.0.1.2 via en1", "10.77.0.2 via en2"]);
        assert_eq!(p, Probe { rdma: Some(true), bridge: Some(false) });
    }

    #[test]
    fn probes_without_known_peer_addresses_are_unknown() {
        let p = probe_layout(&split("en1", "en2"), &args_peers(&[]), &mut |_, _| panic!("must not probe"));
        assert_eq!(p, Probe::default());
        let p = probe_layout(&Layout::None, &two_peers(), &mut |_, _| panic!("must not probe"));
        assert_eq!(p, Probe::default());
    }

    #[test]
    fn bridge_cleanup_alone_counts_as_settled() {
        assert!(addresses_settled(&[]));
        let s = st(vec![port("en1", true, true, &["10.0.1.1"]), port("en2", true, false, &["10.77.0.1"])], &[]);
        let v = plan_for(&s, &two_peers(), &split("en1", "en2"), &routes_split("en1", "en2"));
        assert_eq!(cmds(&v), vec!["bridge0 deletem en1"]);
        assert!(addresses_settled(&v));
        let moving = plan_for(&s, &two_peers(), &split("en2", "en1"), &routes_split("en1", "en2"));
        assert!(!addresses_settled(&moving));
    }

    #[test]
    fn active_names_for_the_change_log() {
        assert_eq!(active_names(&settled()), Some("en6".into()));
        let two = st(vec![port("en1", true, false, &[]), port("en6", true, false, &[])], &[]);
        assert_eq!(active_names(&two), Some("en1, en6".into()));
        assert_eq!(active_names(&st(vec![port("en1", false, false, &[])], &[])), None);
    }

    #[test]
    fn state_file_is_replaced_whole() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("tblink.json");
        let state = DaemonState {
            version: "v".into(),
            rdma_ip: "10.0.1.1".into(),
            layout: split("en1", "en2"),
            updated: 100,
            ..DaemonState::default()
        };
        write_state(&path, &state).unwrap();
        write_state(&path, &state).unwrap();
        let back = net::parse_daemon_state(&std::fs::read_to_string(&path).unwrap(), 130).unwrap();
        assert_eq!(back.layout, split("en1", "en2"));
        assert_eq!(back.age_secs, Some(30));
        assert!(!path.with_extension("json.tmp").exists());
    }

    // ---- port_change_message ----

    #[test]
    fn port_change_messages() {
        assert_eq!(port_change_message(&None, &Some("en1".into())), Some("active Thunderbolt RDMA port: en1".into()));
        assert_eq!(port_change_message(&Some("en1".into()), &Some("en6".into())), Some("active Thunderbolt RDMA port: en6".into()));
        assert_eq!(port_change_message(&Some("en1".into()), &None), Some("no active Thunderbolt RDMA port (cable unplugged?)".into()));
    }

    #[test]
    fn unchanged_port_logs_nothing() {
        assert_eq!(port_change_message(&Some("en1".into()), &Some("en1".into())), None);
        assert_eq!(port_change_message(&None, &None), None);
    }

    // ---- apply ----

    fn two_actions() -> Vec<Action> {
        vec![
            act(IFCONFIG, &["en1", "inet", "10.0.1.1", "-alias"], "stale 10.0.1.1 on inactive en1".into()),
            act(ROUTE, &["-n", "add", "-net", "10.0.1.0/24", "-interface", "en6"], "route 10.0.1.0/24 via en6".into()),
        ]
    }

    #[test]
    fn dry_run_never_calls_the_runner() {
        let mut calls = 0;
        let lines = apply(&two_actions(), true, &mut |_, _| {
            calls += 1;
            (true, String::new())
        });
        assert_eq!(calls, 0);
        assert_eq!(
            lines,
            vec![
                "would run: /sbin/ifconfig en1 inet 10.0.1.1 -alias   (stale 10.0.1.1 on inactive en1)",
                "would run: /sbin/route -n add -net 10.0.1.0/24 -interface en6   (route 10.0.1.0/24 via en6)",
            ]
        );
    }

    #[test]
    fn apply_runs_each_command_with_its_arguments() {
        let mut seen = Vec::new();
        apply(&two_actions(), false, &mut |c, a| {
            seen.push(format!("{c} {}", a.join(" ")));
            (true, String::new())
        });
        assert_eq!(seen, vec!["/sbin/ifconfig en1 inet 10.0.1.1 -alias", "/sbin/route -n add -net 10.0.1.0/24 -interface en6",]);
    }

    #[test]
    fn apply_logs_success_without_output() {
        let lines = apply(&two_actions()[..1], false, &mut |_, _| (true, "noise\n".into()));
        assert_eq!(lines, vec!["/sbin/ifconfig en1 inet 10.0.1.1 -alias: ok  (stale 10.0.1.1 on inactive en1)"]);
    }

    #[test]
    fn apply_logs_failure_with_trimmed_output() {
        let lines = apply(&two_actions()[..1], false, &mut |_, _| {
            (false, "  ifconfig: ioctl (SIOCDIFADDR): Can't assign requested address\n".into())
        });
        assert_eq!(lines, vec!["/sbin/ifconfig en1 inet 10.0.1.1 -alias: FAILED  (stale 10.0.1.1 on inactive en1): ifconfig: ioctl (SIOCDIFADDR): Can't assign requested address"]);
    }

    #[test]
    fn apply_continues_after_a_failure() {
        let mut n = 0;
        let lines = apply(&two_actions(), false, &mut |_, _| {
            n += 1;
            (n != 1, "boom".into())
        });
        assert_eq!(n, 2);
        assert!(lines[0].contains("FAILED"));
        assert!(lines[1].contains(": ok "));
    }

    #[test]
    fn apply_of_nothing_is_nothing() {
        assert!(apply(&[], false, &mut |_, _| panic!("must not run")).is_empty());
    }
}
