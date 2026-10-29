//! The accounting: from a raw sample to rows that add up. Node rows sum to MemTotal; a daemon's
//! rows sum to its resident size. Whatever cannot be attributed is a named row, never dropped.

use crate::proc::ProcInfo;
use crate::raw::{RawDaemon, RawSample};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Mempools that BlueStore's meta priority cache counts as its own (BlueStore.h, MetaCache::_get_used_bytes).
const META_CACHE_POOLS: &[&str] = &[
    "bluestore_blob",
    "bluestore_extent",
    "bluestore_cache_buffer",
    "bluestore_cache_meta",
    "bluestore_cache_other",
    "bluestore_cache_onode",
    "bluestore_shared_blob",
    "bluestore_inline_bl",
];
const DATA_CACHE_POOLS: &[&str] = &["bluestore_cache_data"];
const OSDMAP_POOLS: &[&str] = &["osdmap", "osdmap_mapping", "osd_mapbl", "pgmap"];
const ALLOC_POOLS: &[&str] = &["bluestore_alloc"];
const BUFFER_POOLS: &[&str] = &["buffer_anon", "buffer_meta"];
const WRITE_POOLS: &[&str] = &[
    "bluestore_txc",
    "bluestore_writing",
    "bluestore_writing_deferred",
    "bluestore_fsck",
];
const BLUEFS_POOLS: &[&str] = &["bluefs", "bluefs_file_reader", "bluefs_file_writer"];
const PGLOG_POOLS: &[&str] = &["osd_pglog"];
/// The rows built from mempool groups; they must add up to dump_mempools' total.
const MEMPOOL_ROWS: &[&str] = &[
    "cache:data",
    "cache:meta",
    "nocache:pglog",
    "nocache:osdmap",
    "nocache:alloc",
    "nocache:writes",
    "nocache:bluefs",
    "nocache:buffers",
    "nocache:other",
];

/// One accounting row before tracking.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Row {
    /// Stable key across samples, e.g. "node:free", "anon:osd.0", "cache:data".
    pub id: String,
    pub label: String,
    /// Current value; None when the source is missing or the inputs disagree.
    pub value: Option<u64>,
    /// A signed correction that keeps the level adding up (shared pages counted in several processes).
    pub signed: Option<i64>,
    /// Shown for reference, never summed: it overlaps other rows or belongs to another level.
    pub reference: bool,
    /// Counts toward the level's residual (unattributed memory).
    pub residual: bool,
    /// When unknown, the bytes sit inside the residual row, so the identity still closes.
    pub optional: bool,
    /// Raw keys behind the value, for checking against dump_mempools / meminfo.
    pub source: String,
    pub note: String,
}

impl Row {
    fn new(id: &str, label: &str, value: Option<u64>, source: &str) -> Row {
        Row {
            id: id.into(),
            label: label.into(),
            value,
            source: source.into(),
            ..Default::default()
        }
    }
    fn reference(mut self) -> Row {
        self.reference = true;
        self
    }
    fn residual(mut self) -> Row {
        self.residual = true;
        self
    }
    fn optional(mut self) -> Row {
        self.optional = true;
        self
    }
    fn note(mut self, n: impl Into<String>) -> Row {
        self.note = n.into();
        self
    }
    /// What this row contributes to its level's sum.
    pub fn contribution(&self) -> Option<i128> {
        if self.reference {
            return None;
        }
        if let Some(s) = self.signed {
            return Some(s as i128);
        }
        self.value.map(|v| v as i128)
    }
}

/// The sum of every known contribution: what the level's rows add up to so far.
fn known_sum(rows: &[Row]) -> i128 {
    rows.iter().filter_map(Row::contribution).sum()
}

/// The node level: rows that sum to MemTotal.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NodeAccount {
    pub total: Option<u64>,
    pub rows: Vec<Row>,
    /// Sum of the summed rows, for the check: equals `total` by construction when it could be built.
    pub sum: Option<i128>,
    /// What the identity could not place, as a share of MemTotal in percent (None when unknown).
    pub residual_pct: Option<u64>,
    pub problems: Vec<String>,
}

fn is_ceph_daemon(p: &ProcInfo) -> bool {
    daemon_kind(&p.comm).is_some()
}

/// Daemon type from the executable name.
pub fn daemon_kind(comm: &str) -> Option<&'static str> {
    Some(match comm {
        "ceph-osd" => "osd",
        "ceph-mon" => "mon",
        "ceph-mgr" => "mgr",
        "ceph-mds" => "mds",
        "radosgw" => "rgw",
        "ceph-fuse" => "fuse",
        "rbd-mirror" => "rbd-mirror",
        "cephfs-mirror" => "cephfs-mirror",
        "ceph-nvmeof" => "nvmeof",
        "ceph-exporter" => "exporter",
        _ => return None,
    })
}

/// "osd.0" from a daemon's command line (-i 0 / --id 0 / -n osd.0 / --name osd.0), else None.
pub fn daemon_name_from_cmdline(kind: &str, cmdline: &[String]) -> Option<String> {
    let mut it = cmdline.iter().skip(1);
    while let Some(a) = it.next() {
        let (k, v) = match a.split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (a.as_str(), None),
        };
        match k {
            "-i" | "--id" => {
                let v = v.or_else(|| it.next().cloned())?;
                return Some(format!("{}.{}", kind, v));
            }
            "-n" | "--name" => {
                let v = v.or_else(|| it.next().cloned())?;
                return Some(v);
            }
            _ => {}
        }
    }
    None
}

