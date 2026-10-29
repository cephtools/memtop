//! Kernel-side sources: /proc/meminfo, /proc/net/sockstat, the process list, cgroup v2 memory
//! files and unix-socket inodes. Everything is read relative to `root` so a recorded tree can
//! stand in for a live host.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// /proc/meminfo in bytes, keyed by the field name without the colon.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MemInfo(pub HashMap<String, u64>);

impl MemInfo {
    pub fn get(&self, k: &str) -> Option<u64> {
        self.0.get(k).copied()
    }
}

pub fn read_meminfo(root: &Path) -> std::io::Result<MemInfo> {
    let text = fs::read_to_string(root.join("proc/meminfo"))?;
    Ok(parse_meminfo(&text))
}

pub fn parse_meminfo(text: &str) -> MemInfo {
    let mut m = HashMap::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(k), Some(v)) = (it.next(), it.next()) else {
            continue;
        };
        let Ok(n) = v.parse::<u64>() else {
            continue;
        };
        let bytes = if it.next() == Some("kB") { n * 1024 } else { n };
        m.insert(k.trim_end_matches(':').to_string(), bytes);
    }
    MemInfo(m)
}

/// Socket buffer memory from /proc/net/sockstat (pages charged to TCP and UDP), in bytes.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SockStat {
    pub tcp_bytes: Option<u64>,
    pub udp_bytes: Option<u64>,
}

pub fn read_sockstat(root: &Path, page: u64) -> SockStat {
    fs::read_to_string(root.join("proc/net/sockstat"))
        .map(|t| parse_sockstat(&t, page))
        .unwrap_or_default()
}

pub fn parse_sockstat(text: &str, page: u64) -> SockStat {
    let mut s = SockStat::default();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let proto = it.next().unwrap_or("");
        let toks: Vec<&str> = it.collect();
        let mem = toks
            .windows(2)
            .find(|w| w[0] == "mem")
            .and_then(|w| w[1].parse::<u64>().ok())
            .map(|p| p * page);
        match proto {
            "TCP:" => s.tcp_bytes = mem,
            "UDP:" => s.udp_bytes = mem,
            _ => {}
        }
    }
    s
}

/// One process as seen in /proc.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ProcInfo {
    pub pid: u32,
    pub comm: String,
    pub cmdline: Vec<String>,
    /// Field 22 of /proc/pid/stat: start time in clock ticks since boot. A new value for the
    /// same pid is a restart.
    pub starttime: u64,
    pub rss: Option<u64>,
    pub rss_anon: Option<u64>,
    pub rss_file: Option<u64>,
    pub rss_shmem: Option<u64>,
    /// VmHWM: the kernel's own record of the peak resident size.
    pub vm_hwm: Option<u64>,
    pub threads: Option<u64>,
    /// cgroup v2 path from /proc/pid/cgroup ("/system.slice/x.service"), if any.
    pub cgroup: Option<String>,
    /// Inodes of the unix sockets this process holds (empty when /proc/pid/fd is not readable).
    pub socket_inodes: Vec<u64>,
    pub fds_readable: bool,
}

pub fn list_processes(root: &Path) -> Vec<ProcInfo> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(root.join("proc")) else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let Ok(pid) = name.to_string_lossy().parse::<u32>() else {
            continue;
        };
        if let Some(p) = read_process(root, pid) {
            out.push(p);
        }
    }
    out.sort_by_key(|p| p.pid);
    out
}

