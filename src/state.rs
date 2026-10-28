//! Tracking across samples: peaks, rates, growth streaks and restarts, per row. Memory is one
//! Track per row; samples are not retained.

use crate::model::{daemon_account, node_account, DaemonAccount, NodeAccount, Row};
use crate::peaks::{AuthPeak, Track};
use crate::raw::RawSample;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize)]
pub struct TrackedRow {
    #[serde(flatten)]
    pub row: Row,
    pub track: Track,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct NodeView {
    pub total: Option<u64>,
    pub rows: Vec<TrackedRow>,
    pub residual_pct: Option<u64>,
    pub problems: Vec<String>,
    pub degraded: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DaemonView {
    pub account: DaemonAccount,
    pub rows: Vec<TrackedRow>,
    pub socket: Option<String>,
    pub version: Option<String>,
    pub fsid: Option<String>,
    pub rss: Track,
    /// The kernel's own peak RSS (VmHWM).
    pub rss_kernel_peak: AuthPeak,
    pub cgroup: Track,
    /// memory.peak of the cgroup, which outlives the process.
    pub cgroup_kernel_peak: AuthPeak,
    pub errors: Vec<String>,
    pub conf: Option<String>,
    pub bindir: Option<String>,
    pub stale: bool,
    pub degraded: bool,
    /// Wall time of the last restart seen, and whether to announce it.
    pub restarted_at: Option<u64>,
    pub spread_ms: u64,
}

/// The verdict on one sample: both identities, the hard cross-checks and the residual bound.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Checks {
    pub ok: bool,
    pub failures: Vec<String>,
    /// Daemons left out of the verdict because they could not be read.
    pub unchecked: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct View {
    pub checks: Checks,
    pub seq: u64,
    pub wall_ms: u64,
    pub mono_ms: u64,
    /// Measured interval between this and the previous sample, ms: the resolution of observed peaks.
    pub sampling_ms: Option<u64>,
    pub node: NodeView,
    pub daemons: Vec<DaemonView>,
}

#[derive(Default)]
struct DaemonState {
    pid: Option<u32>,
    starttime: u64,
    rows: BTreeMap<String, Track>,
    rss: Track,
    rss_auth: AuthPeak,
    cg: Track,
    cg_auth: AuthPeak,
    restarted_at: Option<u64>,
    restart_ticks_left: u8,
}

/// One sample with its accounting, computed once; tracking and views derive from it.
struct Snapshot {
    sample: RawSample,
    sampling_ms: Option<u64>,
    node: NodeAccount,
    /// In the sample's daemon order.
    daemons: Vec<DaemonAccount>,
}

#[derive(Default)]
pub struct State {
    node: BTreeMap<String, Track>,
    daemons: BTreeMap<String, DaemonState>,
    /// The identity (pid, start time) of daemons that dropped out of the sample, so one that
    /// comes back with a new pid is announced as restarted rather than taken for new.
    gone: BTreeMap<String, (Option<u32>, u64)>,
    last_mono: Option<u64>,
    /// The last snapshot, so the view can be rebuilt after a reset without new reads.
    last: Option<Snapshot>,
    /// Reads spread wider than this (ms) make a sample degraded.
    pub max_spread_ms: u64,
    /// Largest acceptable residual, percent of the level's total.
    pub tolerance_pct: u64,
}

/// Verify one view: node rows = MemTotal, each readable daemon's rows = its RSS, the hard
/// cross-checks, and the residuals within tolerance.
pub fn verify(v: &View, tol: u64) -> Checks {
    let mut c = Checks {
        ok: true,
        ..Default::default()
    };
    let n = &v.node;
    let sum: Option<i128> = n
        .rows
        .iter()
        .filter_map(|r| r.row.contribution())
        .fold(Some(0i128), |a, x| a.map(|y| y + x));
    match (sum, n.total) {
        (Some(s), Some(t)) if s == t as i128 => {}
        (Some(s), Some(t)) => c
            .failures
            .push(format!("node: rows sum {} != MemTotal {}", s, t)),
        _ => c
            .failures
            .push("node: identity not closable (missing input)".into()),
    }
    match n.residual_pct {
        Some(p) if p > tol => c.failures.push(format!("node: residual {}% > {}%", p, tol)),
        None => c.failures.push("node: residual unknown".into()),
        _ => {}
    }
    for p in &n.problems {
        c.failures.push(format!("node: {}", p));
    }
    for d in &v.daemons {
        let a = &d.account;
        if d.socket.is_none() || !d.errors.is_empty() || a.rss.is_none() {
            c.unchecked.push(a.name.clone());
            continue;
        }
        let rss = a.rss.unwrap();
        let complete = d
            .rows
            .iter()
            .filter(|r| !r.row.reference && !r.row.optional)
            .all(|r| r.row.value.is_some() || r.row.signed.is_some());
        let sum: Option<i128> = d
            .rows
            .iter()
            .filter_map(|r| r.row.contribution())
            .fold(Some(0i128), |a, x| a.map(|y| y + x));
        match sum {
            Some(s) if complete && s == rss as i128 => {}
            Some(s) if complete => c
                .failures
                .push(format!("{}: rows sum {} != rss {}", a.name, s, rss)),
            _ => c
                .failures
                .push(format!("{}: identity not closable", a.name)),
        }
        match a.residual_pct {
            Some(p) if p > tol => c
                .failures
                .push(format!("{}: residual {}% > {}%", a.name, p, tol)),
            None => c.failures.push(format!("{}: residual unknown", a.name)),
            _ => {}
        }
        for p in &a.problems {
            c.failures.push(format!("{}: {}", a.name, p));
        }
    }
    c.ok = c.failures.is_empty();
    c
}

impl State {
    pub fn new(max_spread_ms: u64, tolerance_pct: u64) -> State {
        State {
            max_spread_ms,
            tolerance_pct,
            ..Default::default()
        }
    }

    /// Feed one sample: account for it once, update every track from it, then build the view.
    pub fn ingest(&mut self, s: &RawSample) -> View {
        let sampling_ms = self.last_mono.map(|m| s.mono_ms.saturating_sub(m));
        self.last_mono = Some(s.mono_ms);
        let node_good = s.spread_ms <= self.max_spread_ms;
        let na = node_account(s);
        let mut seen = std::collections::HashSet::new();
        for r in &na.rows {
            self.node
                .entry(r.id.clone())
                .or_default()
                .update(r.value, s.mono_ms, s.wall_ms, node_good);
            seen.insert(r.id.clone());
        }
        self.node.retain(|k, _| seen.contains(k));
        let mut names = std::collections::HashSet::new();
        let mut accounts = Vec::with_capacity(s.daemons.len());
        for d in &s.daemons {
            names.insert(d.name.clone());
            let p = d.pid.and_then(|pid| s.procs.iter().find(|p| p.pid == pid));
            let returned = self.gone.remove(&d.name);
            let st = self.daemons.entry(d.name.clone()).or_default();
            if let (Some((pid, starttime)), None) = (returned, st.pid) {
                // back after a tick without a socket: judge it against its identity from before
                st.pid = pid;
                st.starttime = starttime;
            }
            // a new pid or a new start time for the same pid is a restart: nothing from before applies
            let restarted = match (st.pid, p) {
                (Some(old), Some(p)) => old != p.pid || st.starttime != p.starttime,
                (Some(_), None) => true,
                (None, _) => false,
            };
            if restarted {
                let keep_cg = (st.cg.clone(), st.cg_auth.clone());
                *st = DaemonState {
                    restarted_at: Some(s.wall_ms),
                    restart_ticks_left: 10,
                    ..Default::default()
                };
                st.cg = keep_cg.0;
                st.cg_auth = keep_cg.1;
            } else if st.restart_ticks_left > 0 {
                st.restart_ticks_left -= 1;
            }
            st.pid = p.map(|p| p.pid);
            st.starttime = p.map_or(0, |p| p.starttime);
            let good = !d.stale && d.spread_ms <= self.max_spread_ms && node_good;
            let acc = daemon_account(d, p, s);
            let mut seen = std::collections::HashSet::new();
            for r in &acc.rows {
                st.rows
                    .entry(r.id.clone())
                    .or_default()
                    .update(r.value, s.mono_ms, s.wall_ms, good);
                seen.insert(r.id.clone());
            }
            st.rows.retain(|k, _| seen.contains(k));
            st.rss.update(acc.rss, s.mono_ms, s.wall_ms, node_good);
            st.rss_auth.update(acc.vm_hwm, s.wall_ms);
            st.cg.update(
                acc.cgroup.as_ref().and_then(|c| c.current),
                s.mono_ms,
                s.wall_ms,
                node_good,
            );
            st.cg_auth
                .update(acc.cgroup.as_ref().and_then(|c| c.peak), s.wall_ms);
            accounts.push(acc);
        }
        for (k, st) in &self.daemons {
            if !names.contains(k) {
                self.gone.insert(k.clone(), (st.pid, st.starttime));
            }
        }
        self.daemons.retain(|k, _| names.contains(k));
        self.last = Some(Snapshot {
            sample: s.clone(),
            sampling_ms,
            node: na,
            daemons: accounts,
        });
        self.build()
    }

    /// The view of the last snapshot with the tracks as they are now: after a peak reset, the
    /// summary and the tables show it at once instead of at the next sample. No reads.
    pub fn refresh(&self) -> Option<View> {
        self.last.as_ref().map(|_| self.build())
    }

    fn build(&self) -> View {
        let snap = self.last.as_ref().expect("ingest before build");
        let s = &snap.sample;
        let node_good = s.spread_ms <= self.max_spread_ms;
        let na = &snap.node;
        let node = NodeView {
            total: na.total,
            residual_pct: na.residual_pct,
            problems: na.problems.clone(),
            degraded: !node_good,
            rows: na
                .rows
                .iter()
                .map(|r| TrackedRow {
                    row: r.clone(),
                    track: self.node.get(&r.id).cloned().unwrap_or_default(),
                })
                .collect(),
        };
        let mut daemons = Vec::new();
        for (d, acc) in s.daemons.iter().zip(snap.daemons.iter()) {
            let st = self.daemons.get(&d.name);
            let good = !d.stale && d.spread_ms <= self.max_spread_ms && node_good;
            let rows = acc
                .rows
                .iter()
                .map(|r| TrackedRow {
                    row: r.clone(),
                    track: st
                        .and_then(|st| st.rows.get(&r.id).cloned())
                        .unwrap_or_default(),
                })
                .collect();
            daemons.push(DaemonView {
                socket: d.socket.clone(),
                version: d.version.clone(),
                fsid: d.fsid.clone(),
                rss: st.map(|st| st.rss.clone()).unwrap_or_default(),
                rss_kernel_peak: st.map(|st| st.rss_auth.clone()).unwrap_or_default(),
                cgroup: st.map(|st| st.cg.clone()).unwrap_or_default(),
                cgroup_kernel_peak: st.map(|st| st.cg_auth.clone()).unwrap_or_default(),
                errors: d.errors.clone(),
                conf: d.conf.clone(),
                bindir: d.bindir.clone(),
                stale: d.stale,
                degraded: !good,
                restarted_at: st.and_then(|st| {
                    if st.restart_ticks_left > 0 {
                        st.restarted_at
                    } else {
                        None
                    }
                }),
                spread_ms: d.spread_ms,
                account: acc.clone(),
                rows,
            });
        }
        daemons.sort_by(|a, b| a.account.name.cmp(&b.account.name));
        let mut v = View {
            checks: Checks::default(),
            seq: s.seq,
            wall_ms: s.wall_ms,
            mono_ms: s.mono_ms,
            sampling_ms: snap.sampling_ms,
            node,
            daemons,
        };
        v.checks = verify(&v, self.tolerance_pct);
        v
    }

    /// Clear every observed peak (kernel peaks are not ours to clear).
    pub fn reset_all_peaks(&mut self) {
        for t in self.node.values_mut() {
            t.reset_peak();
        }
        for d in self.daemons.values_mut() {
            d.rss.reset_peak();
            d.cg.reset_peak();
            for t in d.rows.values_mut() {
                t.reset_peak();
            }
        }
    }

    /// Clear one row's observed peak: a node row, a daemon's RSS, or a row inside a daemon.
    pub fn reset_peak(&mut self, daemon: Option<&str>, row: &str) {
        match daemon {
            None => {
                if let Some(t) = self.node.get_mut(row) {
                    t.reset_peak();
                }
            }
            Some(d) => {
                if let Some(st) = self.daemons.get_mut(d) {
                    if row == "rss" {
                        st.rss.reset_peak();
                        st.cg.reset_peak();
                    } else if let Some(t) = st.rows.get_mut(row) {
                        t.reset_peak();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::{MemInfo, ProcInfo};
    use crate::raw::RawDaemon;

    fn sample(seq: u64, anon: u64, pid: u32, starttime: u64, pglog: u64) -> RawSample {
        let meminfo = MemInfo(
            [
                ("MemTotal", 10_000u64),
                ("MemFree", 1000),
                ("Cached", 1000),
                ("Buffers", 0),
                ("Shmem", 0),
                ("Slab", 500),
                ("KernelStack", 0),
                ("PageTables", 0),
                ("Percpu", 0),
                ("AnonPages", anon + 100),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect(),
        );
        let p = ProcInfo {
            pid,
            comm: "ceph-osd".into(),
            starttime,
            cmdline: vec!["ceph-osd".into(), "-i".into(), "0".into()],
            rss: Some(anon + 10),
            rss_anon: Some(anon),
            rss_file: Some(10),
            rss_shmem: Some(0),
            vm_hwm: Some(anon + 50),
            ..Default::default()
        };
        let d = RawDaemon {
            name: "osd.0".into(),
            kind: "osd".into(),
            pid: Some(pid),
            mempools: Some(
                serde_json::json!({"mempool": {"by_pool": {"osd_pglog": {"items": 1, "bytes": pglog}}}}),
            ),
            ..Default::default()
        };
        RawSample {
            seq,
            wall_ms: 1_000_000 + seq * 2000,
            mono_ms: seq * 2000,
            meminfo,
            procs: vec![p],
            daemons: vec![d],
            ..Default::default()
        }
    }

    #[test]
    fn a_daemon_back_after_a_tick_away_with_a_new_pid_is_a_restart() {
        let mut st = State::new(500, 100);
        st.ingest(&sample(0, 500, 7, 100, 20));
        let mut away = sample(1, 500, 7, 100, 20);
        away.daemons.clear();
        away.procs.clear();
        assert!(st.ingest(&away).daemons.is_empty());
        let v = st.ingest(&sample(2, 300, 8, 300, 10));
        let d = &v.daemons[0];
        assert_eq!(d.restarted_at, Some(1_004_000), "announced");
        assert_eq!(
            d.rss.peak,
            Some((310, 1_004_000)),
            "and the old peak is gone"
        );
    }

    #[test]
    fn peaks_follow_rows_and_restarts_clear_them() {
        let mut st = State::new(500, 100);
        let v1 = st.ingest(&sample(0, 500, 7, 100, 20));
        assert_eq!(v1.sampling_ms, None);
        let v2 = st.ingest(&sample(1, 900, 7, 100, 60));
        assert_eq!(v2.sampling_ms, Some(2000));
        let d = &v2.daemons[0];
        assert_eq!(d.rss.peak, Some((910, 1_002_000)));
        assert_eq!(d.rss_kernel_peak.value, Some(950));
        let pg = d.rows.iter().find(|r| r.row.id == "nocache:pglog").unwrap();
        assert_eq!(pg.track.peak, Some((60, 1_002_000)));
        assert_eq!(pg.track.rate, Some(20));
        // same pid, new start time: a restart
        let v3 = st.ingest(&sample(2, 300, 7, 200, 10));
        let d = &v3.daemons[0];
        assert_eq!(d.restarted_at, Some(1_004_000));
        assert_eq!(d.rss.peak, Some((310, 1_004_000)), "the old peak is gone");
        assert_eq!(d.rss_kernel_peak.value, Some(350));
        let pg = d.rows.iter().find(|r| r.row.id == "nocache:pglog").unwrap();
        assert_eq!(pg.track.rate, None, "no rate across a restart");
        assert_eq!(pg.track.peak, Some((10, 1_004_000)));
    }

    #[test]
    fn totals_do_not_sum_peaks() {
        // two rows peaking at different moments: the total's peak is its own maximum, below the sum of the row peaks
        let mut st = State::new(500, 100);
        st.ingest(&sample(0, 500, 7, 100, 100));
        st.ingest(&sample(1, 500, 7, 100, 10));
        let v = st.ingest(&sample(2, 200, 7, 100, 10));
        let d = &v.daemons[0];
        let rows_peak_sum: u64 = d
            .rows
            .iter()
            .filter(|r| !r.row.reference)
            .filter_map(|r| r.track.peak.map(|p| p.0))
            .sum();
        assert_eq!(d.rss.peak.unwrap().0, 510);
        assert!(
            rows_peak_sum > 510,
            "the parts peaked apart: {} > 510",
            rows_peak_sum
        );
    }

    #[test]
    fn reset_clears_only_observed_peaks() {
        let mut st = State::new(500, 100);
        st.ingest(&sample(0, 500, 7, 100, 100));
        st.reset_all_peaks();
        let v = st.ingest(&sample(1, 100, 7, 100, 5));
        let d = &v.daemons[0];
        assert_eq!(d.rss.peak, Some((110, 1_002_000)));
        assert_eq!(
            d.rss_kernel_peak.value,
            Some(550),
            "the kernel's VmHWM is not ours to reset"
        );
        st.reset_peak(Some("osd.0"), "nocache:pglog");
        let v = st.ingest(&sample(2, 100, 7, 100, 3));
        let pg = v.daemons[0]
            .rows
            .iter()
            .find(|r| r.row.id == "nocache:pglog")
            .unwrap();
        assert_eq!(pg.track.peak, Some((3, 1_004_000)));
    }

    #[test]
    fn a_refresh_shows_a_reset_before_the_next_sample() {
        let mut st = State::new(500, 100);
        st.ingest(&sample(0, 500, 7, 100, 100));
        let v = st.ingest(&sample(1, 100, 7, 100, 5));
        assert_eq!(v.daemons[0].rss.peak.map(|p| p.0), Some(510));
        st.reset_all_peaks();
        let r = st.refresh().unwrap();
        assert_eq!(
            r.daemons[0].rss.peak, None,
            "the summary and tables show the reset now"
        );
        assert_eq!(r.daemons[0].rss.cur, Some(110));
        assert_eq!(r.sampling_ms, Some(2000));
        assert!(r.checks.ok, "{:?}", r.checks.failures);
    }

    #[test]
    fn degraded_samples_are_shown_but_not_tracked() {
        let mut st = State::new(500, 100);
        st.ingest(&sample(0, 500, 7, 100, 100));
        let mut s = sample(1, 900, 7, 100, 900);
        s.spread_ms = 5000;
        let v = st.ingest(&s);
        assert!(v.node.degraded && v.daemons[0].degraded);
        assert_eq!(v.daemons[0].rss.cur, Some(910));
        assert_eq!(v.daemons[0].rss.peak, Some((510, 1_000_000)));
    }
}