pub fn node_account(s: &RawSample) -> NodeAccount {
    let m = &s.meminfo;
    let mut a = NodeAccount {
        total: m.get("MemTotal"),
        ..Default::default()
    };
    let Some(total) = a.total else {
        a.problems.push("MemTotal missing from meminfo".into());
        return a;
    };
    let g = |k: &str| m.get(k);
    let mut rows = Vec::new();
    rows.push(Row::new("node:free", "free", g("MemFree"), "MemFree"));
    let shmem = g("Shmem");
    let pagecache = match (g("Cached"), g("Buffers"), shmem) {
        (Some(c), Some(b), Some(sh)) => Some((c + b).saturating_sub(sh)),
        (Some(c), None, Some(sh)) => Some(c.saturating_sub(sh)),
        _ => None,
    };
    rows.push(
        Row::new(
            "node:pagecache",
            "page cache (file)",
            pagecache,
            "Cached + Buffers - Shmem",
        )
        .note("holds the daemons' file-backed RSS too"),
    );
    rows.push(Row::new("node:shmem", "shmem / tmpfs", shmem, "Shmem"));
    rows.push(
        Row::new("node:slab", "slab", g("Slab"), "Slab").note(format!(
            "reclaimable {} / unreclaimable {}",
            crate::units::bytes(g("SReclaimable")),
            crate::units::bytes(g("SUnreclaim"))
        )),
    );
    let kernel = [g("KernelStack"), g("PageTables"), g("Percpu")];
    let kernel_sum = if kernel.iter().any(|v| v.is_none()) {
        None
    } else {
        Some(kernel.iter().map(|v| v.unwrap()).sum::<u64>())
    };
    rows.push(Row::new(
        "node:kernel",
        "kernel stacks, page tables, percpu",
        kernel_sum,
        "KernelStack + PageTables + Percpu",
    ));
    let huge = match (g("HugePages_Total"), g("Hugepagesize")) {
        (Some(n), Some(sz)) if n > 0 => Some(n * sz),
        (Some(_), Some(_)) => Some(0),
        _ => None,
    };
    if huge.unwrap_or(0) > 0 {
        rows.push(Row::new(
            "node:hugepages",
            "huge pages (reserved)",
            huge,
            "HugePages_Total * Hugepagesize",
        ));
    }
    // anonymous memory: per Ceph daemon, other processes, and what no process maps
    let anon_total = g("AnonPages");
    let mut procs_anon: u64 = 0;
    let mut anon_known = true;
    let mut others = 0u64;
    let mut nothers = 0usize;
    let mut daemon_rows = Vec::new();
    for p in &s.procs {
        match p.rss_anon {
            Some(v) => procs_anon += v,
            None => anon_known = false,
        }
        if is_ceph_daemon(p) {
            let name = s
                .daemons
                .iter()
                .find(|d| d.pid == Some(p.pid))
                .map(|d| d.name.clone())
                .or_else(|| {
                    daemon_kind(&p.comm).and_then(|k| daemon_name_from_cmdline(k, &p.cmdline))
                })
                .unwrap_or_else(|| format!("{}[{}]", p.comm, p.pid));
            daemon_rows.push(
                Row::new(&format!("anon:{}", name), &name, p.rss_anon, "RssAnon")
                    .note(format!("rss {}", crate::units::bytes(p.rss))),
            );
        } else {
            others += p.rss_anon.unwrap_or(0);
            nothers += 1;
        }
    }
    daemon_rows.sort_by(|x, y| x.label.cmp(&y.label));
    rows.extend(daemon_rows);
    rows.push(
        Row::new(
            "anon:other",
            "other processes (anon)",
            Some(others),
            "RssAnon of non-Ceph processes",
        )
        .note(format!("{} processes", nothers)),
    );
    match anon_total {
        Some(t) if anon_known => {
            let d = t as i128 - procs_anon as i128;
            if d >= 0 {
                rows.push(
                    Row::new(
                        "anon:noproc",
                        "anon, no process",
                        Some(d as u64),
                        "AnonPages - sum(RssAnon)",
                    )
                    .note("exiting processes, or pages in no map")
                    .residual(),
                );
            } else {
                let mut r = Row::new(
                    "anon:shared",
                    "anon shared by processes (correction)",
                    None,
                    "AnonPages - sum(RssAnon)",
                )
                .note("pages mapped by several processes count once in AnonPages");
                r.signed = Some(d as i64);
                rows.push(r);
            }
        }
        Some(_) => {
            rows.push(
                Row::new(
                    "anon:noproc",
                    "anon, no process",
                    None,
                    "AnonPages - sum(RssAnon)",
                )
                .note("some processes unreadable")
                .residual(),
            );
            a.problems.push("RssAnon missing for some processes".into());
        }
        None => a.problems.push("AnonPages missing from meminfo".into()),
    }
    // the remainder: everything meminfo does not name
    let named: Option<i128> = {
        let mut sum: i128 = 0;
        let mut ok = true;
        for k in [
            g("MemFree"),
            pagecache,
            shmem,
            g("Slab"),
            kernel_sum,
            huge.or(Some(0)),
            anon_total,
        ] {
            match k {
                Some(v) => sum += v as i128,
                None => ok = false,
            }
        }
        if ok {
            Some(sum)
        } else {
            None
        }
    };
    match named {
        Some(n) => {
            let r = total as i128 - n;
            if r >= 0 {
                rows.push(
                    Row::new(
                        "node:other",
                        "kernel, other (vmalloc, drivers, DMA)",
                        Some(r as u64),
                        "MemTotal - named rows",
                    )
                    .residual(),
                );
                a.residual_pct = crate::units::percent(r as u64, total);
            } else {
                rows.push(
                    Row::new("node:other", "kernel, other", None, "MemTotal - named rows")
                        .note("negative: meminfo rows overlap")
                        .residual(),
                );
                a.problems.push(format!(
                    "named meminfo rows exceed MemTotal by {}",
                    crate::units::bytes(Some((-r) as u64))
                ));
            }
        }
        None => {
            rows.push(
                Row::new("node:other", "kernel, other", None, "MemTotal - named rows")
                    .note("a meminfo field is missing")
                    .residual(),
            );
            a.problems
                .push("cannot close the node identity: meminfo field missing".into());
        }
    }
    // reference rows: real, but overlapping the rows above
    let sock = match (s.sockstat.tcp_bytes, s.sockstat.udp_bytes) {
        (None, None) => None,
        (t, u) => Some(t.unwrap_or(0) + u.unwrap_or(0)),
    };
    rows.push(
        Row::new(
            "ref:sockets",
            "socket buffers",
            sock,
            "/proc/net/sockstat TCP+UDP mem",
        )
        .note("inside slab and kernel rows")
        .reference(),
    );
    rows.push(
        Row::new(
            "ref:vmalloc",
            "vmalloc used",
            g("VmallocUsed"),
            "VmallocUsed",
        )
        .note("inside kernel, other")
        .reference(),
    );
    let sum = known_sum(&rows);
    a.sum = Some(sum);
    if named.is_some() && sum != total as i128 {
        a.problems
            .push(format!("node rows sum to {} not MemTotal {}", sum, total));
    }
    a.rows = rows;
    a
}

/// tcmalloc `heap stats` figures, bytes.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HeapStats {
    pub in_use: u64,
    pub pageheap_free: u64,
    pub central_free: u64,
    pub transfer_free: u64,
    pub thread_free: u64,
    pub metadata: u64,
    /// "Actual memory used (physical + swap)": in use + free lists + metadata.
    pub actual: u64,
    pub unmapped: u64,
}

pub fn parse_heap_stats(text: &str) -> Option<HeapStats> {
    let grab = |what: &str| -> Option<u64> {
        text.lines().find(|l| l.contains(what)).and_then(|l| {
            l.trim_start_matches("MALLOC:")
                .trim_start_matches(['+', '=', ' '])
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
    };
    Some(HeapStats {
        in_use: grab("Bytes in use by application")?,
        pageheap_free: grab("Bytes in page heap freelist")?,
        central_free: grab("Bytes in central cache freelist")?,
        transfer_free: grab("Bytes in transfer cache freelist")?,
        thread_free: grab("Bytes in thread cache freelists")?,
        metadata: grab("Bytes in malloc metadata")?,
        actual: grab("Actual memory used")?,
        unmapped: grab("Bytes released to OS").unwrap_or(0),
    })
}

/// What the kernel and the cgroup say about the daemon.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct KernelSide {
    pub pid: Option<u32>,
    pub threads: Option<u64>,
    /// /proc/PID/status VmRSS, RssAnon, RssFile, RssShmem, VmHWM
    pub rss: Option<u64>,
    pub anon: Option<u64>,
    pub file: Option<u64>,
    pub shmem: Option<u64>,
    pub hwm: Option<u64>,
    pub cgroup_path: Option<String>,
    pub cgroup_current: Option<u64>,
    pub cgroup_max: Option<u64>,
    pub cgroup_peak: Option<u64>,
    /// Processes in the cgroup: more than one means the figures are not the daemon's alone.
    pub cgroup_procs: Option<usize>,
}

/// What the daemon says about itself, and the three derived figures:
///   untracked          = heap_in_use - mempool_total
///   allocator_overhead = heap_actual - heap_in_use
///   non_heap           = rss - heap_actual
/// so that rss = mempool_total + untracked + allocator_overhead + non_heap.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CephSide {
    pub target_key: Option<String>,
    /// The configured target (osd_memory_target, mon_memory_target, mds_cache_memory_limit).
    pub target: Option<u64>,
    /// The target the priority-cache tuner is using (perf dump target_bytes).
    pub tuner_target: Option<u64>,
    /// dump_mempools .mempool.total.bytes
    pub mempool_total: Option<u64>,
    /// heap stats "Bytes in use by application"
    pub heap_in_use: Option<u64>,
    /// heap stats "Actual memory used"
    pub heap_actual: Option<u64>,
    /// heap_in_use - mempool_total; None when negative (the inputs disagree).
    pub untracked: Option<u64>,
    /// heap_actual - heap_in_use
    pub allocator_overhead: Option<u64>,
    /// rss - heap_actual; None when negative (heap pages held but not resident).
    pub non_heap: Option<u64>,
    /// Usage under the priority caches: data and meta mempools plus the RocksDB block caches.
    pub caches_used: Option<u64>,
    /// The part of that inside the mempools (data and meta caches).
    pub mempool_caches: Option<u64>,
    /// The part outside them (the kv and kv_onode block caches): in heap_in_use, not in mempool_total.
    pub block_caches: Option<u64>,
    /// mempool_total minus the data and meta cache mempools.
    pub uncached_mempool: Option<u64>,
    /// The tuner's own view of the heap (perf dump mapped_bytes, unmapped_bytes).
    pub tuner_mapped: Option<u64>,
    pub tuner_unmapped: Option<u64>,
    /// What the mempool rows add up to, to check against mempool_total.
    pub mempool_rows: Option<u64>,
}

