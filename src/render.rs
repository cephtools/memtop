//! Tables as strings, shared by the terminal UI and the plain text output. Fixed column widths.

use crate::state::{DaemonView, TrackedRow, View};

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
    /// The accounting row ids this table row stands for: one for a leaf, every leaf beneath for an
    /// aggregate, the daemon name in the daemons table. The keys act on them.
    pub ids: Vec<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct Opts {
    pub peaks: bool,
    pub raw: bool,
    /// Rows under what they belong to (tree) instead of a flat list.
    pub tree: bool,
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
        ids: vec![r.row.id.clone()],
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
            ids: vec![a.name.clone()],
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

/// The accounting in a few aligned lines, before any detail.
pub fn node_summary(v: &View) -> Vec<String> {
    let mut out = Vec::new();
    let n = &v.node;
    let b = units::bytes;
    let get = |id: &str| {
        n.rows
            .iter()
            .find(|r| r.row.id == id)
            .and_then(|r| r.row.value)
    };
    let ceph_rows: Vec<&TrackedRow> = n
        .rows
        .iter()
        .filter(|r| {
            r.row.id.starts_with("anon:")
                && !["anon:other", "anon:noproc", "anon:shared"].contains(&r.row.id.as_str())
        })
        .collect();
    let ceph: u64 = ceph_rows.iter().filter_map(|r| r.row.value).sum();
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
        "node     MemTotal {} | residual {} ({}%) | sampling {} | daemons {} readable{}{}",
        b(n.total),
        b(get("node:other")),
        n.residual_pct
            .map(|p| p.to_string())
            .unwrap_or_else(|| "?".into()),
        v.sampling_ms
            .map(|m| format!("{:.1} s", m as f64 / 1000.0))
            .unwrap_or_else(|| "first sample".into()),
        readable,
        if unreadable.is_empty() {
            String::new()
        } else {
            format!(", not readable: {}", unreadable.join(" "))
        },
        if n.degraded { " | DEGRADED SAMPLE" } else { "" }
    ));
    let anon_all = get("AnonPages").or_else(|| {
        let s: u64 = n
            .rows
            .iter()
            .filter(|r| r.row.id.starts_with("anon:"))
            .filter_map(|r| r.row.contribution())
            .map(|x| x.max(0) as u64)
            .sum();
        Some(s)
    });
    out.push(format!(
        "memory   free {} + page cache {} + shmem {} + slab {} + kernel {} + anon {} + other {} = {}",
        b(get("node:free")),
        b(get("node:pagecache")),
        b(get("node:shmem")),
        b(get("node:slab")),
        b(get("node:kernel")),
        b(anon_all),
        b(get("node:other")),
        b(n.total)
    ));
    let none = match n.rows.iter().find(|r| r.row.id == "anon:shared") {
        Some(r) => format!(
            "shared between processes {}",
            units::rate(r.row.signed).trim_end_matches("/s")
        ),
        None => format!("no process {}", b(get("anon:noproc"))),
    };
    let nothers = n
        .rows
        .iter()
        .find(|r| r.row.id == "anon:other")
        .map(|r| r.row.note.clone())
        .unwrap_or_default();
    out.push(format!(
        "anon     ceph daemons {} ({}) + other processes {} ({}) + {}",
        b(Some(ceph)),
        ceph_rows.len(),
        b(get("anon:other")),
        nothers,
        none
    ));
    let c = &v.checks;
    if c.ok {
        out.push(format!(
            "checks   ok: node rows = MemTotal, daemon rows = rss, cross-checks hold, residuals within tolerance{}",
            if c.unchecked.is_empty() { String::new() } else { format!(" (not checked: {})", c.unchecked.join(" ")) }
        ));
    } else {
        out.push(format!("checks   FAILED: {}", c.failures.join("; ")));
    }
    out
}

