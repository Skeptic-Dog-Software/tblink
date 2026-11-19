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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let values = |name: &str| -> Vec<String> {
        args.windows(2).filter(|w| w[0] == name).map(|w| w[1].clone()).collect()
    };
    let code = match args.first().map(String::as_str) {
        Some("status") => {
            let st = net::status();
            if flag("--json") {
                println!("{}", serde_json::to_string(&st).unwrap());
            } else {
                println!("{}", serde_json::to_string_pretty(&st).unwrap());
            }
            0
        }
        Some("port") => match value("--rdma-ip") {
            Some(rdma_ip) => port::main(port::PortArgs {
                rdma_ip,
                bridge_ip: value("--bridge-ip"),
                peer_ips: values("--peer"),
                interval: value("--interval").and_then(|s| s.parse().ok()).unwrap_or(5),
                once: flag("--once"),
                dry_run: flag("--dry-run"),
            }),
            None => { eprintln!("{USAGE}"); 2 }
        },
        Some("coordinator") => match value("--config") {
            Some(c) => coord::main(&c, flag("--once"), flag("--check")),
            None => { eprintln!("{USAGE}"); 2 }
        },
        _ => { eprintln!("{USAGE}"); 2 }
    };
    std::process::exit(code);
}