/// One cross-check between two sources. `hard` ones must hold; a failure is a problem and fails
/// --check. Soft ones explain a difference.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Match {
    pub what: String,
    pub ok: bool,
    pub hard: bool,
    pub detail: String,
}

/// A figure derived by subtraction: unknown when an input is missing, negative when the two
/// inputs disagree (then the identity cannot hold and the match says which input is off).
#[derive(Clone, Copy, Debug)]
enum Derived {
    Missing,
    Negative,
    Known(u64),
}

impl Derived {
    fn sub(x: Option<u64>, y: Option<u64>) -> Derived {
        match (x, y) {
            (Some(x), Some(y)) if x >= y => Derived::Known(x - y),
            (Some(_), Some(_)) => Derived::Negative,
            _ => Derived::Missing,
        }
    }
    fn known(self) -> Option<u64> {
        match self {
            Derived::Known(v) => Some(v),
            _ => None,
        }
    }
}

fn close(a: u64, b: u64, pct: u64, abs: u64) -> bool {
    let d = a.abs_diff(b);
    d <= abs || d as u128 * 100 <= (a.max(b) as u128) * pct as u128
}

/// One daemon's accounting: rows that sum to its RSS, plus the reference figures.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DaemonAccount {
    pub kernel: KernelSide,
    pub ceph: CephSide,
    pub matches: Vec<Match>,
    pub name: String,
    pub kind: String,
    pub pid: Option<u32>,
    pub rss: Option<u64>,
    pub vm_hwm: Option<u64>,
    pub threads: Option<u64>,
    /// (config key, bytes)
    pub target: Option<(String, u64)>,
    pub cgroup: Option<crate::proc::CgroupMem>,
    pub rows: Vec<Row>,
    pub sum: Option<i128>,
    pub residual_pct: Option<u64>,
    pub heap: Option<HeapStats>,
    pub problems: Vec<String>,
}

/// The PG log's cost beyond its mempool count, from the pool's items and bytes. The mempool
/// counts 552 B per entry and 104 B per dup; the heap holds about 785 and 178 (the index maps,
/// list nodes and allocator size classes), measured with a heap profile on main with tcmalloc.
fn pglog_uncounted(d: &RawDaemon) -> Option<u64> {
    let pool = d.mempools.as_ref()?.pointer("/mempool/by_pool/osd_pglog")?;
    let items = pool.get("items")?.as_f64()?;
    let bytes = pool.get("bytes")?.as_f64()?;
    if items <= 0.0 {
        return Some(0);
    }
    // bytes = 552 E + 104 D and items = E + D
    let dups = ((552.0 * items - bytes) / (552.0 - 104.0)).clamp(0.0, items);
    let entries = items - dups;
    Some((233.0 * entries + 74.0 * dups) as u64)
}

/// The live RocksDB WAL bytes: the sizes of the BlueFS files under db.wal/.
fn wal_alive(d: &RawDaemon) -> Option<u64> {
    let files = d.bluefs_files.as_ref()?.as_array()?;
    Some(
        files
            .iter()
            .filter(|f| {
                f.get("name")
                    .and_then(|n| n.as_str())
                    .map(|n| n.starts_with("db.wal/"))
                    .unwrap_or(false)
            })
            .filter_map(|f| f.get("size").and_then(|s| s.as_u64()))
            .sum(),
    )
}

/// Why the kv-stats figures are unknown, from the same sample: the two switches are read every
/// tick, and a failed read keeps its error.
fn kv_why(d: &RawDaemon) -> String {
    let off: Vec<&str> = ["rocksdb_perf", "rocksdb_collect_memory_stats"]
        .into_iter()
        .filter(|k| d.config.get(*k).map(|v| v != "true").unwrap_or(false))
        .collect();
    let err = d
        .errors
        .iter()
        .find_map(|e| e.strip_prefix("kv stats:"))
        .map(|e| {
            e.trim()
                .trim_start_matches("ERROR:")
                .trim_start_matches("error:")
                .trim()
        });
    let sits = "bytes sit in 'heap, not in any mempool'";
    match (off.is_empty(), err) {
        (false, Some(_)) => format!(
            "n/a: {} off on this OSD (enable declined, or not yet re-set after a restart); {}",
            off.join(" and "),
            sits
        ),
        (false, None) => format!(
            "n/a: {} off on this OSD, --enable-kv-stats sets them; {}",
            off.join(" and "),
            sits
        ),
        (true, Some(e)) => format!(
            "n/a: dump_objectstore_kv_stats: {}; {}",
            e.chars().take(70).collect::<String>(),
            sits
        ),
        (true, None) if d.config.contains_key("rocksdb_perf") => {
            format!("n/a: not read, --enable-kv-stats reads it; {}", sits)
        }
        (true, None) => format!("n/a: --enable-kv-stats reads it; {}", sits),
    }
}

fn mempool_bytes(mp: &serde_json::Value) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    if let Some(by) = mp.pointer("/mempool/by_pool").and_then(|v| v.as_object()) {
        for (k, v) in by {
            if let Some(b) = v.get("bytes").and_then(|b| b.as_u64()) {
                out.insert(k.clone(), b);
            }
        }
    }
    out
}

fn perf_u64(perf: &serde_json::Value, section: &str, key: &str) -> Option<u64> {
    perf.get(section)?.get(key)?.as_u64()
}

/// Config key that bounds a daemon of this kind, and what to call it.
pub fn target_key(kind: &str) -> Option<(&'static str, &'static str)> {
    match kind {
        "osd" => Some(("osd_memory_target", "target")),
        "mon" => Some(("mon_memory_target", "target")),
        "mds" => Some(("mds_cache_memory_limit", "cache limit")),
        _ => None,
    }
}