pub fn daemon_summary(d: &DaemonView, sampling_ms: Option<u64>) -> Vec<String> {
    let a = &d.account;
    let k = &a.kernel;
    let c = &a.ceph;
    let b = units::bytes;
    let na = |v: Option<u64>| if v.is_some() { b(v) } else { "n/a".to_string() };
    let mut out = Vec::new();
    out.push(format!(
        "{:<8} pid {} | threads {} | version {} | socket {}",
        a.name,
        k.pid.map(|p| p.to_string()).unwrap_or_else(|| "n/a".into()),
        k.threads
            .map(|t| t.to_string())
            .unwrap_or_else(|| "n/a".into()),
        d.version.clone().unwrap_or_else(|| "n/a".into()),
        d.socket.clone().unwrap_or_else(|| "none".into())
    ));
    // 1. the kernel and the cgroup
    out.push(format!(
        "kernel   rss {} = anon {} + file {} + shmem {} | VmHWM {} ({}) | rss peak seen {}{}",
        na(k.rss),
        na(k.anon),
        na(k.file),
        na(k.shmem),
        na(k.hwm),
        d.rss_kernel_peak
            .seen_ms
            .map(|t| format!("at {}", units::clock(t)))
            .unwrap_or_else(|| "before start".into()),
        d.rss
            .peak
            .map(|p| b(Some(p.0)))
            .unwrap_or_else(|| "n/a".into()),
        d.rss
            .peak
            .map(|p| format!(" at {}", units::clock(p.1)))
            .unwrap_or_default(),
    ));
    out.push(match &k.cgroup_path {
        Some(path) => format!(
            "         cgroup {}: current {}, max {}, peak {}{}",
            path,
            na(k.cgroup_current),
            k.cgroup_max
                .map(|m| b(Some(m)))
                .unwrap_or_else(|| "unlimited".into()),
            na(k.cgroup_peak),
            k.cgroup_procs
                .filter(|n| *n > 1)
                .map(|n| format!(" (shared by {} processes)", n))
                .unwrap_or_default()
        ),
        None => "         cgroup n/a".to_string(),
    });
    // 2. the daemon
    out.push(format!(
        "ceph     {}",
        match (&c.target_key, c.target) {
            (Some(key), Some(t)) => format!(
                "{} {} (rss {}%{})",
                key,
                b(Some(t)),
                k.rss.and_then(|r| units::percent(r, t)).unwrap_or(0),
                c.tuner_target
                    .filter(|x| *x != t)
                    .map(|x| format!(", tuner uses {}", b(Some(x))))
                    .unwrap_or_default()
            ),
            _ => match c.tuner_target {
                Some(t) => format!("tuner target {}", b(Some(t))),
                None => "no memory target for this daemon type".into(),
            },
        }
    ));
    out.push(format!(
        "         mempool_total {} (caches {} + un-cached {}) | heap_in_use {} | heap_actual {} | tuner sees mapped {}, unmapped {}",
        na(c.mempool_total),
        na(c.mempool_caches),
        na(c.uncached_mempool),
        na(c.heap_in_use),
        na(c.heap_actual),
        na(c.tuner_mapped),
        na(c.tuner_unmapped),
    ));
    let neg = |v: Option<u64>, known: bool| if known { na(v) } else { "n/a".to_string() };
    out.push(format!(
        "         untracked {} = heap_in_use - mempool_total | allocator_overhead {} = heap_actual - heap_in_use | non_heap {} = rss - heap_actual",
        neg(c.untracked, c.heap_in_use.is_some() && c.mempool_total.is_some()),
        neg(c.allocator_overhead, c.heap_actual.is_some()),
        neg(c.non_heap, c.heap_actual.is_some() && k.rss.is_some()),
    ));
    // what the untracked part is made of, as the tree shows it
    if c.untracked.is_some() {
        let row = |id: &str| a.rows.iter().find(|r| r.id == id).and_then(|r| r.value);
        let residual = row("heap:unattributed")
            .or_else(|| row("heap:rest"))
            .or_else(|| row("anon:unattributed"));
        out.push(format!(
            "         untracked {} = block caches {} + memtables {} + table readers {} + residual {}",
            na(c.untracked),
            na(c.block_caches),
            na(row("nocache:memtables")),
            na(row("nocache:sst_readers")),
            na(residual),
        ));
    }
    // 3. the two must agree: one check per line
    for (i, m) in a.matches.iter().enumerate() {
        out.push(format!(
            "{:<8} {:<9} {:<66} {}",
            if i == 0 { "match" } else { "" },
            if m.ok {
                "ok"
            } else if m.hard {
                "MISMATCH"
            } else {
                "differs"
            },
            m.what,
            m.detail
        ));
    }
    out.push(format!(
        "state    sampling {} | residual {}% of rss{}{}",
        sampling_ms
            .map(|m| format!("{:.1} s", m as f64 / 1000.0))
            .unwrap_or_else(|| "first sample".into()),
        a.residual_pct
            .map(|p| p.to_string())
            .unwrap_or_else(|| "?".into()),
        if d.stale {
            " | STALE"
        } else if d.degraded {
            " | DEGRADED"
        } else {
            ""
        },
        d.restarted_at
            .map(|t| format!(" | restarted at {}, peaks and rates reset", units::clock(t)))
            .unwrap_or_default()
    ));
    for e in &d.errors {
        out.push(format!("error    could not read: {}", e));
    }
    for p in &a.problems {
        out.push(format!("problem  {}", p));
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

// ---- tree layout: the same rows, shown under what they belong to ----------------------------

/// One node of the tree: an aggregate (value computed from its children or a side figure) or a
/// leaf bound to an accounting row, which carries the rate and the peaks.
struct Node {
    label: String,
    value: Option<u64>,
    note: String,
    leaf: Option<String>,
    reference: bool,
    children: Vec<Node>,
}

impl Node {
    fn agg(label: &str, value: Option<u64>, note: &str, children: Vec<Node>) -> Node {
        Node {
            label: label.into(),
            value,
            note: note.into(),
            leaf: None,
            reference: false,
            children,
        }
    }
    fn leaf(rows: &[TrackedRow], id: &str, label: &str) -> Node {
        let r = rows.iter().find(|r| r.row.id == id);
        Node {
            label: label.into(),
            value: r.and_then(|r| r.row.value),
            note: r.map(|r| r.row.note.clone()).unwrap_or_default(),
            leaf: Some(id.into()),
            reference: r.map_or(false, |r| r.row.reference),
            children: Vec::new(),
        }
    }
    /// A leaf shown only when its row exists.
    fn leaf_if(rows: &[TrackedRow], id: &str, label: &str) -> Option<Node> {
        rows.iter()
            .any(|r| r.row.id == id)
            .then(|| Node::leaf(rows, id, label))
    }
}

fn leaf_ids(n: &Node, out: &mut Vec<String>) {
    if let Some(id) = &n.leaf {
        out.push(id.clone());
    }
    for c in &n.children {
        leaf_ids(c, out);
    }
}

fn sum_nodes(nodes: &[Node]) -> Option<u64> {
    let vals: Vec<Option<u64>> = nodes
        .iter()
        .filter(|n| !n.reference)
        .map(|n| n.value)
        .collect();
    if vals.is_empty() || vals.iter().any(|v| v.is_none()) {
        None
    } else {
        Some(vals.iter().map(|v| v.unwrap()).sum())
    }
}

/// (glyphs, node) in display order: the tree lines up to and including this node's branch.
fn flatten<'a>(nodes: &'a [Node], prefix: &str, out: &mut Vec<(String, &'a Node)>) {
    let n = nodes.len();
    for (i, node) in nodes.iter().enumerate() {
        let last = i + 1 == n;
        out.push((
            format!("{}{} ", prefix, if last { "└─" } else { "├─" }),
            node,
        ));
        let child_prefix = format!("{}{}", prefix, if last { "   " } else { "│  " });
        flatten(&node.children, &child_prefix, out);
    }
}

/// The daemon's memory as a tree under its RSS. Leaves are the accounting rows and keep their
/// rate and peaks; aggregates show the current value only, since peaks do not add up.
fn daemon_tree(d: &DaemonView) -> Vec<Node> {
    let rows = &d.rows;
    let a = &d.account;
    let k = &a.kernel;
    let c = &a.ceph;
    let b = units::bytes;
    let has = |id: &str| rows.iter().any(|r| r.row.id == id);
    let val = |id: &str| {
        rows.iter()
            .find(|r| r.row.id == id)
            .and_then(|r| r.row.value)
    };
    // mempools: the caches BlueStore counts, and everything under no cache
    let mut cache_pools = Vec::new();
    if let Some(n) = Node::leaf_if(rows, "cache:data", "data cache (BlueStore buffers)") {
        cache_pools.push(n);
    }
    if let Some(n) = Node::leaf_if(rows, "cache:meta", "meta cache (onodes, extents, blobs)") {
        cache_pools.push(n);
    }
    let mut uncached = Vec::new();
    for (id, label) in [
        ("nocache:pglog", "PG log (entries and dups)"),
        ("nocache:osdmap", "OSD maps"),
        ("nocache:alloc", "BlueStore allocator"),
        ("nocache:writes", "BlueStore in-flight writes"),
        ("nocache:bluefs", "BlueFS"),
        ("nocache:buffers", "buffers (messages, bufferlists)"),
        ("nocache:other", "other mempools"),
    ] {
        if let Some(n) = Node::leaf_if(rows, id, label) {
            uncached.push(n);
        }
    }
    let mut mempools = Vec::new();
    if !cache_pools.is_empty() {
        let v = sum_nodes(&cache_pools);
        mempools.push(Node::agg(
            "caches",
            v,
            "under the data and meta priority caches",
            cache_pools,
        ));
    }
    let v = sum_nodes(&uncached);
    mempools.push(Node::agg("un-cached", v, "under no cache", uncached));
    let mempools_node = Node::agg(
        "mempools (dump_mempools total)",
        c.mempool_total,
        "",
        mempools,
    );
    // RocksDB: its block caches are heap outside the mempools; the rest it does not report
    let mut kv = Vec::new();
    if let Some(n) = Node::leaf_if(rows, "cache:kv", "kv block cache") {
        kv.push(n);
    }
    if let Some(n) = Node::leaf_if(rows, "cache:kv_onode", "kv_onode block cache") {
        kv.push(n);
    }
    let mut untracked = Vec::new();
    if !kv.is_empty() {
        let v = sum_nodes(&kv);
        untracked.push(Node::agg(
            "RocksDB block caches",
            v,
            "under the kv priority caches, not in any mempool",
            kv,
        ));
    }
    // the memtables with the live WAL beneath them: the reference that explains their size
    if let Some(mut m) = Node::leaf_if(rows, "nocache:memtables", "RocksDB memtables") {
        if let Some(w) = Node::leaf_if(rows, "ref:wal_alive", "RocksDB WAL alive (BlueFS db.wal)") {
            m.children.push(w);
        }
        untracked.push(m);
    }
    if let Some(n) = Node::leaf_if(
        rows,
        "nocache:sst_readers",
        "RocksDB table readers outside the block cache",
    ) {
        untracked.push(n);
    }
    let residual_id = if has("heap:unattributed") {
        "heap:unattributed"
    } else if has("heap:rest") {
        "heap:rest"
    } else {
        "anon:unattributed"
    };
    // the residual with the estimate of its largest part beneath it
    let mut residual = Node::leaf(rows, residual_id, "not in any mempool (residual)");
    if let Some(p) = Node::leaf_if(
        rows,
        "ref:pglog_uncounted",
        "PG log beyond the mempool count (estimate)",
    ) {
        residual.children.push(p);
    }
    untracked.push(residual);
    let untracked_v = sum_nodes(&untracked);
    let untracked_node = Node::agg(
        "untracked (heap_in_use - mempool_total)",
        untracked_v,
        "",
        untracked,
    );
    // the heap
    let in_use_children = vec![mempools_node, untracked_node];
    let in_use_v = sum_nodes(&in_use_children);
    let mut heap_children = Vec::new();
    let heap_label;
    let heap_v;
    if has("heap:free") {
        heap_children.push(Node::agg(
            "in use by the application (heap_in_use)",
            in_use_v,
            "",
            in_use_children,
        ));
        heap_children.push(Node::leaf(
            rows,
            "heap:free",
            "allocator: free lists and metadata (allocator_overhead)",
        ));
        if let Some(n) = Node::leaf_if(rows, "ref:heap_nonresident", "held but not resident") {
            heap_children.push(n);
        }
        heap_label = "heap (tcmalloc actual, resident part)";
        heap_v = sum_nodes(&heap_children);
    } else if has("heap:rest") {
        heap_children = in_use_children;
        heap_label = "heap (tuner's mapped bytes; no heap stats)";
        heap_v = sum_nodes(&heap_children);
    } else {
        heap_children = in_use_children;
        heap_label = "heap (no heap stats, no tuner view)";
        heap_v = sum_nodes(&heap_children);
    }
    let mut anon_children = vec![Node::agg(heap_label, heap_v, "", heap_children)];
    if let Some(n) = Node::leaf_if(
        rows,
        "anon:outside_heap",
        "anon outside heap (stacks, other allocators)",
    ) {
        anon_children.push(n);
    }
    let anon_note = match (k.anon, sum_nodes(&anon_children)) {
        (Some(an), Some(s)) if an != s => format!(
            "RssAnon {}: children differ by {}",
            b(Some(an)),
            units::rate(Some(an as i64 - s as i64)).trim_end_matches("/s")
        ),
        _ => "RssAnon".into(),
    };
    let anon = Node::agg("anon", k.anon, &anon_note, anon_children);
    let mut top = vec![anon];
    if let Some(n) = Node::leaf_if(
        rows,
        "rss:file",
        "file-backed (binary, libraries, mapped files)",
    ) {
        top.push(n);
    }
    if let Some(n) = Node::leaf_if(rows, "rss:shmem", "shared memory") {
        top.push(n);
    }
    let _ = val;
    top
}

/// The node's memory as a tree under MemTotal.
fn node_tree(v: &View) -> Vec<Node> {
    let rows = &v.node.rows;
    let mut anon_children: Vec<Node> = rows
        .iter()
        .filter(|r| {
            r.row.id.starts_with("anon:")
                && !["anon:other", "anon:noproc", "anon:shared"].contains(&r.row.id.as_str())
        })
        .map(|r| Node::leaf(rows, &r.row.id, &r.row.label))
        .collect();
    let ceph_v = sum_nodes(&anon_children);
    let n_ceph = anon_children.len();
    let mut anon = vec![Node::agg(
        "ceph daemons",
        ceph_v,
        &format!("{} daemons", n_ceph),
        std::mem::take(&mut anon_children),
    )];
    for (id, label) in [
        ("anon:other", "other processes"),
        ("anon:noproc", "no process (exiting, or in no map)"),
        ("anon:shared", "shared between processes (correction)"),
    ] {
        if let Some(n) = Node::leaf_if(rows, id, label) {
            anon.push(n);
        }
    }
    let anon_v = {
        let mut t: i128 = 0;
        let mut ok = true;
        for n in &anon {
            match (n.value, n.leaf.as_deref()) {
                (Some(v), _) => t += v as i128,
                (None, Some("anon:shared")) => {
                    t += rows
                        .iter()
                        .find(|r| r.row.id == "anon:shared")
                        .and_then(|r| r.row.signed)
                        .unwrap_or(0) as i128
                }
                _ => ok = false,
            }
        }
        if ok && t >= 0 {
            Some(t as u64)
        } else {
            None
        }
    };
    let mut top = Vec::new();
    for (id, label) in [
        ("node:free", "free"),
        ("node:pagecache", "page cache (file)"),
        ("node:shmem", "shmem / tmpfs"),
        ("node:slab", "slab"),
        ("node:kernel", "kernel stacks, page tables, percpu"),
        ("node:hugepages", "huge pages (reserved)"),
    ] {
        if let Some(n) = Node::leaf_if(rows, id, label) {
            top.push(n);
        }
    }
    top.push(Node::agg("anon (AnonPages)", anon_v, "", anon));
    top.push(Node::leaf(
        rows,
        "node:other",
        "kernel, other (residual: vmalloc, drivers, DMA)",
    ));
    for (id, label) in [
        ("ref:sockets", "socket buffers"),
        ("ref:vmalloc", "vmalloc used"),
    ] {
        if let Some(n) = Node::leaf_if(rows, id, label) {
            top.push(n);
        }
    }
    top
}

fn tree_table(
    title: String,
    nodes: &[Node],
    rows: &[TrackedRow],
    whole: Option<u64>,
    share_of: &str,
    o: Opts,
) -> Table {
    let (header, mut widths) = tracked_header(o, share_of);
    widths[0] = 60; // the tree lines take room
    let mut flat = Vec::new();
    flatten(nodes, "", &mut flat);
    let mut out = Vec::new();
    for (glyphs, n) in flat {
        let leaf = n
            .leaf
            .as_deref()
            .and_then(|id| rows.iter().find(|r| r.row.id == id));
        let label = |marker: &str| format!("{}{}{}", glyphs, marker, n.label);
        let mut cells = match leaf {
            Some(r) => {
                let mut c = tracked_cells(r, whole, o).cells;
                c[0] = label(if r.row.reference {
                    "ref: "
                } else if r.row.residual {
                    "? "
                } else {
                    ""
                });
                c
            }
            None => {
                let mut c = vec![
                    label(""),
                    n.value
                        .map(|v| units::bytes(Some(v)))
                        .unwrap_or_else(|| "n/a".into()),
                    match (n.value, whole) {
                        (Some(v), Some(w)) => units::percent(v, w)
                            .map(|p| format!("{}%", p))
                            .unwrap_or_default(),
                        _ => String::new(),
                    },
                    String::new(),
                    String::new(),
                ];
                if o.peaks {
                    c.push(String::new());
                    c.push(String::new());
                }
                c.push(if o.raw { String::new() } else { n.note.clone() });
                c
            }
        };
        if cells[0].chars().count() > 60 {
            cells[0] = cells[0].chars().take(60).collect();
        }
        let mut ids = Vec::new();
        leaf_ids(n, &mut ids);
        out.push(TableRow {
            cells,
            dim: n.reference,
            warn: n.value.is_none()
                && !n.reference
                && leaf.map_or(true, |r| r.row.signed.is_none()),
            ids,
        });
    }
    Table {
        title,
        header,
        widths,
        rows: out,
    }
}

pub fn daemon_tree_table(d: &DaemonView, o: Opts) -> Table {
    let nodes = daemon_tree(d);
    tree_table(
        format!(
            "{}: rss {} = ...",
            d.account.name,
            units::bytes(d.account.rss)
        ),
        &nodes,
        &d.rows,
        d.account.rss,
        "%rss",
        o,
    )
}

pub fn node_tree_table(v: &View, o: Opts) -> Table {
    let nodes = node_tree(v);
    tree_table(
        format!("node: MemTotal {} = ...", units::bytes(v.node.total)),
        &nodes,
        &v.node.rows,
        v.node.total,
        "%mem",
        o,
    )
}
