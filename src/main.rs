//! tblink: keep a two-Mac Thunderbolt RDMA link, and the oMLX cluster
//! deployment that runs over it, up across cable moves and reboots.
//!
//!   tblink status [--json]
//!   tblink port --rdma-ip 10.0.1.N [--bridge-ip 10.77.0.N] [--peer IP]... [--interval 5] [--once] [--dry-run]
//!   tblink coordinator --config PATH [--once] [--check]

mod coord;
mod net;
mod port;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn log(msg: &str) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (_, ts) = net::run("/bin/date", &["-r", &secs.to_string(), "+%Y-%m-%dT%H:%M:%S%z"]);
    println!("{} {msg}", ts.trim());
}

pub fn sleep_secs(s: u64) {
    std::thread::sleep(Duration::from_secs(s));
}

const USAGE: &str = "usage:
  tblink status [--json]
  tblink port --rdma-ip 10.0.1.N [--bridge-ip 10.77.0.N] [--peer IP]... [--interval SECS] [--once] [--dry-run]
  tblink coordinator --config PATH [--once] [--check]";

/// A parsed command line.
#[derive(Debug, PartialEq)]
enum Cmd {
    Status { json: bool },
    Port(port::PortArgs),
    Coordinator { config: String, once: bool, check: bool },
}

/// Parse the arguments after the program name. `None` means print usage.
fn parse(args: &[String]) -> Option<Cmd> {
    let flag = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let values = |name: &str| -> Vec<String> {
        args.windows(2).filter(|w| w[0] == name).map(|w| w[1].clone()).collect()
    };
    match args.first().map(String::as_str) {
        Some("status") => Some(Cmd::Status { json: flag("--json") }),
        Some("port") => value("--rdma-ip").map(|rdma_ip| Cmd::Port(port::PortArgs {
            rdma_ip,
            bridge_ip: value("--bridge-ip"),
            peer_ips: values("--peer"),
            interval: value("--interval").and_then(|s| s.parse().ok()).unwrap_or(5),
            once: flag("--once"),
            dry_run: flag("--dry-run"),
        })),
        Some("coordinator") => value("--config").map(|config| Cmd::Coordinator { config, once: flag("--once"), check: flag("--check") }),
        _ => None,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match parse(&args) {
        Some(Cmd::Status { json }) => {
            let st = net::status();
            if json {
                println!("{}", serde_json::to_string(&st).unwrap());
            } else {
                println!("{}", serde_json::to_string_pretty(&st).unwrap());
            }
            0
        }
        Some(Cmd::Port(a)) => port::main(a),
        Some(Cmd::Coordinator { config, once, check }) => coord::main(&config, once, check),
        None => { eprintln!("{USAGE}"); 2 }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Option<Cmd> {
        let v: Vec<String> = s.split_whitespace().map(str::to_string).collect();
        parse(&v)
    }
    fn port_args(s: &str) -> port::PortArgs {
        match p(s) { Some(Cmd::Port(a)) => a, other => panic!("expected port, got {other:?}") }
    }

    #[test]
    fn no_arguments_is_usage() {
        assert_eq!(p(""), None);
    }

    #[test]
    fn unknown_subcommand_is_usage() {
        assert_eq!(p("start"), None);
        assert_eq!(p("--json"), None);
        assert_eq!(p("Status"), None);
    }

    #[test]
    fn status_plain_and_json() {
        assert_eq!(p("status"), Some(Cmd::Status { json: false }));
        assert_eq!(p("status --json"), Some(Cmd::Status { json: true }));
    }

    #[test]
    fn port_needs_an_rdma_ip() {
        assert_eq!(p("port"), None);
        assert_eq!(p("port --bridge-ip 10.77.0.1 --peer 10.0.1.2"), None);
        // A trailing flag with no value is missing too.
        assert_eq!(p("port --rdma-ip"), None);
    }

    #[test]
    fn port_minimal_uses_defaults() {
        assert_eq!(port_args("port --rdma-ip 10.0.1.2"), port::PortArgs {
            rdma_ip: "10.0.1.2".into(), bridge_ip: None, peer_ips: vec![], interval: 5, once: false, dry_run: false,
        });
    }

    #[test]
    fn port_full_install_line() {
        // The exact arguments install.sh writes into the LaunchDaemon.
        assert_eq!(port_args("port --rdma-ip 10.0.1.1 --bridge-ip 10.77.0.1 --peer 10.0.1.2 --peer 10.77.0.2"), port::PortArgs {
            rdma_ip: "10.0.1.1".into(), bridge_ip: Some("10.77.0.1".into()),
            peer_ips: vec!["10.0.1.2".into(), "10.77.0.2".into()], interval: 5, once: false, dry_run: false,
        });
    }

    #[test]
    fn port_documented_dry_run_line() {
        let a = port_args("port --rdma-ip 10.0.1.2 --bridge-ip 10.77.0.2 --peer 10.0.1.1 --peer 10.77.0.1 --once --dry-run");
        assert!(a.once && a.dry_run);
        assert_eq!(a.peer_ips, vec!["10.0.1.1", "10.77.0.1"]);
    }

    #[test]
    fn port_flags_in_any_order() {
        let a = port_args("port --once --peer 10.0.1.1 --dry-run --rdma-ip 10.0.1.2 --interval 9");
        assert_eq!(a.rdma_ip, "10.0.1.2");
        assert_eq!(a.peer_ips, vec!["10.0.1.1"]);
        assert_eq!(a.interval, 9);
        assert!(a.once && a.dry_run);
    }

    #[test]
    fn port_interval_falls_back_to_five() {
        assert_eq!(port_args("port --rdma-ip 10.0.1.2 --interval soon").interval, 5);
        assert_eq!(port_args("port --rdma-ip 10.0.1.2 --interval -3").interval, 5);
        assert_eq!(port_args("port --rdma-ip 10.0.1.2 --interval").interval, 5);
        assert_eq!(port_args("port --rdma-ip 10.0.1.2 --interval 0").interval, 0);
    }

    #[test]
    fn port_first_value_wins_for_single_flags() {
        let a = port_args("port --rdma-ip 10.0.1.1 --rdma-ip 10.0.1.9");
        assert_eq!(a.rdma_ip, "10.0.1.1");
    }

    #[test]
    fn port_peers_keep_their_order() {
        let a = port_args("port --rdma-ip 10.0.1.1 --peer c --peer a --peer b");
        assert_eq!(a.peer_ips, vec!["c", "a", "b"]);
    }

    #[test]
    fn coordinator_needs_a_config() {
        assert_eq!(p("coordinator"), None);
        assert_eq!(p("coordinator --once"), None);
        assert_eq!(p("coordinator --config"), None);
    }

    #[test]
    fn coordinator_options() {
        assert_eq!(p("coordinator --config ~/c.json"), Some(Cmd::Coordinator { config: "~/c.json".into(), once: false, check: false }));
        assert_eq!(p("coordinator --check --config /c.json"), Some(Cmd::Coordinator { config: "/c.json".into(), once: false, check: true }));
        assert_eq!(p("coordinator --config /c.json --once --check"), Some(Cmd::Coordinator { config: "/c.json".into(), once: true, check: true }));
    }

    #[test]
    fn subcommand_must_come_first() {
        assert_eq!(p("--json status"), None);
        assert_eq!(p("--rdma-ip 10.0.1.1 port"), None);
    }

    #[test]
    fn usage_lists_every_subcommand() {
        for s in ["tblink status", "tblink port", "tblink coordinator"] {
            assert!(USAGE.contains(s), "{s}");
        }
    }
}