pub fn daemon_account(d: &RawDaemon, p: Option<&ProcInfo>, s: &RawSample) -> DaemonAccount {
    let mut a = DaemonAccount {
        name: d.name.clone(),
        kind: d.kind.clone(),
        pid: d.pid,
        ..Default::default()
    };
    if let Some(p) = p {
        a.rss = p.rss;
        a.vm_hwm = p.vm_hwm;
        a.threads = p.threads;
        a.cgroup = p.cgroup.as_ref().and_then(|c| s.cgroups.get(c).cloned());
    }
    if let Some((key, _)) = target_key(&d.kind) {
        if let Some(v) = d.config.get(key).and_then(|v| v.parse::<u64>().ok()) {
            a.target = Some((key.to_string(), v));
        }
    }
    let mut rows = Vec::new();
    let pools = d.mempools.as_ref().map(mempool_bytes).unwrap_or_default();
    let have_pools = d.mempools.is_some();
    let group = |id: &str, label: &str, keys: &[&str]| -> Row {
        let mut sum = 0u64;
        let mut src = Vec::new();
        for k in keys {
            if let Some(v) = pools.get(*k) {
                sum += v;
                src.push(format!("{}={}", k, v));
            }
        }
        Row::new(
            id,
            label,
            if have_pools { Some(sum) } else { None },
            &src.join(" "),
        )
    };
    let grouped: Vec<&str> = [
        DATA_CACHE_POOLS,
        META_CACHE_POOLS,
        PGLOG_POOLS,
        OSDMAP_POOLS,
        ALLOC_POOLS,
        WRITE_POOLS,
        BLUEFS_POOLS,
        BUFFER_POOLS,
    ]
    .concat();
    let is_bluestore = d
        .perf
        .as_ref()
        .is_some_and(|p| p.get("bluestore-pricache").is_some());
    let pricache = if is_bluestore {
        "bluestore-pricache"
    } else {
        "prioritycache"
    };
    let perf = d.perf.as_ref();
    // priority caches: usage from the source each cache counts, budget from the cache's committed bytes
    let budget = |cache: &str| {
        perf.and_then(|p| perf_u64(p, &format!("{}:{}", pricache, cache), "committed_bytes"))
    };
    let with_budget = |r: Row, cache: &str| -> Row {
        match budget(cache) {
            Some(b) => r.note(format!("budget {}", crate::units::bytes(Some(b)))),
            None => r,
        }
    };
    if is_bluestore {
        let r = group(
            "cache:data",
            "cache: data (BlueStore buffers)",
            DATA_CACHE_POOLS,
        );
        rows.push(with_budget(r, "data"));
        let r = group(
            "cache:meta",
            "cache: meta (onodes, extents, blobs)",
            META_CACHE_POOLS,
        );
        rows.push(with_budget(r, "meta"));
    }
    let kv = perf.and_then(|p| perf_u64(p, "rocksdb-cache-default", "usage"));
    if perf.is_some_and(|p| p.get("rocksdb-cache-default").is_some()) {
        rows.push(with_budget(
            Row::new(
                "cache:kv",
                "cache: kv (RocksDB block cache)",
                kv,
                "rocksdb-cache-default.usage",
            ),
            "kv",
        ));
    }
    let kvo = perf.and_then(|p| perf_u64(p, "rocksdb-cache-O", "usage"));
    if perf.is_some_and(|p| p.get("rocksdb-cache-O").is_some()) {
        rows.push(with_budget(
            Row::new(
                "cache:kv_onode",
                "cache: kv_onode (RocksDB onode block cache)",
                kvo,
                "rocksdb-cache-O.usage",
            ),
            "kv_onode",
        ));
    }
    if let Some(p) = perf {
        for (sec, v) in p.as_object().into_iter().flatten() {
            if let Some(c) = sec.strip_prefix(&format!("{}:", pricache)) {
                if !["data", "meta", "kv", "kv_onode"].contains(&c) {
                    // a cache the daemon does not export a usage counter for (the mon's full/inc
                    // osdmap caches): it requests its used bytes at its own priority
                    // (OSDMonitor.cc OSDMemCache::_request_cache_bytes), so the bins below the
                    // last are its usage; the bytes themselves sit in the mempools below
                    let used: u64 = (0..11)
                        .filter_map(|i| v.get(format!("pri{}_bytes", i)).and_then(|x| x.as_u64()))
                        .sum();
                    rows.push(
                        with_budget(
                            Row::new(
                                &format!("cache:{}", c),
                                &format!("cache: {}", c),
                                Some(used),
                                &format!("{}.pri0..10_bytes", sec),
                            ),
                            c,
                        )
                        .note("reference: lives in the mempool rows below")
                        .reference(),
                    );
                }
            }
        }
    }
    // not under any cache
    // dump_objectstore_kv_stats: the daemon flattens the "rocksdb_memtable_statistics" section in
    // practice; accept both shapes. Values are strings.
    let kv_num = |key: &str| -> Option<u64> {
        let k = d.kv_stats.as_ref()?;
        let v = k
            .pointer(&format!("/rocksdb_memtable_statistics/{}", key))
            .or_else(|| k.get(key))?;
        v.as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| v.as_u64())
    };
    let memtable = kv_num("rocksdb_memtable_usage");
    let sst_readers = kv_num("rocksdb_index_filter_blocks_usage");
    let wal_alive = wal_alive(d);
    if d.kind == "osd" {
        // only the OSD exports these (dump_objectstore_kv_stats), and only for RocksDB's default
        // column family: RocksDBStore::get_statistics calls DB::GetProperty without a handle, so
        // on a sharded OSD (O, L, P, M... families) the figure misses nearly everything. Shown as
        // what it is; the rest sits in the heap residual.
        let r = Row::new(
            "nocache:memtables",
            "RocksDB memtables",
            memtable,
            "dump_objectstore_kv_stats rocksdb_memtable_usage",
        )
        .optional();
        // a daemon without the all-families fix answers for the default family: a few KiB while
        // the live WAL, which the memtables cannot exceed by much, is large
        let default_only = match (memtable, wal_alive) {
            (Some(m), Some(w)) => w > 64 << 20 && m < w / 50,
            _ => false,
        };
        rows.push(if memtable.is_none() {
            r.note(kv_why(d))
        } else if default_only {
            r.note("default column family only (tiny next to the live WAL): the sharded families sit in 'heap, not in any mempool'")
        } else {
            r.note("all column families with the GetAggregatedIntProperty fix in dump_objectstore_kv_stats; an older daemon reports the default one")
        });
        let r = Row::new(
            "nocache:sst_readers",
            "RocksDB table readers outside the block cache",
            sst_readers,
            "dump_objectstore_kv_stats rocksdb_index_filter_blocks_usage (rocksdb.estimate-table-readers-mem)",
        )
        .optional();
        rows.push(if sst_readers.is_none() {
            r.note("n/a: same reason as the memtables row")
        } else if default_only {
            r.note("default column family only")
        } else {
            r.note("all column families only with the fix; see the memtables row")
        });
        rows.push(
            Row::new(
                "ref:wal_alive",
                "RocksDB WAL alive (BlueFS db.wal files)",
                wal_alive,
                "bluefs files list: sum of db.wal/* sizes",
            )
            .reference()
            .optional()
            .note("every write still in a memtable is in a live WAL; the memtables run ~10 % above it plus a fixed arena block per column family; a WAL file lives until every column family flushed it"),
        );
    }
    if d.kind == "osd" {
        rows.push(
            group("nocache:pglog", "PG log (entries and dups)", PGLOG_POOLS)
                .note("mempool counts 552 B/entry, 104 B/dup; real ~785 and 178"),
        );
        rows.push(
            Row::new(
                "ref:pglog_uncounted",
                "PG log beyond the mempool count (estimate)",
                pglog_uncounted(d),
                "osd_pglog items and bytes: 233 B/entry + 74 B/dup over the counted 552/104",
            )
            .reference()
            .optional()
            .note(
                "index maps, list nodes, size-class rounding; sits in 'heap, not in any mempool'",
            ),
        );
    }
    rows.push(group("nocache:osdmap", "OSD maps", OSDMAP_POOLS));
    if is_bluestore {
        rows.push(group("nocache:alloc", "BlueStore allocator", ALLOC_POOLS));
        rows.push(group(
            "nocache:writes",
            "BlueStore in-flight writes",
            WRITE_POOLS,
        ));
        rows.push(group("nocache:bluefs", "BlueFS", BLUEFS_POOLS));
    }
    rows.push(group(
        "nocache:buffers",
        "buffers (messages, bufferlists)",
        BUFFER_POOLS,
    ));
    let rest: Vec<&str> = pools
        .keys()
        .filter(|k| !grouped.contains(&k.as_str()) && pools[*k] > 0)
        .map(|k| k.as_str())
        .collect();
    if have_pools {
        let r = group("nocache:other", "other mempools", &rest);
        rows.push(r);
    }
    let attributed: u64 = rows
        .iter()
        .filter(|r| !r.reference)
        .filter_map(|r| r.value)
        .sum();
    a.heap = d.heap.as_deref().and_then(parse_heap_stats);
    let mapped = perf
        .and_then(|p| perf_u64(p, pricache, "mapped_bytes"))
        .filter(|v| *v > 0);
    heap_rows(
        &mut a,
        &mut rows,
        mapped,
        p.and_then(|p| p.rss_anon),
        attributed,
        have_pools,
    );
    if let Some(p) = p {
        rows.push(
            Row::new(
                "rss:file",
                "file-backed (binary, libraries, mapped files)",
                p.rss_file,
                "RssFile",
            )
            .note("in the node's page cache"),
        );
        rows.push(Row::new(
            "rss:shmem",
            "shared memory",
            p.rss_shmem,
            "RssShmem",
        ));
    }
    let sum = known_sum(&rows);
    a.sum = Some(sum);
    if let Some(rss) = a.rss {
        // the identity: every summed row known, and the sum the RSS; optional rows may be
        // unknown, their bytes then sit in the residual
        let unknown: Vec<&str> = rows
            .iter()
            .filter(|r| !r.reference && !r.optional && r.value.is_none() && r.signed.is_none())
            .map(|r| r.id.as_str())
            .collect();
        if !unknown.is_empty() {
            a.problems.push(format!(
                "identity not closable: {} unknown",
                unknown.join(" ")
            ));
        } else if sum != rss as i128 {
            a.problems
                .push(format!("daemon rows sum to {} not RSS {}", sum, rss));
        }
        let resid: u64 = rows
            .iter()
            .filter(|r| r.residual)
            .filter_map(|r| r.value)
            .sum();
        a.residual_pct = crate::units::percent(resid, rss);
    }
    if d.errors
        .iter()
        .any(|e| e.starts_with("perf dump") || e.starts_with("dump_mempools"))
    {
        a.problems.extend(d.errors.iter().cloned());
    }
    sides(&mut a, &rows, d, p, perf, pricache, have_pools);
    a.rows = rows;
    a
}

