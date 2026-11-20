//! Which Thunderbolt cable carries what, when the Macs are joined by more
//! than one.
//!
//! With one cable, it carries both of this Mac's addresses: the RDMA address
//! (10.0.1.N, used only by JACCL) and the management address (10.77.0.N: SSH,
//! API clients, model copies, the Ring fallback). With two or more, the
//! fastest cable carries only the RDMA address and the next one carries the
//! management address.
//!
//! Both Macs must pick the same physical cable for each role, and they do it
//! without talking to each other:
//!
//!   1. Ranking. Cables are ordered by negotiated speed, then by cable id (the
//!      pair of Thunderbolt domain UUIDs the cable joins, see `tb`). Both ends
//!      of a cable see the same speed and the same id, so both Macs rank their
//!      cables the same way, ties included.
//!   2. Debounce. Losing a cable in use moves its role to a remaining cable at
//!      once. Moving back to a cable that appeared waits until it has been up
//!      for `settle` seconds, doubling after each flap (a cable lost soon after
//!      roles moved onto it), up to `max_settle`.
//!   3. Verification. Ranking can only disagree if the Macs see different
//!      cables (a port without RDMA on one side, a cable half-seated). So each
//!      Mac checks that the other Mac's RDMA address answers over the inference
//!      cable and its management address over the management cable (a TCP
//!      connect bound to that port). If, for `verify` seconds, one answers and
//!      the other does not, the other Mac is evidently running both addresses
//!      on one cable, and this Mac follows it there. It goes back as soon as
//!      either of the other Mac's addresses stops answering on that cable for
//!      `release` seconds (`release` is shorter than `verify`, so the Mac that
//!      moves first is followed, not chased). So that two Macs can never hold
//!      each other in a fallback, the leader (the Mac with the lower RDMA
//!      address) gives its fallback up after `lease` seconds, doubling each
//!      time, and tries the ranking again; the follower then follows it.
//!      If neither address answers and the top two cables tie on speed, the
//!      follower swaps the two roles (backing off each time), in case the Macs
//!      broke the tie differently. None of this happens until the other Mac
//!      has answered a probe at least once, so a Mac that does not accept
//!      connections (Remote Login off) cannot make the roles wander.

use serde::{Deserialize, Serialize};

/// A usable link: an active RDMA-capable Thunderbolt port with what is known
/// about its cable.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub port: String,
    pub speed_gbps: Option<u32>,
    pub cable_id: Option<String>,
}

/// Where this Mac's two addresses go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum Layout {
    /// No usable link.
    #[default]
    None,
    /// One cable carries both addresses.
    Single { port: String },
    /// A dedicated inference cable and a management cable.
    Split { inference: String, management: String },
}

impl Layout {
    /// The port that carries the RDMA address.
    pub fn rdma_port(&self) -> Option<&str> {
        match self {
            Layout::None => None,
            Layout::Single { port } => Some(port),
            Layout::Split { inference, .. } => Some(inference),
        }
    }
    /// The port that carries the management (bridge) address.
    pub fn bridge_port(&self) -> Option<&str> {
        match self {
            Layout::None => None,
            Layout::Single { port } => Some(port),
            Layout::Split { management, .. } => Some(management),
        }
    }
    pub fn ports(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.rdma_port().into_iter().chain(self.bridge_port()).collect();
        v.dedup();
        v
    }
    pub fn describe(&self) -> String {
        match self {
            Layout::None => "no link".into(),
            Layout::Single { port } => format!("{port} carries inference and management"),
            Layout::Split { inference, management } => format!("inference on {inference} (dedicated), management on {management}"),
        }
    }
}

/// Fastest first; ties by cable id, then by port name. Unknown speeds and ids sort last.
pub fn rank(links: &[Link]) -> Vec<Link> {
    let mut v = links.to_vec();
    v.sort_by(|a, b| {
        b.speed_gbps
            .unwrap_or(0)
            .cmp(&a.speed_gbps.unwrap_or(0))
            .then_with(|| a.cable_id.is_none().cmp(&b.cable_id.is_none()))
            .then_with(|| a.cable_id.cmp(&b.cable_id))
            .then_with(|| a.port.cmp(&b.port))
    });
    v
}

/// The layout the ranking calls for. `split` is false when there is no
/// management address to place.
pub fn ideal(links: &[Link], split: bool) -> Layout {
    let r = rank(links);
    match r.as_slice() {
        [] => Layout::None,
        [one] => Layout::Single { port: one.port.clone() },
        [a, b, ..] if split && a.cable_id.is_some() && b.cable_id.is_some() => {
            Layout::Split { inference: a.port.clone(), management: b.port.clone() }
        }
        // Without cable ids the Macs could not agree on a split; keep today's single-cable behaviour.
        [a, ..] => Layout::Single { port: a.port.clone() },
    }
}

/// The top two links negotiated the same speed.
pub fn tied(links: &[Link]) -> bool {
    let r = rank(links);
    r.len() >= 2 && r[0].speed_gbps == r[1].speed_gbps
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tuning {
    pub settle: u64,
    pub max_settle: u64,
    pub flap_window: u64,
    pub verify: u64,
    pub release: u64,
    /// How long the leader holds a fallback before trying the dedicated link
    /// again (doubling each time, up to `max_lease`).
    pub lease: u64,
    pub max_lease: u64,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning { settle: 30, max_settle: 600, flap_window: 600, verify: 45, release: 15, lease: 600, max_lease: 14400 }
    }
}

/// Whether the other Mac's addresses answer over this Mac's ports. `None`
/// means not checked (one cable, or addresses still moving).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Probe {
    /// The other Mac's RDMA address, over the port carrying ours.
    pub rdma: Option<bool>,
    /// The other Mac's management address, over the port carrying ours.
    pub bridge: Option<bool>,
}