pub fn read_process(root: &Path, pid: u32) -> Option<ProcInfo> {
    let dir = root.join("proc").join(pid.to_string());
    let status = fs::read_to_string(dir.join("status")).ok()?;
    let mut p = ProcInfo {
        pid,
        ..Default::default()
    };
    for line in status.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        let kb = || {
            v.split_whitespace()
                .next()
                .and_then(|x| x.parse::<u64>().ok())
                .map(|x| x * 1024)
        };
        match k {
            "Name" => p.comm = v.to_string(),
            "VmRSS" => p.rss = kb(),
            "RssAnon" => p.rss_anon = kb(),
            "RssFile" => p.rss_file = kb(),
            "RssShmem" => p.rss_shmem = kb(),
            "VmHWM" => p.vm_hwm = kb(),
            "Threads" => p.threads = v.parse().ok(),
            _ => {}
        }
    }
    p.rss?; // a kernel thread: no address space, nothing to account
    p.cmdline = fs::read(dir.join("cmdline"))
        .map(|b| {
            b.split(|c| *c == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect()
        })
        .unwrap_or_default();
    if let Ok(stat) = fs::read_to_string(dir.join("stat")) {
        // the comm field may contain spaces: everything after the closing paren is positional
        if let Some(rest) = stat.rsplit_once(')') {
            p.starttime = rest
                .1
                .split_whitespace()
                .nth(19)
                .and_then(|x| x.parse().ok())
                .unwrap_or(0);
        }
    }
    if let Ok(cg) = fs::read_to_string(dir.join("cgroup")) {
        p.cgroup = cg
            .lines()
            .find_map(|l| l.strip_prefix("0::").map(|s| s.to_string()));
    }
    if let Ok(rd) = fs::read_dir(dir.join("fd")) {
        p.fds_readable = true;
        for f in rd.flatten() {
            if let Ok(t) = fs::read_link(f.path()) {
                let t = t.to_string_lossy();
                if let Some(n) = t.strip_prefix("socket:[").and_then(|s| s.strip_suffix(']')) {
                    if let Ok(n) = n.parse() {
                        p.socket_inodes.push(n);
                    }
                }
            }
        }
    }
    Some(p)
}

/// Memory files of one cgroup v2 directory.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CgroupMem {
    pub path: String,
    pub current: Option<u64>,
    /// memory.peak, where the kernel exposes it (5.19+).
    pub peak: Option<u64>,
    /// memory.max; None when unlimited ("max") or unreadable.
    pub max: Option<u64>,
    pub high: Option<u64>,
    /// Number of processes in the cgroup: 1 means the figure is this daemon's alone.
    pub nprocs: Option<usize>,
}

pub fn read_cgroup(root: &Path, path: &str) -> Option<CgroupMem> {
    let dir = root
        .join("sys/fs/cgroup")
        .join(path.trim_start_matches('/'));
    let num =
        |f: &str| -> Option<u64> { fs::read_to_string(dir.join(f)).ok()?.trim().parse().ok() };
    let current = num("memory.current")?;
    Some(CgroupMem {
        path: path.to_string(),
        current: Some(current),
        peak: num("memory.peak"),
        max: num("memory.max"),
        high: num("memory.high"),
        nprocs: fs::read_to_string(dir.join("cgroup.procs"))
            .ok()
            .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count()),
    })
}

/// Bound unix sockets whose path ends in ".asok": (socket inode, path), from /proc/net/unix.
pub fn asok_sockets(root: &Path) -> Vec<(u64, String)> {
    let Ok(text) = fs::read_to_string(root.join("proc/net/unix")) else {
        return Vec::new();
    };
    parse_net_unix(&text)
}

pub fn parse_net_unix(text: &str) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 8 || !f[7].ends_with(".asok") {
            continue;
        }
        if let Ok(ino) = f[6].parse::<u64>() {
            out.push((ino, f[7].to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_is_bytes() {
        let m = parse_meminfo(
            "MemTotal:       32852348 kB\nMemFree:        17022704 kB\nHugePages_Total:       0\n",
        );
        assert_eq!(m.get("MemTotal"), Some(32852348 * 1024));
        assert_eq!(m.get("HugePages_Total"), Some(0));
        assert_eq!(m.get("Nope"), None);
    }

    #[test]
    fn sockstat_pages() {
        let s = parse_sockstat(
            "sockets: used 1200\nTCP: inuse 33 orphan 0 tw 2 alloc 40 mem 17\nUDP: inuse 6 mem 3\n",
            4096,
        );
        assert_eq!(s.tcp_bytes, Some(17 * 4096));
        assert_eq!(s.udp_bytes, Some(3 * 4096));
    }

    #[test]
    fn net_unix_asok_lines() {
        let t = "Num RefCount Protocol Flags Type St Inode Path\n\
                 0000000000000000: 00000002 00000000 00010000 0001 01 51234 /root/vs/asok/osd.0.asok\n\
                 0000000000000000: 00000002 00000000 00010000 0001 01 51235 /run/dbus/system_bus_socket\n";
        assert_eq!(
            parse_net_unix(t),
            vec![(51234, "/root/vs/asok/osd.0.asok".to_string())]
        );
    }

    #[test]
    fn live_proc_is_readable() {
        let me = read_process(Path::new("/"), std::process::id()).expect("own /proc entry");
        assert!(me.rss.unwrap() > 0);
        assert!(me.starttime > 0);
        assert!(me.fds_readable);
    }
}
