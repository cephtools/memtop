//! Live collection: find the daemons, read /proc and the admin sockets once per tick. Socket reads
//! run on their own threads with a budget, so an unresponsive daemon freezes its own row only.

use crate::asok;
use crate::model::{daemon_kind, daemon_name_from_cmdline, target_key};
use crate::proc::{self, ProcInfo};
use crate::raw::{RawDaemon, RawSample};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct Options {
    pub asok: Vec<String>,
    pub asok_dirs: Vec<String>,
    /// Per-call socket timeout.
    pub timeout: Duration,
    /// How long one tick waits for the socket reads before freezing late rows.
    pub budget: Duration,
    /// Read dump_objectstore_kv_stats (needs rocksdb_perf + rocksdb_collect_memory_stats).
    pub kv_stats: bool,
    /// With kv_stats: set the two switches on an OSD found with them off (consent given at start).
    pub kv_enable: bool,
    /// Ask tcmalloc for heap stats.
    pub heap: bool,
}

/// A daemon as discovered: a socket, a process, or both.
#[derive(Clone, Debug, Default)]
pub struct DaemonInfo {
    pub name: String,
    pub kind: String,
    pub socket: Option<String>,
    pub pid: Option<u32>,
    pub fsid: Option<String>,
    pub version: Option<String>,
    /// From the process command line (-c), for the on-request cluster queries.
    pub conf: Option<String>,
    /// The daemon binary's directory: a vstart tree keeps `ceph` next to it.
    pub bindir: Option<String>,
    /// Why the socket could not be used, if it could not.
    pub unreadable: Option<String>,
}

pub struct Collector {
    pub opts: Options,
    root: PathBuf,
    start: Instant,
    seq: u64,
    pub known: Vec<DaemonInfo>,
    probed: HashMap<String, (String, Option<String>, Option<String>)>,
    in_flight: HashMap<String, Arc<AtomicBool>>,
    last: HashMap<String, RawDaemon>,
    no_heap: HashSet<String>,
    ticks_since_discovery: u32,
}

