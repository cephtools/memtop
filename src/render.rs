//! Tables as strings, shared by the terminal UI and the plain text output. Fixed column widths.

use crate::state::{DaemonView, TrackedRow, View};

trait RowId {
    fn id_is(&self, id: &str) -> bool;
}

impl RowId for TrackedRow {
    fn id_is(&self, id: &str) -> bool {
        self.row.id == id
    }
}
use crate::units;

pub struct Table {
    pub title: String,
    pub header: Vec<String>,
    pub widths: Vec<u16>,
    pub rows: Vec<TableRow>,
}

#[derive(Clone, Debug, Default)]
pub struct TableRow {
    pub cells: Vec<String>,
    pub dim: bool,
    pub warn: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Opts {
    pub peaks: bool,
    pub raw: bool,
}

fn grow_mark(t: &crate::peaks::Track) -> String {
    if t.growing >= 3 {
        format!("^{}", t.growing.min(99))
    } else {
        String::new()
    }
}

fn peak_cells(t: &crate::peaks::Track) -> Vec<String> {
    match t.peak {
        Some((v, at)) => vec![units::bytes(Some(v)), units::clock(at)],
        None => vec![String::new(), String::new()],
    }
}

fn tracked_cells(r: &TrackedRow, whole: Option<u64>, o: Opts) -> TableRow {
    let v = r.row.value;
    let cur = match (r.row.signed, v) {
        (Some(s), _) => units::rate(Some(s)).trim_end_matches("/s").to_string(),
        (None, Some(v)) => units::bytes(Some(v)),
        (None, None) => "n/a".to_string(),
    };
    let share = match (v, whole) {
        (Some(v), Some(w)) if !r.row.reference => units::percent(v, w)
            .map(|p| format!("{}%", p))
            .unwrap_or_default(),
        _ => String::new(),
    };
    let mut cells = vec![
        format!(
            "{}{}",
            if r.row.reference {
                "  ref "
            } else if r.row.residual {
                "  ? "
            } else {
                ""
            },
            r.row.label
        ),
        cur,
        share,
        units::rate(r.track.rate),
        grow_mark(&r.track),
    ];
    if o.peaks {
        cells.extend(peak_cells(&r.track));
    }
    cells.push(if o.raw {
        r.row.source.clone()
    } else {
        r.row.note.clone()
    });
    TableRow {
        cells,
        dim: r.row.reference,
        warn: v.is_none() && r.row.signed.is_none() && !r.row.reference,
    }
}

fn tracked_header(o: Opts, share_of: &str) -> (Vec<String>, Vec<u16>) {
    let mut h = vec![
        "row".to_string(),
        "current".into(),
        share_of.into(),
        "rate".into(),
        "grow".into(),
    ];
    let mut w = vec![46u16, 8, 5, 10, 4];
    if o.peaks {
        h.extend(["peak:seen".to_string(), "at".into()]);
        w.extend([9, 8]);
    }
    h.push(if o.raw {
        "source".into()
    } else {
        "note".into()
    });
    w.push(60);
    (h, w)
}

pub fn node_table(v: &View, o: Opts) -> Table {
    let (header, widths) = tracked_header(o, "%mem");
    let rows = v
        .node
        .rows
        .iter()
        .map(|r| tracked_cells(r, v.node.total, o))
        .collect();
    Table {
        title: "node memory, adds up to MemTotal".into(),
        header,
        widths,
        rows,
    }
}

pub fn daemons_table(v: &View, o: Opts) -> Table {
    let mut header = vec![
        "daemon".to_string(),
        "pid".into(),
        "anon".into(),
        "rss".into(),
        "rss rate".into(),
    ];
    let mut widths = vec![14u16, 7, 8, 8, 10];
    if o.peaks {
        header.extend([
            "rss peak:seen".to_string(),
            "at".into(),
            "rss peak:kernel".into(),
            "at".into(),
        ]);
        widths.extend([13, 8, 15, 8]);
    }
    header.extend([
        "target".to_string(),
        "%tgt".into(),
        "cgroup".into(),
        "cg max".into(),
    ]);
    widths.extend([8, 5, 8, 8]);
    if o.peaks {
        header.extend(["cg peak:kernel".to_string(), "at".into()]);
        widths.extend([14, 8]);
    }
    header.push("state".into());
    widths.push(40);
    let mut rows = Vec::new();
    for d in &v.daemons {
        let a = &d.account;
        let anon = v
            .node
            .rows
            .iter()
            .find(|r| r.row.id == format!("anon:{}", a.name))
            .and_then(|r| r.row.value);
        let mut cells = vec![
            a.name.clone(),
            a.pid.map(|p| p.to_string()).unwrap_or_default(),
            units::bytes(anon),
            units::bytes(d.rss.cur),
            units::rate(d.rss.rate),
        ];
        if o.peaks {
            cells.extend(peak_cells(&d.rss));
            cells.push(units::bytes(d.rss_kernel_peak.value));
            cells.push(
                d.rss_kernel_peak
                    .seen_ms
                    .map(units::clock)
                    .unwrap_or_else(|| {
                        if d.rss_kernel_peak.value.is_some() {
                            "<start".into()
                        } else {
                            String::new()
                        }
                    }),
            );
        }
        let tgt = a.target.as_ref().map(|(_, v)| *v);
        cells.push(units::bytes(tgt));
        cells.push(match (d.rss.cur, tgt) {
            (Some(r), Some(t)) => units::percent(r, t)
                .map(|p| format!("{}%", p))
                .unwrap_or_default(),
            _ => String::new(),
        });
        let cg = a.cgroup.as_ref();
        let shared = cg.map_or(false, |c| c.nprocs.map_or(false, |n| n > 1));
        cells.push(match cg.and_then(|c| c.current) {
            Some(c) => format!("{}{}", units::bytes(Some(c)), if shared { "*" } else { "" }),
            None => String::new(),
        });
        cells.push(
            cg.and_then(|c| c.max)
                .map(|m| units::bytes(Some(m)))
                .unwrap_or_else(|| {
                    if cg.is_some() {
                        "max".into()
                    } else {
                        String::new()
                    }
                }),
        );
        if o.peaks {
            cells.push(units::bytes(d.cgroup_kernel_peak.value));
            cells.push(
                d.cgroup_kernel_peak
                    .seen_ms
                    .map(units::clock)
                    .unwrap_or_else(|| {
                        if d.cgroup_kernel_peak.value.is_some() {
                            "<start".into()
                        } else {
                            String::new()
                        }
                    }),
            );
        }
        let mut state = Vec::new();
        if let Some(t) = d.restarted_at {
            state.push(format!("restarted {} (peaks reset)", units::clock(t)));
        }
        if d.stale {
            state.push("stale".into());
        } else if d.degraded {
            state.push(format!("degraded ({} ms)", d.spread_ms));
        }
        if a.pid.is_none() {
            state.push("no process visible".into());
        }
        if d.socket.is_none() {
            state.push("no socket".into());
        }
        if let Some(e) = d.errors.first() {
            state.push(e.clone());
        }
        if shared {
            state.push(format!(
                "cgroup shared by {} procs",
                cg.and_then(|c| c.nprocs).unwrap_or(0)
            ));
        }
        cells.push(state.join("; "));
        rows.push(TableRow {
            cells,
            dim: d.stale,
            warn: !d.errors.is_empty(),
        });
    }
    Table {
        title: "daemons (anon counts in the node rows; rss, target and cgroup are reference)"
            .into(),
        header,
        widths,
        rows,
    }
}

pub fn daemon_table(d: &DaemonView, o: Opts) -> Table {
    let (header, widths) = tracked_header(o, "%rss");
    let rows = d
        .rows
        .iter()
        .map(|r| tracked_cells(r, d.account.rss, o))
        .collect();
    Table {
        title: format!(
            "{}: adds up to rss {}",
            d.account.name,
            units::bytes(d.account.rss)
        ),
        header,
        widths,
        rows,
    }
}

/// The accounting in one line, before any detail.
pub fn node_summary(v: &View) -> Vec<String> {
    let mut out = Vec::new();
    let n = &v.node;
    let get = |id: &str| {
        n.rows
            .iter()
            .find(|r| r.row.id == id)
            .and_then(|r| r.row.value)
    };
    let ceph: u64 = n
        .rows
        .iter()
        .filter(|r| {
            r.row.id.starts_with("anon:")
                && !["anon:other", "anon:noproc", "anon:shared"].contains(&r.row.id.as_str())
        })
        .filter_map(|r| r.row.value)
        .sum();
    out.push(format!(
        "MemTotal {} = free {} + page cache {} + shmem {} + slab {} + kernel {} + anon (ceph {}, other {}, none {}) + other {}",
        units::bytes(n.total),
        units::bytes(get("node:free")),
        units::bytes(get("node:pagecache")),
        units::bytes(get("node:shmem")),
        units::bytes(get("node:slab")),
        units::bytes(get("node:kernel")),
        units::bytes(Some(ceph)),
        units::bytes(get("anon:other")),
        match n.rows.iter().find(|r| r.id_is("anon:shared")) {
            Some(r) => format!("shared {}", units::rate(r.row.signed).trim_end_matches("/s")),
            None => units::bytes(get("anon:noproc")),
        },
        units::bytes(get("node:other")),
    ));
    let readable = v
        .daemons
        .iter()
        .filter(|d| d.socket.is_some() && d.errors.is_empty())
        .count();
    let unreadable: Vec<String> = v
        .daemons
        .iter()
        .filter(|d| d.socket.is_none() || !d.errors.is_empty())
        .map(|d| d.account.name.clone())
        .collect();
    out.push(format!(
        "residual {}% of MemTotal | sampling {} (observed peaks resolve to this) | daemons readable {}{}{}",
        n.residual_pct.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
        v.sampling_ms.map(|m| format!("{:.1} s", m as f64 / 1000.0)).unwrap_or_else(|| "first sample".into()),
        readable,
        if unreadable.is_empty() { String::new() } else { format!(", not readable: {}", unreadable.join(" ")) },
        if n.degraded { " | DEGRADED SAMPLE" } else { "" }
    ));
    for p in &n.problems {
        out.push(format!("problem: {}", p));
    }
    out
}

pub fn daemon_summary(d: &DaemonView, sampling_ms: Option<u64>) -> Vec<String> {
    let a = &d.account;
    let mut out = Vec::new();
    out.push(format!(
        "{} pid {} version {} socket {}",
        a.name,
        a.pid.map(|p| p.to_string()).unwrap_or_else(|| "n/a".into()),
        d.version.clone().unwrap_or_else(|| "n/a".into()),
        d.socket.clone().unwrap_or_else(|| "none".into())
    ));
    let tgt = a
        .target
        .as_ref()
        .map(|(k, v)| {
            format!(
                "{} {} ({}%)",
                k,
                units::bytes(Some(*v)),
                d.rss.cur.and_then(|r| units::percent(r, *v)).unwrap_or(0)
            )
        })
        .unwrap_or_else(|| "no memory target for this daemon type".into());
    out.push(format!(
        "rss {} (peak:seen {} at {}, peak:kernel {} {}) | {} | threads {}",
        units::bytes(d.rss.cur),
        d.rss
            .peak
            .map(|p| units::bytes(Some(p.0)))
            .unwrap_or_default(),
        d.rss.peak.map(|p| units::clock(p.1)).unwrap_or_default(),
        units::bytes(d.rss_kernel_peak.value),
        d.rss_kernel_peak
            .seen_ms
            .map(|t| format!("at {}", units::clock(t)))
            .unwrap_or_else(|| "before start".into()),
        tgt,
        a.threads.map(|t| t.to_string()).unwrap_or_default()
    ));
    if let Some(cg) = &a.cgroup {
        out.push(format!(
            "cgroup {} current {} max {} peak:kernel {}{}",
            cg.path,
            units::bytes(cg.current),
            cg.max
                .map(|m| units::bytes(Some(m)))
                .unwrap_or_else(|| "unlimited".into()),
            units::bytes(cg.peak),
            cg.nprocs
                .filter(|n| *n > 1)
                .map(|n| format!(" (shared by {} processes)", n))
                .unwrap_or_default()
        ));
    }
    out.push(format!(
        "residual {}% of rss | sampling {} | {}{}",
        a.residual_pct
            .map(|p| p.to_string())
            .unwrap_or_else(|| "?".into()),
        sampling_ms
            .map(|m| format!("{:.1} s", m as f64 / 1000.0))
            .unwrap_or_else(|| "first sample".into()),
        if d.stale {
            "STALE "
        } else if d.degraded {
            "DEGRADED "
        } else {
            ""
        },
        d.restarted_at
            .map(|t| format!("restarted at {}, peaks and rates reset", units::clock(t)))
            .unwrap_or_default()
    ));
    for e in &d.errors {
        out.push(format!("could not read: {}", e));
    }
    for p in &a.problems {
        out.push(format!("problem: {}", p));
    }
    out
}

/// Plain text rendering of a table.
pub fn plain(t: &Table) -> String {
    let mut s = String::new();
    s.push_str(&format!("== {}\n", t.title));
    let line = |cells: &[String]| -> String {
        let mut l = String::new();
        for (i, c) in cells.iter().enumerate() {
            let w = t.widths.get(i).copied().unwrap_or(10) as usize;
            let c: String = c.chars().take(w).collect();
            if i == 0 || i == cells.len() - 1 {
                l.push_str(&format!("{:<w$} ", c, w = w));
            } else {
                l.push_str(&format!("{:>w$} ", c, w = w));
            }
        }
        l.trim_end().to_string()
    };
    s.push_str(&line(&t.header));
    s.push('\n');
    for r in &t.rows {
        s.push_str(&line(&r.cells));
        s.push('\n');
    }
    s
}