/// A departure from the ranking, made on evidence about the other Mac. It
/// holds only while the cables stay as they were (`base`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Adapt {
    /// Both addresses on one cable, because the other Mac evidently uses that
    /// cable for both. Held while both of the other Mac's addresses answer there.
    Fallback { to: Layout, base: Layout },
    /// Inference and management swapped: the other Mac broke a speed tie the other way.
    Swapped { base: Layout },
}

impl Adapt {
    fn base(&self) -> &Layout {
        match self {
            Adapt::Fallback { base, .. } | Adapt::Swapped { base } => base,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Controller {
    pub layout: Layout,
    pub adapt: Option<Adapt>,
    /// Whether the other Mac has ever answered a probe.
    pub probe_works: bool,
    /// The lower RDMA address leads: it never swaps a tie.
    pub leader: bool,
    /// A management address is configured, so a split is possible.
    pub split: bool,
    pub tuning: Tuning,
    adopted: u64,
    pending: Option<(Layout, u64)>,
    flaps: u32,
    last_loss: Option<u64>,
    /// The last probe result and since when it has been exactly that.
    seen: Option<(Probe, u64)>,
    /// Tie swaps since the other Mac last answered; each one waits twice as long.
    swaps: u32,
    /// Leader fallbacks given up since the dedicated link last worked.
    leases: u32,
}

fn swap(l: &Layout) -> Layout {
    match l {
        Layout::Split { inference, management } => Layout::Split { inference: management.clone(), management: inference.clone() },
        other => other.clone(),
    }
}

impl Controller {
    pub fn new(leader: bool, split: bool, tuning: Tuning) -> Self {
        Controller {
            layout: Layout::None,
            adapt: None,
            probe_works: false,
            leader,
            split,
            tuning,
            adopted: 0,
            pending: None,
            flaps: 0,
            last_loss: None,
            seen: None,
            swaps: 0,
            leases: 0,
        }
    }

    /// How long a newly available layout must stay available before roles move to it.
    pub fn settle_secs(&self, now: u64) -> u64 {
        let recent = self.last_loss.map(|t| now.saturating_sub(t) < self.tuning.flap_window).unwrap_or(false);
        let flaps = if recent { self.flaps.min(16) } else { 0 };
        self.tuning.settle.saturating_mul(1 << flaps).min(self.tuning.max_settle.max(self.tuning.settle))
    }

    /// The layout the controller is heading for.
    pub fn target(&self, ideal: &Layout) -> Layout {
        match &self.adapt {
            None => ideal.clone(),
            Some(Adapt::Fallback { to, .. }) => to.clone(),
            Some(Adapt::Swapped { .. }) => swap(ideal),
        }
    }

    /// Seconds until a pending layout is adopted, if one is waiting.
    pub fn pending(&self, now: u64) -> Option<(Layout, u64)> {
        self.pending.as_ref().map(|(l, since)| (l.clone(), (since + self.settle_secs(now)).saturating_sub(now)))
    }

    fn adopt(&mut self, to: Layout, now: u64, why: &str, log: &mut Vec<String>) {
        log.push(format!("roles: {} ({why})", to.describe()));
        self.layout = to;
        self.adopted = now;
        self.pending = None;
        self.seen = None;
    }

    /// One tick: given the usable links and what the probes saw of the layout
    /// in force, decide the layout. `now` is in seconds on any monotonic clock.
    /// Returns log lines for anything that changed.
    pub fn step(&mut self, links: &[Link], probe: Probe, now: u64) -> Vec<String> {
        let mut log = Vec::new();
        let ideal = ideal(links, self.split);

        if self.adapt.as_ref().is_some_and(|a| a.base() != &ideal) {
            log.push("roles: the cables changed; going back to the ranking".to_string());
            self.adapt = None;
            self.swaps = 0;
            self.leases = 0;
        }

        // Evidence about the layout in force.
        if probe.rdma == Some(true) || probe.bridge == Some(true) {
            self.probe_works = true;
            self.swaps = 0;
        }
        if probe == (Probe { rdma: Some(true), bridge: Some(true) }) && matches!(self.layout, Layout::Split { .. }) {
            self.leases = 0;
        }
        // Judge a pattern only once it has held: a Mac that just woke up must get
        // its `release` window before this one acts on what it sees.
        self.seen = match self.seen {
            Some((p, since)) if p == probe => Some((p, since)),
            _ if probe == Probe::default() => None,
            _ => Some((probe, now)),
        };
        let held = |secs: u64| self.seen.is_some_and(|(_, since)| now.saturating_sub(since) >= secs);
        let (rdma, bridge) = (probe.rdma, probe.bridge);
        let mut forced: Option<String> = None;
        if self.probe_works && self.layout == self.target(&ideal) {
            let verify = self.tuning.verify;
            match (&self.adapt, &self.layout) {
                (None | Some(Adapt::Swapped { .. }), Layout::Split { inference, management }) => {
                    let (inference, management) = (inference.clone(), management.clone());
                    if rdma == Some(false) && bridge == Some(true) && held(verify) {
                        self.adapt = Some(Adapt::Fallback { to: Layout::Single { port: management.clone() }, base: ideal.clone() });
                        forced = Some(format!(
                            "the other Mac's RDMA address does not answer on {inference} but its management address answers on {management}; it is using {management} for both"
                        ));
                    } else if bridge == Some(false) && rdma == Some(true) && held(verify) {
                        self.adapt = Some(Adapt::Fallback { to: Layout::Single { port: inference.clone() }, base: ideal.clone() });
                        forced = Some(format!(
                            "the other Mac's management address does not answer on {management} but its RDMA address answers on {inference}; it is using {inference} for both"
                        ));
                    } else if rdma == Some(false)
                        && bridge == Some(false)
                        && held(verify.saturating_mul(1 << self.swaps.min(6)).min(3600))
                        && !self.leader
                        && tied(links)
                    {
                        // The other Mac may simply be down; back off so that does not churn.
                        self.swaps += 1;
                        self.adapt = match self.adapt {
                            Some(Adapt::Swapped { .. }) => None,
                            _ => Some(Adapt::Swapped { base: ideal.clone() }),
                        };
                        forced = Some(
                            "neither of the other Mac's addresses answers and the cables tie on speed; swapping inference and management"
                                .into(),
                        );
                    }
                }
                (Some(Adapt::Fallback { to, .. }), _) => {
                    // The other Mac is on this cable for both roles only while both its addresses answer here.
                    let gone = rdma == Some(false) || bridge == Some(false);
                    let lease = self.tuning.lease.saturating_mul(1 << self.leases.min(10)).min(self.tuning.max_lease);
                    let port = to.rdma_port().unwrap_or("?").to_string();
                    if gone && held(self.tuning.release) {
                        self.adapt = None;
                        forced = Some(format!("the other Mac left {port}; using the dedicated inference link again"));
                    } else if self.leader && now.saturating_sub(self.adopted) >= lease {
                        // Both Macs could be following each other; the leader breaks that by trying the ranking again.
                        self.adapt = None;
                        self.leases += 1;
                        forced = Some(format!("held {port} for both roles for {lease} s; trying the dedicated inference link again"));
                    }
                }
                _ => {}
            }
        }

        let target = self.target(&ideal);
        if target == self.layout {
            if self.pending.take().is_some() && forced.is_none() && self.flaps > 0 {
                // The link we were waiting for vanished again while still flapping: keep the backoff alive.
                self.last_loss = Some(now);
            }
            return log;
        }
        let in_use_present = self.layout.ports().iter().all(|p| links.iter().any(|l| l.port == *p));
        if self.layout == Layout::None || !in_use_present {
            let why = if self.layout == Layout::None {
                "link up".to_string()
            } else {
                // Lost a cable in use. A loss soon after moving onto it is a flap.
                self.flaps = if now.saturating_sub(self.adopted) < self.tuning.flap_window { self.flaps.saturating_add(1) } else { 0 };
                self.last_loss = Some(now);
                let gone: Vec<&str> = self.layout.ports().into_iter().filter(|p| !links.iter().any(|l| l.port == *p)).collect();
                format!("{} went away", gone.join(", "))
            };
            self.adopt(target, now, &why, &mut log);
        } else if let Some(why) = forced {
            self.adopt(target, now, &why, &mut log);
        } else {
            // Something better appeared; wait until it has stayed.
            if self.pending.as_ref().map(|(p, _)| p != &target).unwrap_or(true) {
                self.pending = Some((target.clone(), now));
                log.push(format!("roles: {} is possible; waiting {} s for the link to settle", target.describe(), self.settle_secs(now)));
            }
            let since = self.pending.as_ref().map(|(_, s)| *s).unwrap_or(now);
            if now.saturating_sub(since) >= self.settle_secs(now) {
                self.adopt(target, now, "link stayed up", &mut log);
            }
        }
        log
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(port: &str, speed: u32, id: &str) -> Link {
        Link { port: port.into(), speed_gbps: Some(speed), cable_id: Some(id.into()) }
    }
    fn split(i: &str, m: &str) -> Layout {
        Layout::Split { inference: i.into(), management: m.into() }
    }
    fn single(p: &str) -> Layout {
        Layout::Single { port: p.into() }
    }
    fn ctl() -> Controller {
        Controller::new(true, true, Tuning::default())
    }
    const NO: Probe = Probe { rdma: None, bridge: None };

    // ---- ranking ----

    #[test]
    fn fastest_cable_is_inference() {
        let l = [link("en2", 40, "B"), link("en1", 80, "A")];
        assert_eq!(ideal(&l, true), split("en1", "en2"));
        assert_eq!(ideal(&[link("en1", 40, "A"), link("en2", 80, "B")], true), split("en2", "en1"));
    }

    #[test]
    fn identical_cables_break_the_tie_by_cable_id_not_port_name() {
        // Port names differ between the Macs; cable ids do not.
        let mac_b = [link("en1", 40, "Y"), link("en2", 40, "X")];
        let mac_a = [link("en6", 40, "X"), link("en1", 40, "Y")];
        assert_eq!(ideal(&mac_b, true), split("en2", "en1"));
        assert_eq!(ideal(&mac_a, true), split("en6", "en1"));
        assert!(tied(&mac_a) && tied(&mac_b));
    }

    #[test]
    fn ranking_ignores_input_order() {
        let a = [link("en1", 40, "Y"), link("en2", 80, "X"), link("en6", 40, "Z")];
        let mut b = a.clone();
        b.reverse();
        assert_eq!(rank(&a), rank(&b));
        assert_eq!(ideal(&a, true), split("en2", "en1"));
    }

    #[test]
    fn third_cable_is_unused() {
        let l = [link("en1", 80, "A"), link("en2", 40, "B"), link("en6", 40, "C")];
        assert_eq!(ideal(&l, true), split("en1", "en2"));
    }

    #[test]
    fn one_cable_carries_both() {
        assert_eq!(ideal(&[link("en2", 40, "A")], true), single("en2"));
        assert_eq!(ideal(&[], true), Layout::None);
    }

    #[test]
    fn without_a_management_address_the_fastest_carries_rdma() {
        assert_eq!(ideal(&[link("en2", 40, "B"), link("en1", 80, "A")], false), single("en1"));
    }

    #[test]
    fn without_cable_ids_two_cables_stay_single() {
        let l = [Link { port: "en1".into(), speed_gbps: Some(80), cable_id: None }, link("en2", 40, "B")];
        assert_eq!(ideal(&l, true), single("en1"));
        let unknown =
            [Link { port: "en6".into(), speed_gbps: None, cable_id: None }, Link { port: "en1".into(), speed_gbps: None, cable_id: None }];
        // Today's behaviour: the first by name.
        assert_eq!(ideal(&unknown, true), single("en1"));
    }

    #[test]
    fn unknown_speed_ranks_below_known() {
        let l = [Link { port: "en1".into(), speed_gbps: None, cable_id: Some("A".into()) }, link("en2", 40, "B")];
        assert_eq!(ideal(&l, true), split("en2", "en1"));
        assert!(!tied(&l));
    }

    #[test]
    fn layout_accessors() {
        assert_eq!(split("en1", "en2").rdma_port(), Some("en1"));
        assert_eq!(split("en1", "en2").bridge_port(), Some("en2"));
        assert_eq!(split("en1", "en2").ports(), vec!["en1", "en2"]);
        assert_eq!(single("en2").ports(), vec!["en2"]);
        assert!(Layout::None.ports().is_empty());
    }

    #[test]
    fn layout_json_shape() {
        assert_eq!(
            serde_json::to_value(split("en1", "en2")).unwrap(),
            serde_json::json!({"mode": "split", "inference": "en1", "management": "en2"})
        );
        assert_eq!(serde_json::to_value(single("en2")).unwrap(), serde_json::json!({"mode": "single", "port": "en2"}));
        assert_eq!(serde_json::to_value(Layout::None).unwrap(), serde_json::json!({"mode": "none"}));
    }

    // ---- controller: single Mac ----

    #[test]
    fn first_link_is_used_at_once() {
        let mut c = ctl();
        let log = c.step(&[link("en2", 40, "B")], NO, 0);
        assert_eq!(c.layout, single("en2"));
        assert_eq!(log, vec!["roles: en2 carries inference and management (link up)"]);
    }

    #[test]
    fn two_cables_at_start_split_at_once() {
        let mut c = ctl();
        c.step(&[link("en2", 40, "B"), link("en1", 80, "A")], NO, 0);
        assert_eq!(c.layout, split("en1", "en2"));
    }

    #[test]
    fn steady_state_logs_nothing() {
        let mut c = ctl();
        let l = [link("en2", 40, "B"), link("en1", 80, "A")];
        c.step(&l, NO, 0);
        for t in 1..100 {
            assert!(c.step(&l, NO, t * 5).is_empty());
        }
    }

    #[test]
    fn losing_the_inference_cable_fails_over_at_once() {
        let mut c = ctl();
        c.step(&[link("en2", 40, "B"), link("en1", 80, "A")], NO, 0);
        let log = c.step(&[link("en2", 40, "B")], NO, 5);
        assert_eq!(c.layout, single("en2"));
        assert_eq!(log, vec!["roles: en2 carries inference and management (en1 went away)"]);
    }

    #[test]
    fn losing_the_management_cable_fails_over_at_once() {
        let mut c = ctl();
        c.step(&[link("en2", 40, "B"), link("en1", 80, "A")], NO, 0);
        c.step(&[link("en1", 80, "A")], NO, 5);
        assert_eq!(c.layout, single("en1"));
    }

    #[test]
    fn losing_every_cable() {
        let mut c = ctl();
        c.step(&[link("en2", 40, "B")], NO, 0);
        c.step(&[], NO, 5);
        assert_eq!(c.layout, Layout::None);
    }

    #[test]
    fn returning_cable_waits_for_the_settle_time() {
        let mut c = ctl();
        let both = [link("en2", 40, "B"), link("en1", 80, "A")];
        c.step(&[link("en2", 40, "B")], NO, 0);
        let log = c.step(&both, NO, 1000);
        assert_eq!(log, vec!["roles: inference on en1 (dedicated), management on en2 is possible; waiting 30 s for the link to settle"]);
        for t in [1005, 1010, 1025] {
            c.step(&both, NO, t);
            assert_eq!(c.layout, single("en2"), "t={t}");
        }
        c.step(&both, NO, 1030);
        assert_eq!(c.layout, split("en1", "en2"));
    }

    #[test]
    fn a_new_third_cable_does_not_disturb_until_settled() {
        let mut c = ctl();
        c.step(&[link("en2", 40, "B"), link("en1", 40, "A")], NO, 0);
        assert_eq!(c.layout, split("en1", "en2"));
        let three = [link("en2", 40, "B"), link("en1", 40, "A"), link("en6", 80, "C")];
        c.step(&three, NO, 700);
        assert_eq!(c.layout, split("en1", "en2"));
        c.step(&three, NO, 730);
        assert_eq!(c.layout, split("en6", "en1"));
    }

    #[test]
    fn a_cable_that_drops_during_settling_resets_the_wait() {
        let mut c = ctl();
        let both = [link("en2", 40, "B"), link("en1", 80, "A")];
        let one = [link("en2", 40, "B")];
        c.step(&one, NO, 0);
        c.step(&both, NO, 1000);
        c.step(&one, NO, 1020);
        c.step(&both, NO, 1025);
        c.step(&both, NO, 1050);
        assert_eq!(c.layout, single("en2"), "the 30 s restart at 1025");
        c.step(&both, NO, 1055);
        assert_eq!(c.layout, split("en1", "en2"));
    }

    #[test]
    fn flapping_cable_backs_off_and_bounds_rebuilds() {
        let mut c = ctl();
        let both = [link("en2", 40, "B"), link("en1", 80, "A")];
        let one = [link("en2", 40, "B")];
        c.step(&both, NO, 0);
        // The fast cable goes down for 5 s every 40 s for an hour.
        let mut changes = 0;
        let mut last = c.layout.clone();
        let mut t = 0;
        while t < 3600 {
            t += 5;
            let up = t % 40 >= 5;
            c.step(if up { &both } else { &one }, NO, t);
            if c.layout != last {
                changes += 1;
                last = c.layout.clone();
            }
        }
        // Without backoff this would be ~180 role changes; after the first loss the cable never stays up 60 s.
        assert!(changes <= 2, "{changes}");
        assert_eq!(c.layout, single("en2"));
    }

    #[test]
    fn settle_doubles_per_flap_and_caps() {
        let mut c = ctl();
        let both = [link("en2", 40, "B"), link("en1", 80, "A")];
        let one = [link("en2", 40, "B")];
        c.step(&both, NO, 0);
        let mut t = 0;
        let mut waits = Vec::new();
        for _ in 0..7 {
            t += 10;
            c.step(&one, NO, t); // lost soon after adopting: a flap
            t += 5;
            c.step(&both, NO, t);
            let wait = c.settle_secs(t);
            waits.push(wait);
            t += wait;
            c.step(&both, NO, t);
            assert_eq!(c.layout, split("en1", "en2"));
        }
        assert_eq!(waits, vec![60, 120, 240, 480, 600, 600, 600]);
    }

    #[test]
    fn flap_count_resets_after_a_quiet_period() {
        let mut c = ctl();
        let both = [link("en2", 40, "B"), link("en1", 80, "A")];
        let one = [link("en2", 40, "B")];
        c.step(&both, NO, 0);
        c.step(&one, NO, 10);
        assert_eq!(c.settle_secs(15), 60);
        // Ten minutes later the flap no longer counts.
        assert_eq!(c.settle_secs(10 + 600), 30);
        // And losing a cable that was up for a long time is not a flap.
        c.step(&both, NO, 2000);
        c.step(&both, NO, 2030);
        c.step(&one, NO, 9000);
        assert_eq!(c.settle_secs(9005), 30);
    }

    #[test]
    fn cable_moved_to_another_port() {
        let mut c = ctl();
        c.step(&[link("en2", 40, "B"), link("en1", 80, "A")], NO, 0);
        // The fast cable moves from en1 to en6. Its id changes too (another bus on this Mac).
        c.step(&[link("en2", 40, "B")], NO, 900);
        assert_eq!(c.layout, single("en2"));
        let moved = [link("en2", 40, "B"), link("en6", 80, "A2")];
        c.step(&moved, NO, 905);
        c.step(&moved, NO, 935);
        assert_eq!(c.layout, split("en6", "en2"));
    }

    #[test]
    fn cable_moved_within_one_tick() {
        let mut c = ctl();
        c.step(&[link("en2", 40, "B"), link("en1", 80, "A")], NO, 0);
        c.step(&[link("en2", 40, "B"), link("en6", 80, "A2")], NO, 900);
        assert_eq!(c.layout, split("en6", "en2"), "the layout in use lost a port, so the new ranking applies at once");
    }

    #[test]
    fn swapping_cables_between_the_two_ports() {
        let mut c = ctl();
        c.step(&[link("en2", 40, "B"), link("en1", 80, "A")], NO, 0);
        // Both ports stay up but the fast cable is now on en2: no port vanished, so wait.
        let swapped = [link("en2", 80, "A2"), link("en1", 40, "B2")];
        c.step(&swapped, NO, 900);
        assert_eq!(c.layout, split("en1", "en2"));
        c.step(&swapped, NO, 930);
        assert_eq!(c.layout, split("en2", "en1"));
    }

    // ---- controller: probes on one Mac ----

    fn settled_split() -> (Controller, [Link; 2]) {
        let mut c = ctl();
        let l = [link("en2", 40, "B"), link("en1", 80, "A")];
        c.step(&l, NO, 0);
        (c, l)
    }
    const OK: Probe = Probe { rdma: Some(true), bridge: Some(true) };

    #[test]
    fn healthy_probes_change_nothing() {
        let (mut c, l) = settled_split();
        for t in 1..200 {
            assert!(c.step(&l, OK, t * 5).is_empty());
        }
        assert!(c.probe_works);
        assert_eq!(c.layout, split("en1", "en2"));
    }

    #[test]
    fn peer_using_the_management_cable_for_both_is_followed_after_verify() {
        let (mut c, l) = settled_split();
        c.step(&l, OK, 5);
        let p = Probe { rdma: Some(false), bridge: Some(true) };
        c.step(&l, p, 10);
        c.step(&l, p, 50);
        assert_eq!(c.layout, split("en1", "en2"), "44 s is not enough");
        let log = c.step(&l, p, 55);
        assert_eq!(c.layout, single("en2"));
        assert!(log[0].contains("it is using en2 for both"), "{log:?}");
        // Held while the other Mac's RDMA address answers on en2.
        for t in 1..100 {
            c.step(&l, OK, 55 + t * 5);
        }
        assert_eq!(c.layout, single("en2"));
    }

    #[test]
    fn fallback_releases_when_the_peer_leaves() {
        let (mut c, l) = settled_split();
        c.step(&l, OK, 5);
        let p = Probe { rdma: Some(false), bridge: Some(true) };
        c.step(&l, p, 10);
        c.step(&l, p, 55);
        assert_eq!(c.layout, single("en2"));
        // The other Mac moved its RDMA address away from en2.
        c.step(&l, p, 60);
        c.step(&l, p, 70);
        assert_eq!(c.layout, single("en2"));
        let log = c.step(&l, p, 75);
        assert_eq!(c.layout, split("en1", "en2"));
        assert!(log[0].contains("the other Mac left en2"), "{log:?}");
    }

    #[test]
    fn peer_using_the_inference_cable_for_both_is_followed() {
        let (mut c, l) = settled_split();
        c.step(&l, OK, 5);
        let p = Probe { rdma: Some(true), bridge: Some(false) };
        c.step(&l, p, 10);
        c.step(&l, p, 55);
        assert_eq!(c.layout, single("en1"));
        assert!(matches!(c.adapt, Some(Adapt::Fallback { .. })));
    }

    #[test]
    fn peer_down_changes_nothing_for_the_leader() {
        let (mut c, l) = settled_split();
        c.step(&l, OK, 5);
        let down = Probe { rdma: Some(false), bridge: Some(false) };
        for t in 2..500 {
            c.step(&l, down, t * 5);
        }
        assert_eq!(c.layout, split("en1", "en2"));
    }

    #[test]
    fn peer_down_with_unequal_cables_changes_nothing_for_the_follower() {
        let (mut c, l) = settled_split();
        c.leader = false;
        c.step(&l, OK, 5);
        let down = Probe { rdma: Some(false), bridge: Some(false) };
        for t in 2..500 {
            c.step(&l, down, t * 5);
        }
        assert_eq!(c.layout, split("en1", "en2"));
    }

    #[test]
    fn follower_swaps_a_tie_when_nothing_answers() {
        let mut c = Controller::new(false, true, Tuning::default());
        let l = [link("en2", 40, "B"), link("en1", 40, "A")];
        c.step(&l, NO, 0);
        assert_eq!(c.layout, split("en1", "en2"));
        c.step(&l, OK, 5);
        let none = Probe { rdma: Some(false), bridge: Some(false) };
        c.step(&l, none, 10);
        c.step(&l, none, 55);
        assert_eq!(c.layout, split("en2", "en1"));
        // Answers now: stays swapped.
        for t in 12..100 {
            c.step(&l, OK, t * 5);
        }
        assert_eq!(c.layout, split("en2", "en1"));
    }

    #[test]
    fn leader_never_swaps_a_tie() {
        let mut c = ctl();
        let l = [link("en2", 40, "B"), link("en1", 40, "A")];
        c.step(&l, NO, 0);
        c.step(&l, OK, 5);
        let none = Probe { rdma: Some(false), bridge: Some(false) };
        for t in 2..200 {
            c.step(&l, none, t * 5);
        }
        assert_eq!(c.layout, split("en1", "en2"));
    }

    #[test]
    fn probes_mean_nothing_until_the_peer_has_answered_once() {
        // Remote Login off on the other Mac: every probe fails, roles follow the ranking alone.
        let mut c = Controller::new(false, true, Tuning::default());
        let l = [link("en2", 40, "B"), link("en1", 40, "A")];
        c.step(&l, NO, 0);
        for p in [Probe { rdma: Some(false), bridge: Some(false) }, Probe { rdma: Some(false), bridge: None }] {
            for t in 1..300 {
                c.step(&l, p, t * 5);
            }
        }
        assert_eq!(c.layout, split("en1", "en2"));
        assert!(c.adapt.is_none());
    }

    #[test]
    fn unprobed_ticks_reset_the_failure_clock() {
        let (mut c, l) = settled_split();
        c.step(&l, OK, 5);
        let p = Probe { rdma: Some(false), bridge: Some(true) };
        c.step(&l, p, 10);
        c.step(&l, NO, 40); // addresses moving: not probed
        c.step(&l, p, 45);
        c.step(&l, p, 85);
        assert_eq!(c.layout, split("en1", "en2"));
        c.step(&l, p, 90);
        assert_eq!(c.layout, single("en2"));
    }

    #[test]
    fn fallback_is_dropped_when_the_cables_change() {
        let (mut c, l) = settled_split();
        c.step(&l, OK, 5);
        let p = Probe { rdma: Some(false), bridge: Some(true) };
        c.step(&l, p, 10);
        c.step(&l, p, 55);
        assert_eq!(c.layout, single("en2"));
        // A third cable appears: the ranking is different, the evidence is stale.
        let three = [link("en2", 40, "B"), link("en1", 80, "A"), link("en6", 80, "C")];
        let log = c.step(&three, NO, 60);
        assert!(c.adapt.is_none());
        assert!(log[0].contains("cables changed"), "{log:?}");
    }

    #[test]
    fn single_cable_never_adapts() {
        let mut c = ctl();
        let l = [link("en2", 40, "B")];
        c.step(&l, NO, 0);
        for t in 1..200 {
            c.step(&l, Probe { rdma: Some(t % 2 == 0), bridge: Some(false) }, t * 5);
        }
        assert_eq!(c.layout, single("en2"));
    }

    // ---- two Macs ----
    //
    // A small simulation: cables with an id, a speed and a port on each Mac,
    // visible to each Mac or not; each Mac runs its own controller on its own
    // clock; a probe answers iff the other Mac's address of that kind sits on
    // the same cable.

    #[derive(Clone)]
    struct Cable {
        id: &'static str,
        speed: u32,
        port: [&'static str; 2],
        seen: [bool; 2],
        /// The id each Mac reads for it (normally the same).
        ids: [&'static str; 2],
    }
    fn cable(id: &'static str, speed: u32, a: &'static str, b: &'static str) -> Cable {
        Cable { id, speed, port: [a, b], seen: [true, true], ids: [id, id] }
    }

    struct Sim {
        macs: [Controller; 2],
        cables: Vec<Cable>,
        up: [bool; 2],
        answers: [bool; 2],
        /// Whether the addresses settled after the last tick (they take a tick to move).
        settled: [bool; 2],
        changes: [u32; 2],
        t: u64,
        log: Vec<String>,
    }

    impl Sim {
        fn new(cables: Vec<Cable>) -> Sim {
            Sim {
                macs: [Controller::new(true, true, Tuning::default()), Controller::new(false, true, Tuning::default())],
                cables,
                up: [true, true],
                answers: [true, true],
                settled: [false, false],
                changes: [0, 0],
                t: 0,
                log: Vec::new(),
            }
        }
        fn links(&self, m: usize) -> Vec<Link> {
            self.cables.iter().filter(|c| c.seen[m]).map(|c| link(c.port[m], c.speed, c.ids[m])).collect()
        }
        fn cable_of(&self, m: usize, port: Option<&str>) -> Option<&'static str> {
            let port = port?;
            self.cables.iter().find(|c| c.port[m] == port && c.seen[m]).map(|c| c.id)
        }
        fn probe(&self, m: usize) -> Probe {
            let o = 1 - m;
            if !self.settled[m] || self.links(m).len() < 2 {
                return Probe::default();
            }
            let me = &self.macs[m].layout;
            let them = &self.macs[o].layout;
            let alive = self.up[o] && self.answers[o];
            let rdma = me.rdma_port().map(|_| {
                alive
                    && self.cable_of(m, me.rdma_port()).is_some()
                    && self.cable_of(m, me.rdma_port()) == self.cable_of(o, them.rdma_port())
            });
            let bridge = me.bridge_port().map(|_| {
                alive
                    && self.cable_of(m, me.bridge_port()).is_some()
                    && self.cable_of(m, me.bridge_port()) == self.cable_of(o, them.bridge_port())
            });
            Probe { rdma, bridge }
        }
        /// Advance 1 s; each Mac ticks every 5 s, Mac 1 offset by `skew`.
        fn second(&mut self, skew: u64) {
            self.t += 1;
            for m in 0..2 {
                let offset = if m == 0 { 0 } else { skew };
                if !self.up[m] || !(self.t + offset).is_multiple_of(5) {
                    continue;
                }
                let p = self.probe(m);
                let before = self.macs[m].layout.clone();
                let links = self.links(m);
                for line in self.macs[m].step(&links, p, self.t) {
                    self.log.push(format!(
                        "t={} mac{m} links={:?} probe={:?}: {line}",
                        self.t,
                        links.iter().map(|l| l.port.as_str()).collect::<Vec<_>>(),
                        p
                    ));
                }
                let changed = self.macs[m].layout != before;
                if changed {
                    self.changes[m] += 1;
                }
                self.settled[m] = !changed;
            }
        }
        fn run(&mut self, secs: u64, skew: u64) {
            for _ in 0..secs {
                self.second(skew);
            }
        }
        /// The cable each Mac uses for (inference, management).
        fn roles(&self, m: usize) -> (Option<&'static str>, Option<&'static str>) {
            let l = &self.macs[m].layout;
            (self.cable_of(m, l.rdma_port()), self.cable_of(m, l.bridge_port()))
        }
        fn agree(&self) -> bool {
            self.roles(0) == self.roles(1)
        }
    }

    fn two_cables() -> Vec<Cable> {
        // TB5 on Mac A's en6 and Mac B's en1; TB4 on Mac A's en1 and Mac B's en2.
        vec![cable("tb5", 80, "en6", "en1"), cable("tb4", 40, "en1", "en2")]
    }

    #[test]
    fn two_macs_two_speeds_agree_on_the_fast_cable() {
        for skew in 0..5 {
            let mut s = Sim::new(two_cables());
            s.run(600, skew);
            assert!(s.agree(), "skew {skew}");
            assert_eq!(s.roles(0), (Some("tb5"), Some("tb4")));
            assert_eq!(s.changes, [1, 1]);
        }
    }

    #[test]
    fn two_macs_identical_cables_agree() {
        for skew in 0..5 {
            let mut s = Sim::new(vec![cable("x", 40, "en6", "en1"), cable("y", 40, "en1", "en2")]);
            s.run(600, skew);
            assert!(s.agree());
            assert_eq!(s.roles(0), (Some("x"), Some("y")));
            assert_eq!(s.changes, [1, 1]);
        }
    }

    #[test]
    fn two_macs_one_cable_behave_as_before() {
        let mut s = Sim::new(vec![cable("tb4", 40, "en1", "en2")]);
        s.run(600, 2);
        assert_eq!(s.roles(0), (Some("tb4"), Some("tb4")));
        assert!(s.agree());
        assert_eq!(s.macs[0].layout, single("en1"));
        assert_eq!(s.macs[1].layout, single("en2"));
    }

    #[test]
    fn two_macs_fail_over_and_back() {
        let mut s = Sim::new(two_cables());
        s.run(100, 3);
        s.cables[0].seen = [false, false];
        s.run(10, 3);
        assert!(s.agree());
        assert_eq!(s.roles(0), (Some("tb4"), Some("tb4")));
        s.cables[0].seen = [true, true];
        // Lost within the flap window of moving onto it, so the wait is 60 s, not 30.
        s.run(50, 3);
        assert_eq!(s.roles(0), (Some("tb4"), Some("tb4")), "not back before the settle time");
        s.run(20, 3);
        assert!(s.agree());
        assert_eq!(s.roles(0), (Some("tb5"), Some("tb4")));
        assert_eq!(s.changes, [3, 3]);
    }

    #[test]
    fn two_macs_flapping_cable_stays_bounded_and_ends_in_agreement() {
        let mut s = Sim::new(two_cables());
        s.run(100, 1);
        for i in 0..90 {
            s.cables[0].seen = if i % 2 == 0 { [false, false] } else { [true, true] };
            s.run(20, 1);
        }
        s.cables[0].seen = [true, true];
        s.run(1300, 1);
        assert!(s.agree());
        assert_eq!(s.roles(0), (Some("tb5"), Some("tb4")));
        assert!(s.changes.iter().all(|&n| n <= 5), "{:?}", s.changes);
    }

    #[test]
    fn two_macs_cable_moves_on_one_mac() {
        let mut s = Sim::new(two_cables());
        s.run(100, 2);
        // Mac B moves the TB5 cable from en1 to en6. It gets a new id (a different bus on Mac B).
        s.cables[0].seen = [false, false];
        s.run(5, 2);
        s.cables[0] = Cable { port: ["en6", "en6"], ..cable("tb5b", 80, "en6", "en6") };
        s.run(120, 2);
        assert!(s.agree());
        assert_eq!(s.roles(0), (Some("tb5b"), Some("tb4")));
    }

    #[test]
    fn two_macs_unplug_on_one_side_only_is_seen_by_both() {
        // A cable pulled at one end goes down at both; model the second Mac noticing a tick later.
        let mut s = Sim::new(two_cables());
        s.run(100, 0);
        s.cables[0].seen = [false, true];
        s.run(4, 0);
        s.cables[0].seen = [false, false];
        s.run(60, 0);
        assert!(s.agree());
        assert_eq!(s.roles(1), (Some("tb4"), Some("tb4")));
    }

    #[test]
    fn two_macs_peer_sees_only_the_slow_cable() {
        // The disagreement case: Mac B cannot use the TB5 cable (say, no RDMA on that port).
        for skew in [0, 2, 4] {
            let mut s = Sim::new(two_cables());
            s.cables[0].seen = [true, false];
            s.run(300, skew);
            assert!(s.agree(), "skew {skew}: {:?} vs {:?}", s.roles(0), s.roles(1));
            assert_eq!(s.roles(0), (Some("tb4"), Some("tb4")));
            // Later Mac B can use it too: both move to the dedicated link.
            s.cables[0].seen = [true, true];
            s.run(300, skew);
            assert!(s.agree(), "skew {skew}: {:?} vs {:?}", s.roles(0), s.roles(1));
            assert_eq!(s.roles(0), (Some("tb5"), Some("tb4")));
            // And stays there.
            let before = s.changes;
            s.run(3600, skew);
            assert_eq!(s.changes, before);
        }
    }

    #[test]
    fn two_macs_leader_sees_only_the_fast_cable() {
        for skew in [0, 3] {
            let mut s = Sim::new(two_cables());
            s.cables[1].seen = [false, true];
            s.run(300, skew);
            assert!(s.agree(), "skew {skew}: {:?} vs {:?}", s.roles(0), s.roles(1));
            assert_eq!(s.roles(1), (Some("tb5"), Some("tb5")));
        }
    }

    #[test]
    fn two_macs_break_a_tie_differently() {
        // If the Macs ever read different ids for identical cables, the follower swaps.
        for skew in [0, 1, 4] {
            let mut s = Sim::new(vec![cable("x", 40, "en6", "en1"), cable("y", 40, "en1", "en2")]);
            s.cables[0].ids = ["x", "y"];
            s.cables[1].ids = ["y", "x"];
            s.run(30, skew);
            assert!(!s.agree());
            // The ranking alone disagrees; the probes need one answer to trust them.
            s.macs[0].probe_works = true;
            s.macs[1].probe_works = true;
            s.run(300, skew);
            assert!(s.agree(), "skew {skew}: {:?} vs {:?}", s.roles(0), s.roles(1));
            let before = s.changes;
            s.run(3600, skew);
            assert_eq!(s.changes, before);
        }
    }

    #[test]
    fn two_macs_peer_reboot_returns_to_agreement() {
        let mut s = Sim::new(vec![cable("x", 40, "en6", "en1"), cable("y", 40, "en1", "en2")]);
        s.run(100, 2);
        assert!(s.macs[0].probe_works && s.macs[1].probe_works);
        s.up[1] = false;
        s.run(240, 2);
        s.up[1] = true;
        s.macs[1] = Controller::new(false, true, Tuning::default());
        s.run(300, 2);
        assert!(s.agree());
        assert_eq!(s.roles(0), (Some("x"), Some("y")));
        // And the leader reboots, while the follower toggles nothing (unequal or equal, the leader is the anchor).
        s.up[0] = false;
        s.run(240, 2);
        s.up[0] = true;
        s.macs[0] = Controller::new(true, true, Tuning::default());
        s.run(400, 2);
        assert!(s.agree(), "{:?} vs {:?}", s.roles(0), s.roles(1));
    }

    #[test]
    fn two_macs_random_events_always_end_in_agreement() {
        // Many random histories of plugs, pulls, one-sided views and reboots.
        // Once things stay put, both Macs must agree and stop changing.
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        let mut rnd = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for case in 0..300 {
            let identical = rnd(2) == 0;
            let mut s = Sim::new(vec![cable("a", if identical { 40 } else { 80 }, "en6", "en1"), cable("b", 40, "en1", "en2")]);
            let skew = rnd(5);
            for _ in 0..rnd(12) {
                match rnd(4) {
                    0 | 1 => {
                        let c = rnd(2) as usize;
                        let v = rnd(2) == 0;
                        s.cables[c].seen = [v, v];
                    }
                    2 => {
                        let c = rnd(2) as usize;
                        let m = rnd(2) as usize;
                        s.cables[c].seen[m] = !s.cables[c].seen[m];
                    }
                    _ => {
                        let m = rnd(2) as usize;
                        s.up[m] = !s.up[m];
                        if s.up[m] {
                            s.macs[m] = Controller::new(m == 0, true, Tuning::default());
                        }
                    }
                }
                s.run(1 + rnd(200), skew);
            }
            s.up = [true, true];
            s.run(2400, skew);
            let before = s.changes;
            s.run(1200, skew);
            let common = s.cables.iter().any(|c| c.seen == [true, true]);
            // A follower whose peer sees no cable at all may still try a tie swap, rarely.
            let allowed = if common { 0 } else { 1 };
            assert!(
                s.changes[0] - before[0] + s.changes[1] - before[1] <= allowed,
                "case {case}: still changing\n{}\n{:?} {:?} / {:?} {:?} cables {:?}",
                s.log[s.log.len().saturating_sub(12)..].join("\n"),
                s.macs[0].layout,
                s.macs[0].adapt,
                s.macs[1].layout,
                s.macs[1].adapt,
                s.cables.iter().map(|c| (c.id, c.seen)).collect::<Vec<_>>()
            );
            let usable: Vec<_> = s.cables.iter().filter(|c| c.seen[0] && c.seen[1]).map(|c| c.id).collect();
            if !usable.is_empty() {
                assert!(s.agree(), "case {case}: {:?} vs {:?} with {usable:?}", s.roles(0), s.roles(1));
                assert!(usable.contains(&s.roles(0).0.unwrap()), "case {case}: inference on a cable one Mac cannot use");
            }
            if s.cables.iter().all(|c| c.seen == [true, true]) {
                assert_eq!(
                    s.roles(0),
                    (Some("a"), Some("b")),
                    "case {case}: both cables usable, so the split must win: {:?} {:?} / {:?} {:?}\n{}",
                    s.macs[0].layout,
                    s.macs[0].adapt,
                    s.macs[1].layout,
                    s.macs[1].adapt,
                    s.log.join("\n")
                );
            }
        }
    }

    #[test]
    fn two_macs_without_probe_answers_follow_the_ranking() {
        let mut s = Sim::new(two_cables());
        s.answers = [false, false];
        s.cables[0].seen = [true, false];
        s.run(600, 1);
        // Nothing to go on: each Mac follows its own ranking, and nothing wanders.
        assert_eq!(s.changes, [1, 1]);
        assert_eq!(s.roles(0), (Some("tb5"), Some("tb4")));
        assert_eq!(s.roles(1), (Some("tb4"), Some("tb4")));
    }
}