/// Where the heap figures come from; it decides what the residual row can be called.
#[derive(Clone, Copy, PartialEq)]
enum HeapSource {
    /// tcmalloc heap stats: the application's in-use bytes and the free lists are told apart.
    Tcmalloc,
    /// Only the priority-cache tuner's mapped bytes: in-use and free lists together.
    Tuner,
    /// Neither: RssAnon bounds everything.
    None,
}

/// The rows after the mempools: what the application holds beyond them (the residual), the
/// allocator's free lists, heap pages held but not resident, and the anon memory outside the
/// heap. `attributed` is what the rows so far already account for.
fn heap_rows(
    a: &mut DaemonAccount,
    rows: &mut Vec<Row>,
    mapped: Option<u64>,
    anon: Option<u64>,
    attributed: u64,
    have_pools: bool,
) {
    // the heap's resident part, and the application's share of it
    let (source, resident, in_use, free) = match (&a.heap, mapped) {
        (Some(h), _) => {
            // tcmalloc's "actual" counts pages it holds, resident or not: a span handed out by the
            // OS and never touched is not in RssAnon. Whatever exceeds RssAnon is taken from the
            // free lists first, then from the in-use bytes, so the rows still add up to RSS.
            let resident = anon.map_or(h.actual, |an| an.min(h.actual));
            let nonres = h.actual - resident;
            let free = h.actual.saturating_sub(h.in_use).saturating_sub(nonres);
            (
                HeapSource::Tcmalloc,
                Some(resident),
                Some(resident - free),
                Some((free, nonres)),
            )
        }
        (None, Some(m)) => {
            let m = anon.map_or(m, |an| an.min(m));
            (HeapSource::Tuner, Some(m), Some(m), None)
        }
        (None, None) => (HeapSource::None, None, anon, None),
    };
    // the residual: in use beyond the rows above
    let (id, label, label_no_pools, src, note, note_negative, problem) = match source {
        HeapSource::Tcmalloc => (
            "heap:unattributed",
            "heap, not in any mempool",
            "heap in use (no mempool data)",
            "tcmalloc in use - rows above",
            "mostly the PG log beyond its count (estimate below), size-class rounding on mempool objects, startup objects (log and perf buffers, scrubbers)",
            "negative: mempools claim more than the resident heap holds",
            "mempool rows exceed tcmalloc in-use bytes",
        ),
        HeapSource::Tuner => (
            "heap:rest",
            "heap not in any mempool + allocator free lists",
            "heap (no mempool data)",
            "pricache mapped_bytes - rows above",
            "no heap stats: the two cannot be told apart",
            "negative: inputs disagree",
            "mempool rows exceed mapped heap",
        ),
        HeapSource::None => (
            "anon:unattributed",
            "anon, unattributed (no heap stats)",
            "anon, unattributed (no heap stats)",
            "RssAnon - rows above",
            "",
            "negative: mempools exceed RssAnon",
            "mempool rows exceed RssAnon",
        ),
    };
    rows.push(match in_use {
        None => Row::new(id, label, None, src)
            .note("process not readable")
            .residual(),
        Some(u) if (u as i128) < attributed as i128 => {
            a.problems.push(problem.into());
            Row::new(id, label, None, src)
                .note(note_negative)
                .residual()
        }
        Some(u) if have_pools => Row::new(id, label, Some(u - attributed), src)
            .note(note)
            .residual(),
        Some(u) => Row::new(id, label_no_pools, Some(u - attributed), src).residual(),
    });
    if let (Some(h), Some((free, nonres))) = (&a.heap, free) {
        rows.push(
            Row::new(
                "heap:free",
                "allocator: free lists and metadata",
                Some(free),
                "tcmalloc actual - in use",
            )
            .note(format!(
                "allocator_overhead {} = page heap {} central {} transfer {} thread {} meta {}; {} of it not resident",
                crate::units::bytes(Some(free + nonres)),
                crate::units::bytes(Some(h.pageheap_free)),
                crate::units::bytes(Some(h.central_free)),
                crate::units::bytes(Some(h.transfer_free)),
                crate::units::bytes(Some(h.thread_free)),
                crate::units::bytes(Some(h.metadata)),
                crate::units::bytes(Some(nonres))
            )),
        );
        if nonres > 0 {
            rows.push(
                Row::new(
                    "ref:heap_nonresident",
                    "heap held but not resident",
                    Some(nonres),
                    "tcmalloc actual - RssAnon",
                )
                .reference(),
            );
        }
    }
    // anon memory the heap does not hold: thread stacks, other allocators
    if let Some(res) = resident {
        let src = match source {
            HeapSource::Tcmalloc => "RssAnon - resident heap",
            _ => "RssAnon - mapped_bytes",
        };
        let label = "anon outside heap (stacks, other allocators)";
        rows.push(match anon {
            Some(an) => Row::new("anon:outside_heap", label, Some(an - res), src).residual(),
            None => Row::new("anon:outside_heap", label, None, src)
                .note("process not readable")
                .residual(),
        });
    }
}

