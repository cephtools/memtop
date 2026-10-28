//! The accounting: from a raw sample to rows that add up. Node rows sum to MemTotal; a daemon's
//! rows sum to its resident size. Whatever cannot be attributed is a named row, never dropped.

use crate::proc::ProcInfo;
use crate::raw::{RawDaemon, RawSample};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Mempools that BlueStore's meta priority cache counts as its own (BlueStore.h, MetaCache::_get_used_bytes).
pub const META_CACHE_POOLS: &[&str] = &[
    "bluestore_blob",
    "bluestore_extent",
    "bluestore_cache_buffer",
    "bluestore_cache_meta",
    "bluestore_cache_other",
    "bluestore_cache_onode",
    "bluestore_shared_blob",
    "bluestore_inline_bl",
];
pub const DATA_CACHE_POOLS: &[&str] = &["bluestore_cache_data"];
pub const OSDMAP_POOLS: &[&str] = &["osdmap", "osdmap_mapping", "osd_mapbl", "pgmap"];
pub const ALLOC_POOLS: &[&str] = &["bluestore_alloc"];
pub const BUFFER_POOLS: &[&str] = &["buffer_anon", "buffer_meta"];
pub const WRITE_POOLS: &[&str] = &[
    "bluestore_txc",
    "bluestore_writing",
    "bluestore_writing_deferred",
    "bluestore_fsck",
];
pub const BLUEFS_POOLS: &[&str] = &["bluefs", "bluefs_file_reader", "bluefs_file_writer"];
pub const PGLOG_POOLS: &[&str] = &["osd_pglog"];

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
    a.sum = rows
        .iter()
        .filter_map(|r| r.contribution())
        .fold(Some(0i128), |acc, v| acc.map(|x| x + v));
    if let Some(sum) = a.sum {
        if named.is_some() && sum != total as i128 {
            a.problems
                .push(format!("node rows sum to {} not MemTotal {}", sum, total));
        }
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

/// One daemon's accounting: rows that sum to its RSS, plus the reference figures.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DaemonAccount {
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
        .map_or(false, |p| p.get("bluestore-pricache").is_some());
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
    if perf.map_or(false, |p| p.get("rocksdb-cache-default").is_some()) {
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
    if perf.map_or(false, |p| p.get("rocksdb-cache-O").is_some()) {
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
    if d.kind == "osd" {
        // only the OSD exports it (dump_objectstore_kv_stats); unknown, it sits in the heap residual
        let r = Row::new(
            "nocache:memtables",
            "RocksDB memtables (all column families)",
            memtable,
            "dump_objectstore_kv_stats rocksdb_memtable_usage",
        )
        .optional();
        rows.push(if memtable.is_none() {
            r.note("n/a: inside 'heap, not in any mempool'; --enable-kv-stats reads it")
        } else {
            r
        });
        let r = Row::new(
            "nocache:sst_readers",
            "RocksDB SST index and filter blocks outside the cache",
            sst_readers,
            "dump_objectstore_kv_stats rocksdb_index_filter_blocks_usage",
        )
        .optional();
        rows.push(if sst_readers.is_none() {
            r.note("n/a: inside 'heap, not in any mempool'")
        } else {
            r
        });
    }
    if d.kind == "osd" {
        rows.push(
            group("nocache:pglog", "PG log (entries and dups)", PGLOG_POOLS)
                .note("mempool undercounts: ~785 B/entry, 178 B/dup real"),
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
        "buffers, unattributed (messages, bufferlists)",
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
    // the heap and what is outside it
    a.heap = d.heap.as_deref().and_then(parse_heap_stats);
    let mapped = perf
        .and_then(|p| perf_u64(p, pricache, "mapped_bytes"))
        .filter(|v| *v > 0);
    let anon = p.and_then(|p| p.rss_anon);
    match (&a.heap, mapped) {
        (Some(h), _) => {
            // tcmalloc's "actual" counts pages it holds, resident or not: a span handed out by the
            // OS and never touched is not in RssAnon. Whatever exceeds RssAnon is taken from the
            // free lists first, then from the in-use bytes, so the rows still add up to RSS.
            let heap_resident = anon.map(|an| an.min(h.actual)).unwrap_or(h.actual);
            let nonres = h.actual - heap_resident;
            let free_all = h.actual.saturating_sub(h.in_use);
            let free_res = free_all.saturating_sub(nonres);
            let in_use_res = heap_resident - free_res;
            let un = in_use_res as i128 - attributed as i128;
            if un >= 0 && have_pools {
                rows.push(Row::new("heap:unattributed", "heap, not in any mempool", Some(un as u64), "tcmalloc in use - rows above").note("RocksDB (memtables if n/a above, SST readers), the pglog undercount, C++ objects").residual());
            } else if have_pools {
                rows.push(
                    Row::new(
                        "heap:unattributed",
                        "heap, not in any mempool",
                        None,
                        "tcmalloc in use - rows above",
                    )
                    .note("negative: mempools claim more than the resident heap holds")
                    .residual(),
                );
                a.problems
                    .push("mempool rows exceed tcmalloc in-use bytes".into());
            } else {
                rows.push(
                    Row::new(
                        "heap:unattributed",
                        "heap in use (no mempool data)",
                        Some(in_use_res),
                        "tcmalloc in use",
                    )
                    .residual(),
                );
            }
            rows.push(
                Row::new(
                    "heap:free",
                    "allocator: free lists and metadata",
                    Some(free_res),
                    "tcmalloc actual - in use",
                )
                .note(format!(
                    "page heap {} central {} transfer {} thread {} meta {}",
                    crate::units::bytes(Some(h.pageheap_free)),
                    crate::units::bytes(Some(h.central_free)),
                    crate::units::bytes(Some(h.transfer_free)),
                    crate::units::bytes(Some(h.thread_free)),
                    crate::units::bytes(Some(h.metadata))
                )),
            );
            if nonres > 0 {
                rows.push(
                    Row::new(
                        "ref:heap_nonresident",
                        "heap pages not resident (held, untouched)",
                        Some(nonres),
                        "tcmalloc actual - RssAnon",
                    )
                    .reference(),
                );
            }
            match anon {
                Some(an) => rows.push(
                    Row::new(
                        "anon:outside_heap",
                        "anon outside the heap (stacks, other allocators)",
                        Some(an - heap_resident),
                        "RssAnon - resident heap",
                    )
                    .residual(),
                ),
                None => rows.push(
                    Row::new(
                        "anon:outside_heap",
                        "anon outside the heap",
                        None,
                        "RssAnon - resident heap",
                    )
                    .note("process not readable")
                    .residual(),
                ),
            }
        }
        (None, Some(m)) => {
            let m = anon.map(|an| an.min(m)).unwrap_or(m);
            let un = m as i128 - attributed as i128;
            if un >= 0 && have_pools {
                rows.push(
                    Row::new(
                        "heap:rest",
                        "heap, not in any mempool, plus allocator free lists",
                        Some(un as u64),
                        "pricache mapped_bytes - rows above",
                    )
                    .note("no heap stats: the two cannot be told apart")
                    .residual(),
                );
            } else if have_pools {
                rows.push(
                    Row::new(
                        "heap:rest",
                        "heap, not in any mempool",
                        None,
                        "pricache mapped_bytes - rows above",
                    )
                    .note("negative: inputs disagree")
                    .residual(),
                );
                a.problems.push("mempool rows exceed mapped heap".into());
            } else {
                rows.push(
                    Row::new(
                        "heap:rest",
                        "heap (no mempool data)",
                        Some(m),
                        "pricache mapped_bytes",
                    )
                    .residual(),
                );
            }
            match anon {
                Some(an) => rows.push(
                    Row::new(
                        "anon:outside_heap",
                        "anon outside the heap",
                        Some(an - m),
                        "RssAnon - mapped_bytes",
                    )
                    .residual(),
                ),
                None => rows.push(
                    Row::new(
                        "anon:outside_heap",
                        "anon outside the heap",
                        None,
                        "RssAnon - mapped_bytes",
                    )
                    .residual(),
                ),
            }
        }
        (None, None) => {
            let r = anon.map(|an| an as i128 - attributed as i128);
            match r {
                Some(v) if v >= 0 => rows.push(
                    Row::new(
                        "anon:unattributed",
                        "anon, unattributed (no heap stats)",
                        Some(v as u64),
                        "RssAnon - rows above",
                    )
                    .residual(),
                ),
                Some(_) => {
                    rows.push(
                        Row::new(
                            "anon:unattributed",
                            "anon, unattributed",
                            None,
                            "RssAnon - rows above",
                        )
                        .note("negative: mempools exceed RssAnon")
                        .residual(),
                    );
                    a.problems.push("mempool rows exceed RssAnon".into());
                }
                None => rows.push(
                    Row::new(
                        "anon:unattributed",
                        "anon, unattributed",
                        None,
                        "RssAnon - rows above",
                    )
                    .note("process not readable")
                    .residual(),
                ),
            }
        }
    }
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
    a.sum = rows
        .iter()
        .filter_map(|r| r.contribution())
        .fold(Some(0i128), |acc, v| acc.map(|x| x + v));
    if let (Some(sum), Some(rss)) = (a.sum, a.rss) {
        if rows
            .iter()
            .filter(|r| !r.reference)
            .all(|r| r.value.is_some() || r.signed.is_some())
            && sum != rss as i128
        {
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
    a.rows = rows;
    a
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
            "bluestore-pricache": {"target_bytes": 2000, "mapped_bytes": 700, "unmapped_bytes": 1, "heap_bytes": 701},
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
        assert_eq!(by("heap:rest"), Some(700 - 253));
        assert_eq!(by("anon:outside_heap"), Some(100));
        assert!(a.rows.iter().all(|r| r.id != "heap:free"));
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
