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
    /// With kv_stats: the operator agreed at start to set the two switches on an OSD found with
    /// them off; the consent covers the run, so a restarted OSD gets them too.
    pub kv_consented: bool,
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
    /// Daemons that answered they have no heap stats, and when: a daemon that is still starting
    /// answers the same (its `heap` command is not registered yet), so it is asked again later.
    /// Daemons whose last `heap stats` was refused: when, and how long to wait before asking
    /// again. The wait doubles up to HEAP_RETRY_MAX, since every refused request is a line in the
    /// daemon's log, and a mgr or rgw of v21.3.0 refuses it for good.
    no_heap: HashMap<String, (Instant, Duration)>,
    ticks_since_discovery: u32,
}

/// How long a "no heap stats" answer holds before the daemon is asked again.
const HEAP_RETRY: Duration = Duration::from_secs(10);
const HEAP_RETRY_MAX: Duration = Duration::from_secs(600);

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
            no_heap: HashMap::new(),
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
                            if e2.path().extension().is_some_and(|x| x == "asok") {
                                push(e2.path());
                            }
                        }
                    }
                } else if p.extension().is_some_and(|x| x == "asok") {
                    push(p);
                }
            }
        }
        for (_, p) in net_unix {
            push(PathBuf::from(p));
        }
        out
    }

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
                    // vstart names the rgw socket radosgw.<port>.asok for client.rgw.<port>
                    let stem = stem
                        .strip_prefix("radosgw.")
                        .map(|p| format!("client.rgw.{}", p))
                        .unwrap_or(stem);
                    d.kind = stem.split('.').next().unwrap_or("").to_string();
                    // a client's socket file outlives its process (rados bench, the ceph CLI):
                    // with nothing listening behind it, it is a leftover, not a daemon
                    if d.kind == "client" && !inode_of.contains_key(path.as_str()) {
                        continue;
                    }
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
                // the socket names a client-type daemon "client.rgw.a"; the binary says what it is
                if let Some(k) = daemon_kind(&p.comm) {
                    d.kind = k.to_string();
                }
                d.conf = conf_from_cmdline(&p.cmdline);
                d.bindir = bindir_of(&p.cmdline);
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
                bindir: bindir_of(&p.cmdline),
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
            .any(|d| d.pid.is_some_and(|p| !pids_alive.contains(&p)));
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
            let want = Wants {
                heap: self.opts.heap
                    && self
                        .no_heap
                        .get(&d.name)
                        .is_none_or(|(t, wait)| t.elapsed() >= *wait),
                kv: self.opts.kv_stats && d.kind == "osd",
                kv_consented: self.opts.kv_consented,
                target_key: target_key(&d.kind).map(|(k, _)| k.to_string()),
            };
            std::thread::spawn(move || {
                let r = read_daemon(&sock, base, &want, timeout, t0);
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
                            || e.contains("Invalid argument")
                            || e.contains("tcmalloc"))
                }) {
                    let wait = self
                        .no_heap
                        .get(&d.name)
                        .map_or(HEAP_RETRY, |(_, w)| (*w * 2).min(HEAP_RETRY_MAX));
                    self.no_heap.insert(d.name.clone(), (Instant::now(), wait));
                } else if r.heap.is_some() {
                    self.no_heap.remove(&d.name);
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

/// What to ask a daemon's socket beyond `perf dump` and `dump_mempools`.
struct Wants {
    /// tcmalloc heap stats; not asked again from a daemon that answered it has none.
    heap: bool,
    /// `dump_objectstore_kv_stats` and the two switches behind it (OSDs).
    kv: bool,
    /// Set a switch found off: the consent given at start covers the run.
    kv_consented: bool,
    /// The config key of this daemon kind's memory target.
    target_key: Option<String>,
}

/// The first line of a reply, for an error message.
fn first_line(t: &str) -> String {
    first_lines(t, 1)
}

fn first_lines(t: &str, n: usize) -> String {
    let l: Vec<&str> = t.lines().take(n).map(str::trim).collect();
    l.join(" ").chars().take(120).collect()
}

/// Every socket read for one daemon, on its own thread: each command's reply or its error.
fn read_daemon(
    sock: &str,
    mut r: RawDaemon,
    want: &Wants,
    timeout: Duration,
    t0: Instant,
) -> RawDaemon {
    match asok::call_json(sock, &asok::cmd("perf dump", &[]), timeout) {
        Ok(v) => r.perf = Some(v),
        Err(e) => r.errors.push(format!("perf dump: {}", e)),
    }
    match asok::call_json(sock, &asok::cmd("dump_mempools", &[]), timeout) {
        Ok(v) => r.mempools = Some(v),
        Err(e) => r.errors.push(format!("dump_mempools: {}", e)),
    }
    if want.heap {
        match asok::call(sock, &asok::cmd("heap", &[("heapcmd", "stats")]), timeout) {
            Ok(t) if t.contains("Bytes in use by application") => r.heap = Some(t),
            // "ERROR: (22) Invalid argument" and, on the next line, why ("unknown command prefix")
            Ok(t) => r.errors.push(format!("heap stats: {}", first_lines(&t, 2))),
            Err(e) => r.errors.push(format!("heap stats: {}", e)),
        }
    }
    let mut keys: Vec<String> = want.target_key.iter().cloned().collect();
    if r.kind == "osd" {
        keys.push("rocksdb_perf".into());
        keys.push("rocksdb_collect_memory_stats".into());
    }
    for k in keys {
        if let Ok(v) = asok::call_json(sock, &asok::cmd("config get", &[("var", &k)]), timeout) {
            if let Some(s) = v.get(&k).and_then(|x| x.as_str()) {
                r.config.insert(k.clone(), s.to_string());
            }
        }
    }
    // the consent given at start covers the run: an OSD found with a switch off (it restarted, or
    // it was unreachable at start) gets it set now, restored on exit
    if want.kv && want.kv_consented {
        for k in ["rocksdb_perf", "rocksdb_collect_memory_stats"] {
            if let Some(v) = r.config.get(k).filter(|v| v.as_str() != "true").cloned() {
                let c = crate::kvstats::Change {
                    socket: sock.to_string(),
                    daemon: r.name.clone(),
                    var: k.into(),
                    before: v,
                };
                r.errors.extend(crate::kvstats::apply(&[c], timeout));
            }
        }
    }
    if want.kv {
        match asok::call(sock, &asok::cmd("dump_objectstore_kv_stats", &[]), timeout) {
            // RocksDBStore::get_statistics writes one top-level object per rocksdb_collect_*
            // switch that is on, back to back; the memory one is wanted. With rocksdb_perf off it
            // writes a plain "error: ..." line instead.
            Ok(t) => {
                let found = serde_json::Deserializer::from_str(&t)
                    .into_iter::<serde_json::Value>()
                    .filter_map(Result::ok)
                    .find(|v| {
                        v.get("rocksdb_memtable_usage").is_some()
                            || v.get("rocksdb_memtable_statistics").is_some()
                    });
                match found {
                    Some(v) => r.kv_stats = Some(v),
                    None if t.trim_start().starts_with('{') => r
                        .errors
                        .push("kv stats: no memory statistics in the reply".into()),
                    None => r.errors.push(format!("kv stats: {}", first_line(&t))),
                }
            }
            Err(e) => r.errors.push(format!("kv stats: {}", e)),
        }
    }
    if r.kind == "osd" {
        match asok::call_json(sock, &asok::cmd("bluefs files list", &[]), timeout) {
            Ok(v) => r.bluefs_files = Some(v),
            Err(e) => r.errors.push(format!("bluefs files list: {}", e)),
        }
    }
    r.spread_ms = t0.elapsed().as_millis() as u64;
    r
}

/// The daemon binary's directory: a vstart tree keeps `ceph` next to it.
fn bindir_of(cmdline: &[String]) -> Option<String> {
    cmdline.first().and_then(|b| {
        Path::new(b)
            .parent()
            .map(|x| x.to_string_lossy().into_owned())
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(dir: &Path) -> Options {
        Options {
            asok: vec![],
            asok_dirs: vec![dir.to_string_lossy().into_owned()],
            timeout: Duration::from_millis(50),
            budget: Duration::from_millis(100),
            kv_stats: false,
            kv_consented: false,
            heap: false,
        }
    }

    #[test]
    fn a_client_socket_file_with_no_listener_is_dropped_but_a_daemons_is_shown_unreadable() {
        let dir = std::env::temp_dir().join(format!("memtop-discover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("client.admin.354107.asok"), b"").unwrap();
        std::fs::write(dir.join("osd.9.asok"), b"").unwrap();
        let mut c = Collector::new(opts(&dir));
        c.discover(&[], &[]);
        let prefix = dir.canonicalize().unwrap().to_string_lossy().into_owned();
        let here: Vec<&DaemonInfo> = c
            .known
            .iter()
            .filter(|d| d.socket.as_deref().is_some_and(|s| s.starts_with(&prefix)))
            .collect();
        let names: Vec<String> = here.iter().map(|d| d.name.clone()).collect();
        let unreadable = here.iter().all(|d| d.unreadable.is_some());
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(names, vec!["osd.9".to_string()]);
        assert!(unreadable);
    }
}