/// The kernel side, the Ceph side, and the checks between them.
fn sides(
    a: &mut DaemonAccount,
    rows: &[Row],
    d: &RawDaemon,
    p: Option<&ProcInfo>,
    perf: Option<&serde_json::Value>,
    pricache: &str,
    have_pools: bool,
) {
    let row = |id: &str| rows.iter().find(|r| r.id == id).and_then(|r| r.value);
    let sum_rows = |ids: &[&str]| -> Option<u64> {
        let mut t = 0u64;
        let mut any = false;
        for id in ids {
            if let Some(v) = row(id) {
                t += v;
                any = true;
            }
        }
        if any {
            Some(t)
        } else {
            None
        }
    };
    a.kernel = KernelSide {
        pid: d.pid,
        threads: p.and_then(|p| p.threads),
        rss: p.and_then(|p| p.rss),
        anon: p.and_then(|p| p.rss_anon),
        file: p.and_then(|p| p.rss_file),
        shmem: p.and_then(|p| p.rss_shmem),
        hwm: p.and_then(|p| p.vm_hwm),
        cgroup_path: a.cgroup.as_ref().map(|c| c.path.clone()),
        cgroup_current: a.cgroup.as_ref().and_then(|c| c.current),
        cgroup_max: a.cgroup.as_ref().and_then(|c| c.max),
        cgroup_peak: a.cgroup.as_ref().and_then(|c| c.peak),
        cgroup_procs: a.cgroup.as_ref().and_then(|c| c.nprocs),
    };
    let mempool_total = d
        .mempools
        .as_ref()
        .and_then(|m| m.pointer("/mempool/total/bytes"))
        .and_then(|v| v.as_u64());
    let heap_in_use = a.heap.as_ref().map(|h| h.in_use);
    let heap_actual = a.heap.as_ref().map(|h| h.actual);
    let rss = a.kernel.rss;
    let cache_pools = sum_rows(&["cache:data", "cache:meta"]);
    let untracked = Derived::sub(heap_in_use, mempool_total);
    let overhead = Derived::sub(heap_actual, heap_in_use);
    let non_heap = Derived::sub(rss, heap_actual);
    a.ceph = CephSide {
        target_key: a.target.as_ref().map(|t| t.0.clone()),
        target: a.target.as_ref().map(|t| t.1),
        tuner_target: perf.and_then(|p| perf_u64(p, pricache, "target_bytes")),
        mempool_total,
        heap_in_use,
        heap_actual,
        untracked: untracked.known(),
        allocator_overhead: overhead.known(),
        non_heap: non_heap.known(),
        caches_used: sum_rows(&["cache:data", "cache:meta", "cache:kv", "cache:kv_onode"]),
        mempool_caches: cache_pools,
        block_caches: sum_rows(&["cache:kv", "cache:kv_onode"]),
        uncached_mempool: match (mempool_total, cache_pools) {
            (Some(t), Some(c)) => Some(t.saturating_sub(c)),
            (Some(t), None) => Some(t),
            _ => None,
        },
        tuner_mapped: perf
            .and_then(|p| perf_u64(p, pricache, "mapped_bytes"))
            .filter(|v| *v > 0),
        tuner_unmapped: perf.and_then(|p| perf_u64(p, pricache, "unmapped_bytes")),
        mempool_rows: if have_pools {
            sum_rows(MEMPOOL_ROWS)
        } else {
            None
        },
    };
    let c = &a.ceph;
    let k = &a.kernel;
    let b = crate::units::bytes;
    let mut m = Vec::new();
    // the identity between the two sides
    if let (Some(r), Some(mt), Some(_), Some(_)) = (rss, mempool_total, heap_in_use, heap_actual) {
        let (ok, detail) = match (untracked, overhead, non_heap) {
            (Derived::Known(u), Derived::Known(o), Derived::Known(n)) => (
                mt + u + o + n == r,
                format!(
                    "{} = {} + {} + {} + {}",
                    b(Some(r)),
                    b(Some(mt)),
                    b(Some(u)),
                    b(Some(o)),
                    b(Some(n))
                ),
            ),
            (Derived::Negative, _, _) => (
                false,
                format!(
                    "heap_in_use {} < mempool_total {}: mempools claim more than the heap holds",
                    b(heap_in_use),
                    b(Some(mt))
                ),
            ),
            (_, Derived::Negative, _) => (
                false,
                format!(
                    "heap_actual {} < heap_in_use {}",
                    b(heap_actual),
                    b(heap_in_use)
                ),
            ),
            (_, _, Derived::Negative) => (
                false,
                format!(
                    "heap_actual {} > rss {}: {} of the heap is not resident",
                    b(heap_actual),
                    b(Some(r)),
                    b(Some(heap_actual.unwrap() - r))
                ),
            ),
            _ => (false, "an input is missing".into()),
        };
        m.push(Match {
            what: "rss = mempool_total + untracked + allocator_overhead + non_heap".into(),
            ok,
            hard: true,
            detail,
        });
    }
    if let (Some(x), Some(y)) = (c.mempool_rows, c.mempool_total) {
        m.push(Match {
            what: "mempool rows = dump_mempools total".into(),
            ok: x == y,
            hard: true,
            detail: format!("{} vs {}", b(Some(x)), b(Some(y))),
        });
    }
    if let (Some(x), Some(y)) = (c.tuner_mapped, c.heap_actual) {
        // the same tcmalloc counters, but the tuner samples them on its own timer (the cache
        // resize interval), so its view lags the heap stats by up to a few seconds
        m.push(Match {
            what: "tuner mapped = tcmalloc actual".into(),
            ok: close(x, y, 10, 8 << 20),
            hard: false,
            detail: format!(
                "{} vs {}{}",
                b(Some(x)),
                b(Some(y)),
                if close(x, y, 10, 8 << 20) {
                    ""
                } else {
                    ": the tuner's view is stale or reads another heap"
                }
            ),
        });
    }
    if let (Some(x), Some(y)) = (c.target, c.tuner_target) {
        m.push(Match {
            what: "configured target = tuner target".into(),
            ok: x == y,
            hard: false,
            detail: if x == y { b(Some(x)) } else { format!("{} vs {}: the tuner was given another target (cgroup limit ratio, or a runtime change)", b(Some(x)), b(Some(y))) },
        });
    }
    if let (Some(cg), Some(an), Some(1)) = (k.cgroup_current, k.anon, k.cgroup_procs) {
        m.push(Match {
            what: "cgroup current covers rss anon".into(),
            ok: cg >= an,
            hard: false,
            detail: if cg >= an {
                format!(
                    "{} charged, {} anon: {} is file cache and kernel memory charged to it",
                    b(Some(cg)),
                    b(Some(an)),
                    b(Some(cg - an))
                )
            } else {
                format!(
                    "{} charged < {} anon: not this process's cgroup?",
                    b(Some(cg)),
                    b(Some(an))
                )
            },
        });
    }
    for x in &m {
        if !x.ok && x.hard {
            a.problems.push(format!("{}: {}", x.what, x.detail));
        }
    }
    a.matches = m;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::MemInfo;

    fn meminfo(pairs: &[(&str, u64)]) -> MemInfo {
        MemInfo(pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect())
    }

    fn proc_(pid: u32, comm: &str, anon: u64, cmd: &[&str]) -> ProcInfo {
        ProcInfo {
            pid,
            comm: comm.into(),
            cmdline: cmd.iter().map(|s| s.to_string()).collect(),
            rss: Some(anon + 1000),
            rss_anon: Some(anon),
            rss_file: Some(1000),
            rss_shmem: Some(0),
            vm_hwm: Some(anon + 5000),
            ..Default::default()
        }
    }

    #[test]
    fn node_rows_sum_to_memtotal() {
        let s = RawSample {
            meminfo: meminfo(&[
                ("MemTotal", 1000),
                ("MemFree", 300),
                ("Cached", 200),
                ("Buffers", 10),
                ("Shmem", 20),
                ("Slab", 100),
                ("KernelStack", 5),
                ("PageTables", 5),
                ("Percpu", 10),
                ("AnonPages", 300),
                ("HugePages_Total", 0),
                ("Hugepagesize", 2048),
            ]),
            procs: vec![
                proc_(1, "systemd", 50, &["systemd"]),
                proc_(2, "ceph-osd", 200, &["ceph-osd", "-i", "0"]),
            ],
            ..Default::default()
        };
        let a = node_account(&s);
        assert!(a.problems.is_empty(), "{:?}", a.problems);
        assert_eq!(a.sum, Some(1000));
        let by = |id: &str| a.rows.iter().find(|r| r.id == id).unwrap().clone();
        assert_eq!(by("node:pagecache").value, Some(190));
        assert_eq!(by("anon:osd.0").value, Some(200));
        assert_eq!(by("anon:other").value, Some(50));
        assert_eq!(by("anon:noproc").value, Some(50));
        assert_eq!(
            by("node:other").value,
            Some(1000 - 300 - 190 - 20 - 100 - 20 - 300)
        );
        assert!(by("ref:sockets").reference);
    }

    #[test]
    fn shared_anon_is_a_signed_correction_not_an_error() {
        let s = RawSample {
            meminfo: meminfo(&[
                ("MemTotal", 1000),
                ("MemFree", 300),
                ("Cached", 200),
                ("Buffers", 0),
                ("Shmem", 0),
                ("Slab", 100),
                ("KernelStack", 0),
                ("PageTables", 0),
                ("Percpu", 0),
                ("AnonPages", 300),
            ]),
            procs: vec![proc_(1, "a", 250, &["a"]), proc_(2, "b", 250, &["b"])],
            ..Default::default()
        };
        let a = node_account(&s);
        assert_eq!(a.sum, Some(1000));
        let r = a.rows.iter().find(|r| r.id == "anon:shared").unwrap();
        assert_eq!(r.signed, Some(-200));
        assert!(a.problems.is_empty());
    }

    #[test]
    fn missing_meminfo_field_is_unknown_not_zero() {
        let s = RawSample {
            meminfo: meminfo(&[("MemTotal", 1000), ("MemFree", 300)]),
            ..Default::default()
        };
        let a = node_account(&s);
        assert_eq!(
            a.rows.iter().find(|r| r.id == "node:other").unwrap().value,
            None
        );
        assert!(!a.problems.is_empty());
    }

    #[test]
    fn daemon_names_from_cmdlines() {
        let c = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            daemon_name_from_cmdline("osd", &c(&["ceph-osd", "-i", "3", "-f"])),
            Some("osd.3".into())
        );
        assert_eq!(
            daemon_name_from_cmdline("osd", &c(&["ceph-osd", "--id=12"])),
            Some("osd.12".into())
        );
        assert_eq!(
            daemon_name_from_cmdline("mon", &c(&["ceph-mon", "-n", "mon.a"])),
            Some("mon.a".into())
        );
        assert_eq!(
            daemon_name_from_cmdline("mgr", &c(&["ceph-mgr", "--name", "mgr.x"])),
            Some("mgr.x".into())
        );
        assert_eq!(daemon_name_from_cmdline("osd", &c(&["ceph-osd"])), None);
    }

    #[test]
    fn heap_stats_parse() {
        let t = "osd.0 tcmalloc heap stats:---\nMALLOC:      730987432 (  697.1 MiB) Bytes in use by application\nMALLOC: +            0 (    0.0 MiB) Bytes in page heap freelist\nMALLOC: +     10270776 (    9.8 MiB) Bytes in central cache freelist\nMALLOC: +      8222976 (    7.8 MiB) Bytes in transfer cache freelist\nMALLOC: +     15053600 (   14.4 MiB) Bytes in thread cache freelists\nMALLOC: +      6422528 (    6.1 MiB) Bytes in malloc metadata\nMALLOC:   ------------\nMALLOC: =    770957312 (  735.2 MiB) Actual memory used (physical + swap)\nMALLOC: +       794624 (    0.8 MiB) Bytes released to OS (aka unmapped)\n";
        let h = parse_heap_stats(t).unwrap();
        assert_eq!(h.in_use, 730987432);
        assert_eq!(h.actual, 770957312);
        assert_eq!(h.unmapped, 794624);
        assert_eq!(h.thread_free, 15053600);
        assert!(parse_heap_stats("no tcmalloc").is_none());
    }

    fn osd_sample(heap: bool) -> (RawDaemon, ProcInfo, RawSample) {
        let perf = serde_json::json!({
            "bluestore-pricache": {"target_bytes": 2000, "mapped_bytes": 600, "unmapped_bytes": 1, "heap_bytes": 601},
            "bluestore-pricache:data": {"committed_bytes": 100}, "bluestore-pricache:meta": {"committed_bytes": 100},
            "bluestore-pricache:kv": {"committed_bytes": 100}, "bluestore-pricache:kv_onode": {"committed_bytes": 100},
            "rocksdb-cache-default": {"usage": 30}, "rocksdb-cache-O": {"usage": 10}, "rocksdb": {}
        });
        let mp = serde_json::json!({"mempool": {"by_pool": {
            "bluestore_cache_data": {"items": 1, "bytes": 50}, "bluestore_cache_onode": {"items": 1, "bytes": 20}, "bluestore_blob": {"items": 1, "bytes": 5},
            "osd_pglog": {"items": 10, "bytes": 100}, "osdmap": {"items": 1, "bytes": 15}, "bluestore_alloc": {"items": 1, "bytes": 8},
            "buffer_anon": {"items": 1, "bytes": 12}, "osd": {"items": 1, "bytes": 3}, "bloom_filter": {"items": 0, "bytes": 0}
        }, "total": {"items": 17, "bytes": 213}}});
        let heap_text = "osd.0 tcmalloc heap stats:\nMALLOC: 500 ( 0.0 MiB) Bytes in use by application\nMALLOC: + 100 ( 0.0 MiB) Bytes in page heap freelist\nMALLOC: + 0 ( 0.0 MiB) Bytes in central cache freelist\nMALLOC: + 0 ( 0.0 MiB) Bytes in transfer cache freelist\nMALLOC: + 0 ( 0.0 MiB) Bytes in thread cache freelists\nMALLOC: + 0 ( 0.0 MiB) Bytes in malloc metadata\nMALLOC: = 600 ( 0.0 MiB) Actual memory used (physical + swap)\nMALLOC: + 1 ( 0.0 MiB) Bytes released to OS (aka unmapped)\n";
        let d = RawDaemon {
            name: "osd.0".into(),
            kind: "osd".into(),
            pid: Some(7),
            perf: Some(perf),
            mempools: Some(mp),
            heap: if heap { Some(heap_text.into()) } else { None },
            config: [("osd_memory_target".to_string(), "2000".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let p = ProcInfo {
            pid: 7,
            comm: "ceph-osd".into(),
            rss: Some(1000),
            rss_anon: Some(800),
            rss_file: Some(150),
            rss_shmem: Some(50),
            vm_hwm: Some(1200),
            threads: Some(70),
            ..Default::default()
        };
        (d, p, RawSample::default())
    }

    #[test]
    fn daemon_rows_sum_to_rss_with_heap_stats() {
        let (d, p, s) = osd_sample(true);
        let a = daemon_account(&d, Some(&p), &s);
        assert!(a.problems.is_empty(), "{:?}", a.problems);
        assert_eq!(a.sum, Some(1000));
        let by = |id: &str| a.rows.iter().find(|r| r.id == id).unwrap().value;
        assert_eq!(by("cache:data"), Some(50));
        assert_eq!(by("cache:meta"), Some(25));
        assert_eq!(by("cache:kv"), Some(30));
        assert_eq!(by("nocache:pglog"), Some(100));
        assert_eq!(
            by("nocache:other"),
            Some(3),
            "osd pool, not bloom_filter (zero)"
        );
        assert_eq!(
            by("nocache:memtables"),
            None,
            "kv stats absent: unknown, not zero"
        );
        // attributed = 50+25+30+10+100+15+8+0+0+12+3 = 253; in use 500 -> 247 unattributed
        assert_eq!(by("heap:unattributed"), Some(247));
        assert_eq!(by("heap:free"), Some(100));
        assert_eq!(by("anon:outside_heap"), Some(200));
        assert_eq!(by("rss:file"), Some(150));
        assert_eq!(a.target, Some(("osd_memory_target".into(), 2000)));
    }

    #[test]
    fn daemon_rows_sum_to_rss_without_heap_stats() {
        let (d, p, s) = osd_sample(false);
        let a = daemon_account(&d, Some(&p), &s);
        assert!(a.problems.is_empty(), "{:?}", a.problems);
        assert_eq!(a.sum, Some(1000));
        let by = |id: &str| a.rows.iter().find(|r| r.id == id).unwrap().value;
        assert_eq!(by("heap:rest"), Some(600 - 253));
        assert_eq!(by("anon:outside_heap"), Some(200));
        assert!(a.rows.iter().all(|r| r.id != "heap:free"));
    }

    #[test]
    fn the_two_sides_match_and_mismatches_are_named() {
        let (d, p, s) = osd_sample(true);
        let a = daemon_account(&d, Some(&p), &s);
        assert!(a.problems.is_empty(), "{:?}", a.problems);
        assert_eq!(
            (a.kernel.rss, a.kernel.anon, a.kernel.hwm),
            (Some(1000), Some(800), Some(1200))
        );
        let c = &a.ceph;
        assert_eq!(
            (c.mempool_total, c.heap_in_use, c.heap_actual),
            (Some(213), Some(500), Some(600))
        );
        assert_eq!(c.untracked, Some(500 - 213));
        assert_eq!(c.allocator_overhead, Some(100));
        assert_eq!(c.non_heap, Some(400));
        assert_eq!(c.caches_used, Some(50 + 25 + 30 + 10));
        assert_eq!(c.mempool_caches, Some(50 + 25));
        assert_eq!(c.block_caches, Some(30 + 10));
        assert_eq!(c.uncached_mempool, Some(213 - 75));
        assert_eq!(c.mempool_rows, Some(213));
        assert!(a.matches.iter().all(|m| m.ok), "{:?}", a.matches);
        let id = a
            .matches
            .iter()
            .find(|m| m.what.starts_with("rss = mempool_total"))
            .unwrap();
        assert!(
            id.hard && id.detail.starts_with("1000B = 213B + 287B + 100B + 400B"),
            "{}",
            id.detail
        );
        // the tuner's target differs from the config: soft, explained, not a problem
        let mut d2 = d.clone();
        d2.perf.as_mut().unwrap()["bluestore-pricache"]["target_bytes"] = serde_json::json!(1500);
        let a2 = daemon_account(&d2, Some(&p), &s);
        let t = a2
            .matches
            .iter()
            .find(|m| m.what == "configured target = tuner target")
            .unwrap();
        assert!(!t.ok && !t.hard && t.detail.contains("cgroup"));
        assert!(a2.problems.is_empty());
        // the tuner's heap view disagrees with tcmalloc: soft, explained
        let mut d3 = d.clone();
        d3.perf.as_mut().unwrap()["bluestore-pricache"]["mapped_bytes"] =
            serde_json::json!(50_000_000u64);
        let a3 = daemon_account(&d3, Some(&p), &s);
        let t = a3
            .matches
            .iter()
            .find(|m| m.what == "tuner mapped = tcmalloc actual")
            .unwrap();
        assert!(!t.ok && !t.hard && t.detail.contains("stale"), "{:?}", t);
        assert!(a3.problems.is_empty());
        // heap larger than rss: the identity cannot hold and says why
        let mut p4 = p.clone();
        p4.rss = Some(550);
        let a4 = daemon_account(&d, Some(&p4), &s);
        let id = a4
            .matches
            .iter()
            .find(|m| m.what.starts_with("rss = mempool_total"))
            .unwrap();
        assert!(
            !id.ok && id.detail.contains("not resident"),
            "{}",
            id.detail
        );
        assert_eq!(a4.ceph.non_heap, None);
    }

    #[test]
    fn the_live_wal_is_a_reference_row_and_exposes_a_default_family_only_daemon() {
        let (mut d, p, s) = osd_sample(true);
        d.bluefs_files = Some(serde_json::json!([
            {"ino": 1, "name": "db.wal/000123.log", "size": 100 << 20},
            {"ino": 2, "name": "db.wal/000124.log", "size": 60 << 20},
            {"ino": 3, "name": "db/000100.sst", "size": 900 << 20}
        ]));
        d.kv_stats = Some(
            serde_json::json!({"rocksdb_memtable_usage": "20", "rocksdb_index_filter_blocks_usage": "7"}),
        );
        let a = daemon_account(&d, Some(&p), &s);
        let by = |id: &str| a.rows.iter().find(|r| r.id == id).unwrap();
        assert_eq!(by("ref:wal_alive").value, Some(160 << 20));
        assert!(by("ref:wal_alive").reference);
        assert_eq!(a.sum, Some(1000), "a reference row is not summed");
        assert!(
            by("nocache:memtables")
                .note
                .contains("default column family only"),
            "{}",
            by("nocache:memtables").note
        );
        // a small live WAL says nothing about the families
        d.bluefs_files = Some(serde_json::json!([{"name": "db.wal/000125.log", "size": 10 << 20}]));
        let a = daemon_account(&d, Some(&p), &s);
        let n = &a
            .rows
            .iter()
            .find(|r| r.id == "nocache:memtables")
            .unwrap()
            .note;
        assert!(n.contains("all column families with the"), "{}", n);
    }

    #[test]
    fn the_pg_log_cost_beyond_the_mempool_count_is_estimated_from_items_and_bytes() {
        let (mut d, p, s) = osd_sample(true);
        let by = |a: &DaemonAccount, id: &str| a.rows.iter().find(|r| r.id == id).unwrap().clone();
        // 10 items of 100 bytes cannot be entries at all: all dups
        let a = daemon_account(&d, Some(&p), &s);
        assert_eq!(by(&a, "ref:pglog_uncounted").value, Some(740));
        assert!(by(&a, "ref:pglog_uncounted").reference);
        assert_eq!(a.sum, Some(1000));
        // 100 entries, no dups
        d.mempools.as_mut().unwrap()["mempool"]["by_pool"]["osd_pglog"] =
            serde_json::json!({"items": 100, "bytes": 55200});
        let a = daemon_account(&d, Some(&p), &s);
        assert_eq!(by(&a, "ref:pglog_uncounted").value, Some(23300));
    }

    #[test]
    fn an_unknown_memtable_row_says_why() {
        let (mut d, p, s) = osd_sample(true);
        let note = |d: &RawDaemon| {
            format!(
                "{:?}",
                daemon_account(d, Some(&p), &s)
                    .rows
                    .iter()
                    .find(|r| r.id == "nocache:memtables")
                    .unwrap()
                    .note
            )
        };
        d.config.insert("rocksdb_perf".into(), "false".into());
        d.config
            .insert("rocksdb_collect_memory_stats".into(), "true".into());
        assert!(
            note(&d).contains("rocksdb_perf off on this OSD, --enable-kv-stats sets them"),
            "{}",
            note(&d)
        );
        d.errors
            .push("kv stats: ERROR: RocksDB perf is disabled, can't probe for stats.".into());
        assert!(note(&d).contains("enable declined"), "{}", note(&d));
        d.errors.clear();
        d.config.insert("rocksdb_perf".into(), "true".into());
        assert!(note(&d).contains("not read"), "{}", note(&d));
        d.errors.push("kv stats: no reply within 2000 ms".into());
        assert!(
            note(&d).contains("dump_objectstore_kv_stats: no reply within 2000 ms"),
            "{}",
            note(&d)
        );
    }

    #[test]
    fn kv_stats_flat_and_nested_shapes() {
        let (mut d, p, s) = osd_sample(true);
        d.kv_stats = Some(
            serde_json::json!({"rocksdb_memtable_usage": "40", "rocksdb_index_filter_blocks_usage": "7"}),
        );
        let a = daemon_account(&d, Some(&p), &s);
        let by = |id: &str| a.rows.iter().find(|r| r.id == id).unwrap().value;
        assert_eq!(by("nocache:memtables"), Some(40));
        assert_eq!(by("nocache:sst_readers"), Some(7));
        assert_eq!(
            by("heap:unattributed"),
            Some(247 - 47),
            "the two leave the residual"
        );
        assert_eq!(a.sum, Some(1000));
        d.kv_stats = Some(
            serde_json::json!({"rocksdb_memtable_statistics": {"rocksdb_memtable_usage": "40"}}),
        );
        let a = daemon_account(&d, Some(&p), &s);
        assert_eq!(
            a.rows
                .iter()
                .find(|r| r.id == "nocache:memtables")
                .unwrap()
                .value,
            Some(40)
        );
    }

    #[test]
    fn heap_held_but_not_resident_keeps_the_identity() {
        let (d, mut p, s) = osd_sample(true);
        p.rss_anon = Some(550); // tcmalloc actual is 600: 50 of its pages are not resident
        p.rss = Some(750);
        let a = daemon_account(&d, Some(&p), &s);
        assert!(a.problems.is_empty(), "{:?}", a.problems);
        assert_eq!(a.sum, Some(750));
        let by = |id: &str| a.rows.iter().find(|r| r.id == id).unwrap();
        assert_eq!(
            by("heap:free").value,
            Some(50),
            "the untouched pages come out of the free lists"
        );
        assert_eq!(by("ref:heap_nonresident").value, Some(50));
        assert!(by("ref:heap_nonresident").reference);
        assert_eq!(by("anon:outside_heap").value, Some(0));
    }

    #[test]
    fn disagreeing_inputs_are_unknown_and_named() {
        let (mut d, p, s) = osd_sample(true);
        // mempools claim more than tcmalloc has in use
        d.mempools = Some(
            serde_json::json!({"mempool": {"by_pool": {"osd_pglog": {"items": 1, "bytes": 5000}}}}),
        );
        let a = daemon_account(&d, Some(&p), &s);
        let r = a.rows.iter().find(|r| r.id == "heap:unattributed").unwrap();
        assert_eq!(r.value, None);
        assert!(r.note.contains("negative"));
        assert!(a.problems.iter().any(|p| p.contains("exceed")));
    }
}
