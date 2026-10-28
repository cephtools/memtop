//! On request only: per-PG log sizes for one OSD, from `ceph pg dump` (the cluster, not the
//! socket). Degrades to "unavailable" without a usable `ceph` binary, conf or keyring.

use std::process::Command;

#[derive(Clone, Debug, Default)]
pub struct PgLog {
    pub pgid: String,
    pub log_size: u64,
    pub ondisk_log_size: u64,
    pub dups: u64,
    pub primary: bool,
}

pub struct PgQuery {
    pub conf: Option<String>,
    pub keyring: Option<String>,
    pub bindir: Option<String>,
}

/// PGs whose acting set holds `osd_id`, largest log first.
pub fn fetch(q: &PgQuery, osd_id: u32) -> Result<Vec<PgLog>, String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(b) = &q.bindir {
        candidates.push(format!("{}/ceph", b));
    }
    candidates.push("ceph".into());
    let mut last_err = String::from("no ceph binary");
    for bin in candidates {
        let mut cmd = Command::new(&bin);
        if let Some(c) = &q.conf {
            cmd.arg("-c").arg(c);
        }
        if let Some(k) = &q.keyring {
            cmd.arg("--keyring").arg(k);
        }
        cmd.args(["pg", "dump", "pgs", "-f", "json", "--connect-timeout", "5"]);
        match cmd.output() {
            Ok(o) if o.status.success() => {
                let v: serde_json::Value =
                    serde_json::from_slice(&o.stdout).map_err(|e| format!("pg dump: {}", e))?;
                let stats = v
                    .get("pg_stats")
                    .and_then(|x| x.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut out = Vec::new();
                for s in stats {
                    let acting: Vec<u64> = s
                        .get("acting")
                        .and_then(|a| a.as_array())
                        .map(|a| a.iter().filter_map(|x| x.as_u64()).collect())
                        .unwrap_or_default();
                    if !acting.contains(&(osd_id as u64)) {
                        continue;
                    }
                    out.push(PgLog {
                        pgid: s
                            .get("pgid")
                            .and_then(|x| x.as_str())
                            .unwrap_or("?")
                            .to_string(),
                        log_size: s.get("log_size").and_then(|x| x.as_u64()).unwrap_or(0),
                        ondisk_log_size: s
                            .get("ondisk_log_size")
                            .and_then(|x| x.as_u64())
                            .unwrap_or(0),
                        dups: s.get("log_dups_size").and_then(|x| x.as_u64()).unwrap_or(0),
                        primary: s.get("acting_primary").and_then(|x| x.as_u64())
                            == Some(osd_id as u64),
                    });
                }
                out.sort_by(|a, b| (b.log_size + b.dups).cmp(&(a.log_size + a.dups)));
                return Ok(out);
            }
            Ok(o) => {
                last_err = format!(
                    "{}: {}",
                    bin,
                    String::from_utf8_lossy(&o.stderr)
                        .lines()
                        .last()
                        .unwrap_or("failed")
                )
            }
            Err(e) => last_err = format!("{}: {}", bin, e),
        }
    }
    Err(last_err)
}
