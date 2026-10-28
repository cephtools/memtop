//! What one tick reads, before any accounting. Serializable, so a recorded sequence of samples
//! replays exactly.

use crate::proc::{CgroupMem, MemInfo, ProcInfo, SockStat};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RawSample {
    pub seq: u64,
    /// Wall clock, ms since the epoch, at the start of the tick.
    pub wall_ms: u64,
    /// Monotonic clock, ms since the tool started, at the start of the tick.
    pub mono_ms: u64,
    pub page_size: u64,
    pub meminfo: MemInfo,
    pub sockstat: SockStat,
    /// Every process with an address space.
    pub procs: Vec<ProcInfo>,
    /// cgroup memory files, keyed by cgroup path, for the cgroups Ceph daemons live in.
    pub cgroups: HashMap<String, CgroupMem>,
    pub daemons: Vec<RawDaemon>,
    /// How far apart the node-level reads were, ms.
    pub spread_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RawDaemon {
    /// "osd.0", "mon.a", ...
    pub name: String,
    /// "osd", "mon", "mgr", "mds", "rgw", ...
    pub kind: String,
    pub socket: Option<String>,
    pub pid: Option<u32>,
    pub fsid: Option<String>,
    pub version: Option<String>,
    /// `perf dump`
    pub perf: Option<serde_json::Value>,
    /// `dump_mempools`
    pub mempools: Option<serde_json::Value>,
    /// `heap stats` text (tcmalloc); None when the build has no tcmalloc or the call failed.
    pub heap: Option<String>,
    /// `dump_objectstore_kv_stats`, when RocksDB statistics are enabled.
    pub kv_stats: Option<serde_json::Value>,
    /// The few config values the accounting needs (memory targets, stats switches).
    pub config: HashMap<String, String>,
    /// What could not be read, one line each: "perf dump: connect: Permission denied".
    pub errors: Vec<String>,
    /// How far apart this daemon's reads were (from the tick start), ms.
    pub spread_ms: u64,
    /// No fresh data this tick: the row is frozen at the previous values.
    pub stale: bool,
    /// The daemon's own conf file (-c) and binary directory, for the on-request cluster query.
    #[serde(default)]
    pub conf: Option<String>,
    #[serde(default)]
    pub bindir: Option<String>,
}
