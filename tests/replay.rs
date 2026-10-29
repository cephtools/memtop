//! Black-box tests through the binary on recorded fixtures: every sample passes its checks,
//! replay is deterministic, peaks come from the samples and not from the parts, restarts reset,
//! a failed check fails the run.

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ceph-memtop"))
}

fn fixtures(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

fn json_lines(args: &[&str]) -> Vec<serde_json::Value> {
    let out = bin().args(args).output().expect("run");
    assert!(
        out.status.success(),
        "{:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("json line"))
        .collect()
}

/// Replay with these arguments; every sample must carry a passing verdict and the run exit 0.
fn all_samples_pass(args: &[&str]) -> Vec<serde_json::Value> {
    let views = json_lines(args);
    assert!(!views.is_empty());
    for v in &views {
        assert_eq!(
            v["checks"]["ok"], true,
            "sample {}: {}",
            v["seq"], v["checks"]["failures"]
        );
    }
    views
}

fn load_fixture(name: &str) -> Vec<serde_json::Value> {
    let mut files: Vec<_> = std::fs::read_dir(fixtures(name))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    files
        .iter()
        .map(|f| serde_json::from_slice(&std::fs::read(f).unwrap()).unwrap())
        .collect()
}

fn daemon<'a>(v: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    v["daemons"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["account"]["name"] == name)
        .unwrap_or_else(|| panic!("no {}", name))
}

fn write_fixture(dir: &Path, samples: &[serde_json::Value]) {
    std::fs::create_dir_all(dir).unwrap();
    for (i, s) in samples.iter().enumerate() {
        std::fs::write(
            dir.join(format!("sample-{:06}.json", i)),
            serde_json::to_vec(s).unwrap(),
        )
        .unwrap();
    }
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("memtop-test-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn set_osd0_proc(s: &mut serde_json::Value, f: impl Fn(&mut serde_json::Value)) {
    for p in s["procs"].as_array_mut().unwrap() {
        if p["comm"] == "ceph-osd"
            && p["cmdline"]
                .as_array()
                .unwrap()
                .windows(2)
                .any(|w| w[0] == "-i" && w[1] == "0")
        {
            f(p);
        }
    }
}

#[test]
fn every_recorded_sample_passes_its_checks() {
    for fx in ["vstart-idle", "vstart-write"] {
        all_samples_pass(&[
            "--replay",
            fixtures(fx).to_str().unwrap(),
            "--json",
            "--tolerance",
            "100",
        ]);
    }
}

#[test]
fn replay_is_deterministic_and_the_peak_is_the_sample_maximum() {
    let fx = fixtures("vstart-write");
    let a = json_lines(&["--replay", fx.to_str().unwrap(), "--json"]);
    let b = json_lines(&["--replay", fx.to_str().unwrap(), "--json"]);
    assert_eq!(a, b, "two replays differ");
    let samples = load_fixture("vstart-write");
    assert_eq!(a.len(), samples.len());
    // expected osd.0 RSS peak, computed here from the raw samples: the largest value and the first time it was seen
    let mut best: Option<(u64, u64)> = None;
    for s in &samples {
        let p = s["procs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| {
                p["comm"] == "ceph-osd"
                    && p["cmdline"]
                        .as_array()
                        .unwrap()
                        .windows(2)
                        .any(|w| w[0] == "-i" && w[1] == "0")
            })
            .unwrap();
        let rss = p["rss"].as_u64().unwrap();
        if best.is_none_or(|(v, _)| rss > v) {
            best = Some((rss, s["wall_ms"].as_u64().unwrap()));
        }
    }
    let (v, at) = best.unwrap();
    let last = a.last().unwrap();
    let d = daemon(last, "osd.0");
    assert_eq!(d["rss"]["peak"]["value"].as_u64(), Some(v));
    assert_eq!(d["rss"]["peak"]["at_ms"].as_u64(), Some(at));
    // the peak comes from the samples, not from the parts: the parts' peaks add up to more
    let parts: u64 = d["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["reference"] == false)
        .filter_map(|r| r["track"]["peak"]["value"].as_u64())
        .sum();
    assert!(parts >= v, "parts' peaks {} < rss peak {}", parts, v);
    // the memtable row is read from the recorded kv stats, and the pglog grew under the write load
    let mt = d["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "nocache:memtables")
        .unwrap();
    assert!(mt["value"].as_u64().is_some(), "memtable row n/a: {}", mt);
    let pg = d["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "nocache:pglog")
        .unwrap();
    assert!(pg["track"]["peak"]["value"].as_u64().unwrap() > 0);
    // kernel peaks are reported as such, with no time when they predate the tool
    assert!(d["rss_kernel_peak"]["value"].as_u64().unwrap() >= v);
    // both sides and their checks travel with the sample
    let acc = &d["account"];
    assert_eq!(acc["kernel"]["rss"].as_u64(), d["rss"]["cur"].as_u64());
    let ceph = &acc["ceph"];
    let (m, u, o, n) = (
        ceph["mempool_total"].as_u64().unwrap(),
        ceph["untracked"].as_u64().unwrap(),
        ceph["allocator_overhead"].as_u64().unwrap(),
        ceph["non_heap"].as_u64().unwrap(),
    );
    assert_eq!(
        m + u + o + n,
        d["rss"]["cur"].as_u64().unwrap(),
        "rss = mempool_total + untracked + allocator_overhead + non_heap"
    );
    assert!(
        acc["matches"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["ok"] == true || x["hard"] == false),
        "{}",
        acc["matches"]
    );
}

#[test]
fn a_restart_clears_peaks_and_rates_and_is_announced() {
    let base = load_fixture("vstart-idle");
    let mut seq = vec![base[0].clone(), base[1].clone()];
    // sample 1: a spike; sample 2: the same pid with a new start time and a small RSS
    set_osd0_proc(&mut seq[1], |p| {
        p["rss"] = serde_json::json!(900u64 << 20);
        p["rss_anon"] = serde_json::json!(800u64 << 20);
        p["vm_hwm"] = serde_json::json!(950u64 << 20);
    });
    let mut s2 = base[2].clone();
    set_osd0_proc(&mut s2, |p| {
        p["starttime"] = serde_json::json!(p["starttime"].as_u64().unwrap() + 5000);
        p["rss"] = serde_json::json!(50u64 << 20);
        p["rss_anon"] = serde_json::json!(40u64 << 20);
        p["vm_hwm"] = serde_json::json!(55u64 << 20);
    });
    seq.push(s2);
    let dir = tmp("restart");
    write_fixture(&dir, &seq);
    let out = bin()
        .args(["--replay", dir.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    let views: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(views.len(), 3);
    let before = daemon(&views[1], "osd.0");
    assert_eq!(before["rss"]["peak"]["value"].as_u64(), Some(900u64 << 20));
    let after = daemon(&views[2], "osd.0");
    assert_eq!(
        after["rss"]["peak"]["value"].as_u64(),
        Some(50u64 << 20),
        "the old peak must not survive the restart"
    );
    assert_eq!(
        after["rss"]["peak"]["at_ms"].as_u64(),
        views[2]["wall_ms"].as_u64()
    );
    assert!(after["rss"]["rate"].is_null(), "no rate across a restart");
    assert_eq!(after["restarted_at"].as_u64(), views[2]["wall_ms"].as_u64());
    assert_eq!(
        after["rss_kernel_peak"]["value"].as_u64(),
        Some(55u64 << 20)
    );
    // the node row for the daemon is reset as well
    let row = views[2]["node"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "anon:osd.0")
        .unwrap();
    assert!(row["track"]["peak"]["value"].as_u64().unwrap() <= 800u64 << 20);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_build_without_heap_stats_still_adds_up() {
    let mut seq = load_fixture("vstart-idle");
    for s in &mut seq {
        for d in s["daemons"].as_array_mut().unwrap() {
            d["heap"] = serde_json::Value::Null;
        }
    }
    let dir = tmp("noheap");
    write_fixture(&dir, &seq);
    let v = all_samples_pass(&[
        "--replay",
        dir.to_str().unwrap(),
        "--json",
        "--tolerance",
        "100",
    ]);
    let d = daemon(&v[0], "osd.0");
    let ids: Vec<&str> = d["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&"heap:rest") && !ids.contains(&"heap:free"),
        "{:?}",
        ids
    );
    assert!(
        d["account"]["ceph"]["heap_in_use"].is_null(),
        "no heap stats: unknown, not zero"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_degraded_sample_is_shown_but_feeds_neither_peaks_nor_rates() {
    let mut seq = load_fixture("vstart-idle");
    set_osd0_proc(&mut seq[1], |p| {
        p["rss"] = serde_json::json!(900u64 << 20);
        p["rss_anon"] = serde_json::json!(800u64 << 20);
    });
    seq[1]["spread_ms"] = serde_json::json!(60_000);
    let dir = tmp("degraded");
    write_fixture(&dir, &seq);
    let out = bin()
        .args(["--replay", dir.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    let views: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let d = daemon(&views[1], "osd.0");
    assert_eq!(d["rss"]["cur"].as_u64(), Some(900u64 << 20), "shown");
    assert!(
        d["rss"]["peak"]["value"].as_u64().unwrap() < 900u64 << 20,
        "not a peak"
    );
    assert!(d["rss"]["rate"].is_null());
    assert_eq!(d["degraded"], true);
    assert_eq!(views[1]["node"]["degraded"], true);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unknown_values_are_blank_and_unreadable_daemons_are_left_out_of_the_verdict() {
    let mut seq = load_fixture("vstart-idle");
    for s in &mut seq {
        for d in s["daemons"].as_array_mut().unwrap() {
            if d["name"] == "osd.1" {
                d["mempools"] = serde_json::Value::Null;
                d["errors"] = serde_json::json!(["dump_mempools: connect: Permission denied"]);
            }
        }
    }
    let dir = tmp("unknown");
    write_fixture(&dir, &seq);
    let out = all_samples_pass(&[
        "--replay",
        dir.to_str().unwrap(),
        "--json",
        "--tolerance",
        "100",
    ]);
    let d = daemon(&out[0], "osd.1");
    let pg = d["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "nocache:pglog")
        .unwrap();
    assert!(pg["value"].is_null());
    assert!(
        pg["track"]["peak"].is_null(),
        "a peak with no samples behind it is blank"
    );
    assert_eq!(d["errors"].as_array().unwrap().len(), 1);
    assert!(
        out[0]["checks"]["unchecked"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x == "osd.1"),
        "{}",
        out[0]["checks"]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_check_is_reported_and_fails_the_run() {
    let mut seq = load_fixture("vstart-idle");
    // a meminfo without AnonPages: the node identity cannot close
    seq[0]["meminfo"]
        .as_object_mut()
        .unwrap()
        .remove("AnonPages");
    let dir = tmp("failcheck");
    write_fixture(&dir, &seq[..1]);
    let out = bin()
        .args(["--replay", dir.to_str().unwrap(), "--json", "--once"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "a failed check must fail the run");
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).lines().next().unwrap()).unwrap();
    assert_eq!(v["checks"]["ok"], false);
    assert!(
        v["checks"]["failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f.as_str().unwrap().starts_with("node:")),
        "{}",
        v["checks"]
    );
    let plain = bin()
        .args(["--replay", dir.to_str().unwrap(), "--once"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&plain.stdout).contains("checks   FAILED"),
        "{}",
        String::from_utf8_lossy(&plain.stdout)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
