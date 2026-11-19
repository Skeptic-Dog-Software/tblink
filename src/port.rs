//! `tblink port`: the per-Mac root daemon.
//!
//! Each tick it makes the link look the way the JACCL backend needs it:
//!   1. the active RDMA-capable Thunderbolt port carries this Mac's RDMA
//!      address (10.0.1.N/24) and, optionally, its old bridge address
//!      (10.77.0.N/24) so SSH, Screen Sharing and API clients keep working;
//!   2. no inactive port still holds either address (a stale copy owns the
//!      subnet route and silently breaks the link);
//!   3. the active port is not a bridge0 member, and bridge0 does not hold
//!      the bridge address while its members are gone;
//!   4. the subnet routes to the peer go out the active port.
//!
//! It only ever adds or removes these two addresses, removes the active port
//! from bridge0, and fixes those two routes.

use crate::net::{self, Status, BRIDGE, IFCONFIG, ROUTE};
use crate::{log, sleep_secs};

#[derive(Debug, Clone, PartialEq)]
pub struct PortArgs {
    pub rdma_ip: String,
    pub bridge_ip: Option<String>,
    pub peer_ips: Vec<String>,
    pub interval: u64,
    pub once: bool,
    pub dry_run: bool,
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

fn subnet24(addr: &str) -> String {
    let mut o: Vec<&str> = addr.split('.').take(3).collect();
    o.push("0");
    format!("{}/24", o.join("."))
}

/// Pure planning step, so it can be unit-tested against fixed snapshots.
pub fn plan(st: &Status, a: &PortArgs, routes: &dyn Fn(&str) -> Option<String>) -> Vec<Action> {
    let mut out = Vec::new();
    let Some(active) = st.active.as_ref() else { return out };
    let ours: Vec<&str> = std::iter::once(a.rdma_ip.as_str()).chain(a.bridge_ip.as_deref()).collect();

    for p in st.ports.iter().filter(|p| p.name != active.name) {
        for addr in &ours {
            if p.inet.iter().any(|x| x == addr) {
                out.push(act(IFCONFIG, &[&p.name, "inet", addr, "-alias"], format!("stale {addr} on inactive {}", p.name)));
            }
        }
    }
    if active.bridge_member {
        out.push(act(IFCONFIG, &[BRIDGE, "deletem", &active.name], format!("take {} out of {BRIDGE}", active.name)));
    }
    if let Some(b) = a.bridge_ip.as_deref() {
        if st.bridge_inet.iter().any(|x| x == b) {
            out.push(act(IFCONFIG, &[BRIDGE, "inet", b, "-alias"], format!("{b} moves from {BRIDGE} to {}", active.name)));
        }
    }
    let mut added_nets = Vec::new();
    for addr in &ours {
        if !active.inet.iter().any(|x| x == addr) {
            out.push(act(IFCONFIG, &[&active.name, "inet", addr, "netmask", "255.255.255.0", "alias"], format!("add {addr} to {}", active.name)));
            added_nets.push(subnet24(addr));
        }
    }
    // Adding an address installs its subnet route; judge routes on the next tick.
    for peer in a.peer_ips.iter().filter(|p| !added_nets.contains(&subnet24(p))) {
        match routes(peer) {
            Some(ifc) if ifc == active.name => {}
            other => {
                let net = subnet24(peer);
                if other.is_some() {
                    out.push(act(ROUTE, &["-n", "delete", "-net", &net], format!("route to {peer} uses {}", other.as_deref().unwrap_or("?"))));
                }
                out.push(act(ROUTE, &["-n", "add", "-net", &net, "-interface", &active.name], format!("route {net} via {}", active.name)));
            }
        }
    }
    out
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
            format!("{} {}: {}  ({}){}", x.cmd, x.args.join(" "), if ok { "ok" } else { "FAILED" }, x.why,
                if ok { String::new() } else { format!(": {}", out.trim()) })
        })
        .collect()
}