fn now_wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Collector {
    pub fn new(opts: Options) -> Collector {
        Collector {
            opts,
            root: PathBuf::from("/"),
            start: Instant::now(),
            seq: 0,
            known: Vec::new(),
            probed: HashMap::new(),
            in_flight: HashMap::new(),
            last: HashMap::new(),
            no_heap: HashSet::new(),
            ticks_since_discovery: 0,
        }
    }

    /// Socket paths to try: explicit, from the directories, and every bound ".asok" the kernel lists.
    fn candidate_sockets(&self, net_unix: &[(u64, String)]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut push = |p: PathBuf| {
            if let Ok(c) = p.canonicalize() {
                let s = c.to_string_lossy().into_owned();
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        };
        for a in &self.opts.asok {
            push(PathBuf::from(a));
        }
        let mut dirs: Vec<PathBuf> = self.opts.asok_dirs.iter().map(PathBuf::from).collect();
        if let Ok(d) = std::env::var("CEPH_ASOK_DIR") {
            dirs.push(PathBuf::from(d));
        }
        for d in ["/var/run/ceph", "/run/ceph"] {
            dirs.push(PathBuf::from(d));
        }
        if let Ok(v) = std::env::var("VSTART_DEST") {
            dirs.push(PathBuf::from(v).join("asok"));
        }
        if let Ok(cwd) = std::env::current_dir() {
            for d in ["asok", "build/asok", "out", "build/out"] {
                dirs.push(cwd.join(d));
            }
        }
        let mut seen_dirs = HashSet::new();
        for d in dirs {
            let Ok(d) = d.canonicalize() else {
                continue;
            };
            if !seen_dirs.insert(d.clone()) {
                continue;
            }
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    // cephadm keeps one directory per cluster fsid
                    if let Ok(rd2) = std::fs::read_dir(&p) {
                        for e2 in rd2.flatten() {
                            if e2.path().extension().map_or(false, |x| x == "asok") {
                                push(e2.path());
                            }
                        }
                    }
                } else if p.extension().map_or(false, |x| x == "asok") {
                    push(p);
                }
            }
        }
        for (_, p) in net_unix {
            push(PathBuf::from(p));
        }
        out
    }

    /// Ask a socket who it is. Cached per path: a socket keeps its identity for its lifetime.
    /// Ask a socket who it is. A successful answer is cached per path (a socket keeps its
    /// identity for its lifetime); a failure is retried at the next discovery.
    fn probe(&mut self, path: &str) -> Result<(String, Option<String>, Option<String>), String> {
        if let Some(r) = self.probed.get(path) {
            return Ok(r.clone());
        }
        let t = self.opts.timeout;
        let show = asok::call_json(path, &asok::cmd("config show", &[]), t).map_err(|e| e.0)?;
        let name = show
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| "config show has no name".to_string())?;
        let fsid = show
            .get("fsid")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let version = asok::call_json(path, &asok::cmd("version", &[]), t)
            .ok()
            .and_then(|v| {
                v.get("version")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string())
            });
        let r = (name, fsid, version);
        self.probed.insert(path.to_string(), r.clone());
        Ok(r)
    }

    pub fn discover(&mut self, procs: &[ProcInfo], net_unix: &[(u64, String)]) {
        let inode_of: HashMap<&str, u64> = net_unix.iter().map(|(i, p)| (p.as_str(), *i)).collect();
        let mut found: Vec<DaemonInfo> = Vec::new();
        let mut paired: HashSet<u32> = HashSet::new();
        let ceph_procs: Vec<&ProcInfo> = procs
            .iter()
            .filter(|p| daemon_kind(&p.comm).is_some())
            .collect();
        for path in self.candidate_sockets(net_unix) {
            let mut d = DaemonInfo {
                socket: Some(path.clone()),
                ..Default::default()
            };
            match self.probe(&path) {
                Ok((name, fsid, version)) => {
                    d.kind = name.split('.').next().unwrap_or("").to_string();
                    d.name = name;
                    d.fsid = fsid;
                    d.version = version;
                }
                Err(e) => {
                    // name it from the file name so the operator sees which daemon is unreadable
                    let stem = Path::new(&path)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let stem = stem.strip_prefix("ceph-").unwrap_or(&stem).to_string();
                    let stem = stem
                        .rsplit_once('-')
                        .map(|(_, n)| n.to_string())
                        .filter(|n| n.contains('.'))
                        .unwrap_or(stem);
                    d.kind = stem.split('.').next().unwrap_or("").to_string();
                    d.name = stem;
                    d.unreadable = Some(e);
                }
            }
            // the process that holds this socket, by inode; else the process with this name
            let ino = inode_of.get(path.as_str()).copied();
            let by_inode =
                ino.and_then(|i| ceph_procs.iter().find(|p| p.socket_inodes.contains(&i)));
            let by_name = ceph_procs.iter().find(|p| {
                !paired.contains(&p.pid)
                    && daemon_kind(&p.comm)
                        .and_then(|k| daemon_name_from_cmdline(k, &p.cmdline))
                        .as_deref()
                        == Some(d.name.as_str())
            });
            if let Some(p) = by_inode.or(by_name) {
                d.pid = Some(p.pid);
                paired.insert(p.pid);
                d.conf = conf_from_cmdline(&p.cmdline);
                d.bindir = p.cmdline.first().and_then(|b| {
                    Path::new(b)
                        .parent()
                        .map(|x| x.to_string_lossy().into_owned())
                });
            }
            found.push(d);
        }
        for p in ceph_procs {
            if paired.contains(&p.pid) {
                continue;
            }
            let kind = daemon_kind(&p.comm).unwrap_or("").to_string();
            let name = daemon_name_from_cmdline(&kind, &p.cmdline)
                .unwrap_or_else(|| format!("{}[{}]", p.comm, p.pid));
            found.push(DaemonInfo {
                name,
                kind,
                pid: Some(p.pid),
                conf: conf_from_cmdline(&p.cmdline),
                bindir: p.cmdline.first().and_then(|b| {
                    Path::new(b)
                        .parent()
                        .map(|x| x.to_string_lossy().into_owned())
                }),
                unreadable: Some("no admin socket found for this process".into()),
                ..Default::default()
            });
        }
        found.sort_by(|a, b| a.name.cmp(&b.name).then(a.pid.cmp(&b.pid)));
        self.known = found;
        self.ticks_since_discovery = 0;
    }

    /// One tick: everything, under one timestamp.
    pub fn sample(&mut self) -> RawSample {
        let t0 = Instant::now();
        let wall_ms = now_wall_ms();
        let mono_ms = t0.duration_since(self.start).as_millis() as u64;
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
        let meminfo = proc::read_meminfo(&self.root).unwrap_or_default();
        let sockstat = proc::read_sockstat(&self.root, page_size);
        let procs = proc::list_processes(&self.root);
        let net_unix = proc::asok_sockets(&self.root);
        let pids_alive: HashSet<u32> = procs.iter().map(|p| p.pid).collect();
        let lost = self
            .known
            .iter()
            .any(|d| d.pid.map_or(false, |p| !pids_alive.contains(&p)));
        if self.known.is_empty() || lost || self.ticks_since_discovery >= 15 {
            self.discover(&procs, &net_unix);
        }
        self.ticks_since_discovery += 1;
        let mut cgroups = HashMap::new();
        for d in &self.known {
            if let Some(p) = d.pid.and_then(|pid| procs.iter().find(|p| p.pid == pid)) {
                if let Some(cg) = &p.cgroup {
                    if let Some(c) = proc::read_cgroup(&self.root, cg) {
                        cgroups.insert(cg.clone(), c);
                    }
                }
            }
        }
        let (tx, rx) = mpsc::channel::<RawDaemon>();
        let mut expected = 0usize;
        let mut daemons: Vec<RawDaemon> = Vec::new();
        for d in self.known.clone() {
            let base = RawDaemon {
                name: d.name.clone(),
                kind: d.kind.clone(),
                socket: d.socket.clone(),
                pid: d.pid,
                fsid: d.fsid.clone(),
                version: d.version.clone(),
                conf: d.conf.clone(),
                bindir: d.bindir.clone(),
                ..Default::default()
            };
            let (Some(sock), None) = (&d.socket, &d.unreadable) else {
                let mut b = base;
                if let Some(u) = &d.unreadable {
                    b.errors.push(u.clone());
                }
                daemons.push(b);
                continue;
            };
            let flag = self
                .in_flight
                .entry(d.name.clone())
                .or_insert_with(|| Arc::new(AtomicBool::new(false)))
                .clone();
            if flag.swap(true, Ordering::SeqCst) {
                // the previous read has not returned: freeze the row
                let mut b = self.last.get(&d.name).cloned().unwrap_or(base);
                b.stale = true;
                b.errors.push("previous read still in flight".into());
                daemons.push(b);
                continue;
            }
            expected += 1;
            let sock = sock.clone();
            let tx = tx.clone();
            let timeout = self.opts.timeout;
            let want_heap = self.opts.heap && !self.no_heap.contains(&d.name);
            let want_kv = self.opts.kv_stats && d.kind == "osd";
            let kv_enable = self.opts.kv_enable;
            let target = target_key(&d.kind).map(|(k, _)| k.to_string());
            std::thread::spawn(move || {
                let mut r = base;
                match asok::call_json(&sock, &asok::cmd("perf dump", &[]), timeout) {
                    Ok(v) => r.perf = Some(v),
                    Err(e) => r.errors.push(format!("perf dump: {}", e)),
                }
                match asok::call_json(&sock, &asok::cmd("dump_mempools", &[]), timeout) {
                    Ok(v) => r.mempools = Some(v),
                    Err(e) => r.errors.push(format!("dump_mempools: {}", e)),
                }
                if want_heap {
                    match asok::call(&sock, &asok::cmd("heap", &[("heapcmd", "stats")]), timeout) {
                        Ok(t) if t.contains("Bytes in use by application") => r.heap = Some(t),
                        Ok(t) => r.errors.push(format!(
                            "heap stats: {}",
                            t.lines()
                                .next()
                                .unwrap_or("")
                                .chars()
                                .take(80)
                                .collect::<String>()
                        )),
                        Err(e) => r.errors.push(format!("heap stats: {}", e)),
                    }
                }
                let mut keys: Vec<String> = Vec::new();
                if let Some(k) = target {
                    keys.push(k);
                }
                if r.kind == "osd" {
                    keys.push("rocksdb_perf".into());
                    keys.push("rocksdb_collect_memory_stats".into());
                }
                for k in keys {
                    if let Ok(v) =
                        asok::call_json(&sock, &asok::cmd("config get", &[("var", &k)]), timeout)
                    {
                        if let Some(s) = v.get(&k).and_then(|x| x.as_str()) {
                            r.config.insert(k.clone(), s.to_string());
                        }
                    }
                }
                // the consent given at start covers the run: an OSD found with a switch off (it
                // restarted, or it was unreachable at start) gets it set now, restored on exit
                if want_kv && kv_enable {
                    for k in ["rocksdb_perf", "rocksdb_collect_memory_stats"] {
                        if let Some(v) = r.config.get(k).filter(|v| v.as_str() != "true").cloned() {
                            let c = crate::kvstats::Change {
                                socket: sock.clone(),
                                daemon: r.name.clone(),
                                var: k.into(),
                                before: v,
                            };
                            for e in crate::kvstats::apply(&[c], timeout) {
                                r.errors.push(e);
                            }
                        }
                    }
                }
                if want_kv {
                    match asok::call(&sock, &asok::cmd("dump_objectstore_kv_stats", &[]), timeout) {
                        Ok(t) => match serde_json::from_str::<serde_json::Value>(&t) {
                            Ok(v) => r.kv_stats = Some(v),
                            Err(_) => r.errors.push(format!(
                                "kv stats: {}",
                                t.lines()
                                    .next()
                                    .unwrap_or("")
                                    .chars()
                                    .take(80)
                                    .collect::<String>()
                            )),
                        },
                        Err(e) => r.errors.push(format!("kv stats: {}", e)),
                    }
                }
                r.spread_ms = (t0.elapsed()).as_millis() as u64;
                let _ = tx.send(r);
                flag.store(false, Ordering::SeqCst);
            });
        }
        drop(tx);
        let deadline = t0 + self.opts.budget;
        let mut got: HashMap<String, RawDaemon> = HashMap::new();
        while got.len() < expected {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match rx.recv_timeout(deadline - now) {
                Ok(r) => {
                    got.insert(r.name.clone(), r);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        for d in self.known.clone() {
            let (Some(_), None) = (&d.socket, &d.unreadable) else {
                continue;
            };
            if let Some(r) = got.remove(&d.name) {
                if r.errors.iter().any(|e| {
                    e.starts_with("heap stats:")
                        && (e.contains("unknown command")
                            || e.contains("not supported")
                            || e.contains("tcmalloc"))
                }) {
                    self.no_heap.insert(d.name.clone());
                }
                self.last.insert(d.name.clone(), r.clone());
                daemons.push(r);
            } else if !daemons.iter().any(|x| x.name == d.name) {
                // late: the row freezes at its last values; the worker clears its flag when it returns
                let mut b = self
                    .last
                    .get(&d.name)
                    .cloned()
                    .unwrap_or_else(|| RawDaemon {
                        name: d.name.clone(),
                        kind: d.kind.clone(),
                        socket: d.socket.clone(),
                        pid: d.pid,
                        ..Default::default()
                    });
                b.stale = true;
                b.errors.push(format!(
                    "no reply within {} ms",
                    self.opts.budget.as_millis()
                ));
                daemons.push(b);
            }
        }
        drop(rx);
        daemons.sort_by(|a, b| a.name.cmp(&b.name));
        let spread_ms = t0.elapsed().as_millis() as u64;
        self.seq += 1;
        RawSample {
            seq: self.seq - 1,
            wall_ms,
            mono_ms,
            page_size,
            meminfo,
            sockstat,
            procs,
            cgroups,
            daemons,
            spread_ms,
        }
    }
}

pub fn conf_from_cmdline(cmdline: &[String]) -> Option<String> {
    let mut it = cmdline.iter().skip(1);
    while let Some(a) = it.next() {
        if a == "-c" || a == "--conf" {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix("--conf=") {
            return Some(v.to_string());
        }
    }
    None
}