pub fn main(a: PortArgs) -> i32 {
    if !a.dry_run && unsafe_geteuid() != 0 {
        eprintln!("tblink port must run as root (or pass --dry-run)");
        return 1;
    }
    let mut last_active: Option<String> = None;
    loop {
        let st = net::status();
        let now_active = st.active.as_ref().map(|p| p.name.clone());
        if let Some(m) = port_change_message(&last_active, &now_active) {
            log(&m);
        }
        last_active = now_active;
        // Planned against a fresh route view: route state can lag the ifconfig change.
        let actions = plan(&st, &a, &|p| net::route_interface(p));
        for line in apply(&actions, a.dry_run, &mut |c, args| net::run(c, args)) {
            log(&line);
        }
        if a.once {
            return 0;
        }
        sleep_secs(a.interval);
    }
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
        Port { name: name.into(), rdma_device: format!("rdma_{name}"), active, bridge_member: member, inet: inet.iter().map(|s| s.to_string()).collect() }
    }
    fn args() -> PortArgs {
        PortArgs { rdma_ip: "10.0.1.1".into(), bridge_ip: Some("10.77.0.1".into()), peer_ips: vec!["10.0.1.2".into()], interval: 5, once: true, dry_run: true }
    }
    fn args_peers(peers: &[&str]) -> PortArgs {
        PortArgs { peer_ips: peers.iter().map(|s| s.to_string()).collect(), ..args() }
    }
    fn st(ports: Vec<Port>, bridge_inet: &[&str]) -> Status {
        Status { hostname: "t".into(), active: ports.iter().find(|p| p.active).cloned(), bridge_members: ports.iter().filter(|p| p.bridge_member).map(|p| p.name.clone()).collect(), ports, bridge_inet: bridge_inet.iter().map(|s| s.to_string()).collect() }
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
        assert_eq!(cmds(&v), vec![
            "en1 inet 10.0.1.1 -alias",
            "bridge0 deletem en6",
            "bridge0 inet 10.77.0.1 -alias",
            "en6 inet 10.0.1.1 netmask 255.255.255.0 alias",
            "en6 inet 10.77.0.1 netmask 255.255.255.0 alias",
        ]);
    }

    #[test]
    fn stale_route_is_replaced() {
        assert_eq!(cmds(&plan(&settled(), &args(), &routes_via("en1"))), vec![
            "-n delete -net 10.0.1.0/24",
            "-n add -net 10.0.1.0/24 -interface en6",
        ]);
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
        assert_eq!(cmds(&plan(&s, &args(), &|_| None)), vec![
            "bridge0 deletem en1",
            "en1 inet 10.0.1.1 netmask 255.255.255.0 alias",
            "en1 inet 10.77.0.1 netmask 255.255.255.0 alias",
        ]);
    }

    // ---- plan: stale addresses ----

    #[test]
    fn both_addresses_stale_on_one_inactive_port() {
        let s = st(vec![port("en1", false, false, &["10.0.1.1", "10.77.0.1"]), port("en6", true, false, &["10.0.1.1", "10.77.0.1"])], &[]);
        assert_eq!(cmds(&plan(&s, &args(), &routes_via("en6"))), vec![
            "en1 inet 10.0.1.1 -alias",
            "en1 inet 10.77.0.1 -alias",
        ]);
    }

    #[test]
    fn stale_copies_on_several_inactive_ports_are_all_removed() {
        let s = st(vec![
            port("en1", false, false, &["10.0.1.1"]),
            port("en2", false, false, &["10.77.0.1"]),
            port("en6", true, false, &["10.0.1.1", "10.77.0.1"]),
        ], &[]);
        assert_eq!(cmds(&plan(&s, &args(), &routes_via("en6"))), vec![
            "en1 inet 10.0.1.1 -alias",
            "en2 inet 10.77.0.1 -alias",
        ]);
    }

    #[test]
    fn unrelated_addresses_are_never_touched() {
        // Only this Mac's two configured addresses are managed; anything else stays.
        let s = st(vec![
            port("en1", false, false, &["10.0.1.2", "169.254.4.123"]),
            port("en6", true, false, &["10.0.1.1", "10.77.0.1", "192.168.50.9"]),
        ], &["10.77.0.2"]);
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
        let s = st(vec![port("en1", false, true, &[]), port("en2", false, true, &[]), port("en6", true, false, &["10.0.1.1", "10.77.0.1"])], &[]);
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
        let v = plan(&s, &args(), &|p| { asked.borrow_mut().push(p.to_string()); Some("en1".into()) });
        assert!(asked.borrow().is_empty());
        assert_eq!(cmds(&v), vec!["en6 inet 10.0.1.1 netmask 255.255.255.0 alias"]);
    }

    #[test]
    fn routes_for_each_peer_subnet() {
        let a = args_peers(&["10.0.1.2", "10.77.0.2"]);
        let v = plan(&settled(), &a, &routes_via("bridge0"));
        assert_eq!(cmds(&v), vec![
            "-n delete -net 10.0.1.0/24",
            "-n add -net 10.0.1.0/24 -interface en6",
            "-n delete -net 10.77.0.0/24",
            "-n add -net 10.77.0.0/24 -interface en6",
        ]);
    }

    #[test]
    fn only_the_wrong_route_is_fixed() {
        let a = args_peers(&["10.0.1.2", "10.77.0.2"]);
        let routes = |p: &str| Some(if p.starts_with("10.0.1.") { "en6" } else { "en9" }.to_string());
        assert_eq!(cmds(&plan(&settled(), &a, &routes)), vec![
            "-n delete -net 10.77.0.0/24",
            "-n add -net 10.77.0.0/24 -interface en6",
        ]);
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
        assert!(plan(&settled(), &a, &|_| { *asked.borrow_mut() += 1; Some("en1".into()) }).is_empty());
        assert_eq!(*asked.borrow(), 0);
    }

    #[test]
    fn route_is_looked_up_per_peer_address() {
        let a = args_peers(&["10.0.1.2", "10.77.0.2"]);
        let asked = RefCell::new(Vec::new());
        plan(&settled(), &a, &|p| { asked.borrow_mut().push(p.to_string()); Some("en6".into()) });
        assert_eq!(*asked.borrow(), vec!["10.0.1.2", "10.77.0.2"]);
    }

    #[test]
    fn route_check_skips_only_the_newly_added_subnet() {
        // 10.77.0.1 is being added; the 10.0.1.x route is still checked.
        let a = args_peers(&["10.0.1.2", "10.77.0.2"]);
        let s = st(vec![port("en6", true, false, &["10.0.1.1"])], &[]);
        let asked = RefCell::new(Vec::new());
        plan(&s, &a, &|p| { asked.borrow_mut().push(p.to_string()); Some("en6".into()) });
        assert_eq!(*asked.borrow(), vec!["10.0.1.2"]);
    }

    // ---- plan: ordering and idempotence ----

    #[test]
    fn actions_run_in_a_safe_order() {
        // Stale copies go first (they own the route), then bridge cleanup, then adds, then routes.
        let a = args_peers(&["10.0.1.2", "10.77.0.2"]);
        let s = st(vec![port("en1", false, true, &["10.0.1.1"]), port("en6", true, true, &["10.77.0.1"])], &["10.77.0.1"]);
        let v = plan(&s, &a, &routes_via("en1"));
        assert_eq!(cmds(&v), vec![
            "en1 inet 10.0.1.1 -alias",
            "bridge0 deletem en6",
            "bridge0 inet 10.77.0.1 -alias",
            "en6 inet 10.0.1.1 netmask 255.255.255.0 alias",
            "-n delete -net 10.77.0.0/24",
            "-n add -net 10.77.0.0/24 -interface en6",
        ]);
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
        let lines = apply(&two_actions(), true, &mut |_, _| { calls += 1; (true, String::new()) });
        assert_eq!(calls, 0);
        assert_eq!(lines, vec![
            "would run: /sbin/ifconfig en1 inet 10.0.1.1 -alias   (stale 10.0.1.1 on inactive en1)",
            "would run: /sbin/route -n add -net 10.0.1.0/24 -interface en6   (route 10.0.1.0/24 via en6)",
        ]);
    }

    #[test]
    fn apply_runs_each_command_with_its_arguments() {
        let mut seen = Vec::new();
        apply(&two_actions(), false, &mut |c, a| { seen.push(format!("{c} {}", a.join(" "))); (true, String::new()) });
        assert_eq!(seen, vec![
            "/sbin/ifconfig en1 inet 10.0.1.1 -alias",
            "/sbin/route -n add -net 10.0.1.0/24 -interface en6",
        ]);
    }

    #[test]
    fn apply_logs_success_without_output() {
        let lines = apply(&two_actions()[..1], false, &mut |_, _| (true, "noise\n".into()));
        assert_eq!(lines, vec!["/sbin/ifconfig en1 inet 10.0.1.1 -alias: ok  (stale 10.0.1.1 on inactive en1)"]);
    }

    #[test]
    fn apply_logs_failure_with_trimmed_output() {
        let lines = apply(&two_actions()[..1], false, &mut |_, _| (false, "  ifconfig: ioctl (SIOCDIFADDR): Can't assign requested address\n".into()));
        assert_eq!(lines, vec!["/sbin/ifconfig en1 inet 10.0.1.1 -alias: FAILED  (stale 10.0.1.1 on inactive en1): ifconfig: ioctl (SIOCDIFADDR): Can't assign requested address"]);
    }

    #[test]
    fn apply_continues_after_a_failure() {
        let mut n = 0;
        let lines = apply(&two_actions(), false, &mut |_, _| { n += 1; (n != 1, "boom".into()) });
        assert_eq!(n, 2);
        assert!(lines[0].contains("FAILED"));
        assert!(lines[1].contains(": ok "));
    }

    #[test]
    fn apply_of_nothing_is_nothing() {
        assert!(apply(&[], false, &mut |_, _| panic!("must not run")).is_empty());
    }
}
