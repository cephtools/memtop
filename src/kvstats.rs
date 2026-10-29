//! The one thing the tool may change on a daemon: the two RocksDB switches that make
//! dump_objectstore_kv_stats report memtable usage. Changed only on request and with confirmation,
//! restored on exit, including on a signal or a panic.

use crate::asok;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Change {
    pub socket: String,
    pub daemon: String,
    pub var: String,
    pub before: String,
}

static RESTORE: Mutex<Vec<Change>> = Mutex::new(Vec::new());

/// What would have to change on these OSD sockets.
pub fn plan(sockets: &[(String, String)], timeout: Duration) -> Vec<Change> {
    let mut out = Vec::new();
    for (daemon, sock) in sockets {
        for var in ["rocksdb_perf", "rocksdb_collect_memory_stats"] {
            if let Ok(v) = asok::call_json(sock, &asok::cmd("config get", &[("var", var)]), timeout)
            {
                if let Some(cur) = v.get(var).and_then(|x| x.as_str()) {
                    if cur != "true" {
                        out.push(Change {
                            socket: sock.clone(),
                            daemon: daemon.clone(),
                            var: var.into(),
                            before: cur.to_string(),
                        });
                    }
                }
            }
        }
    }
    out
}

/// Say what --enable-kv-stats would change on these OSD sockets, ask unless `yes`, and apply it
/// on consent. The answer covers the run: an OSD that restarts or appears later gets the switches
/// too. True when nothing needs changing.
pub fn consent(sockets: &[(String, String)], timeout: Duration, yes: bool) -> bool {
    let plan = plan(sockets, timeout);
    if plan.is_empty() {
        return true;
    }
    eprintln!("--enable-kv-stats would set, until exit (and again on an OSD that restarts):");
    for p in &plan {
        eprintln!("  {}: {} = true (now {})", p.daemon, p.var, p.before);
    }
    let ok = yes || {
        eprint!("proceed? [y/N] ");
        let mut l = String::new();
        std::io::stdin().read_line(&mut l).ok();
        l.trim().eq_ignore_ascii_case("y")
    };
    if ok {
        for e in apply(&plan, timeout) {
            eprintln!("{}", e);
        }
    } else {
        eprintln!("left as found; memtable rows will read n/a");
    }
    ok
}

pub fn apply(changes: &[Change], timeout: Duration) -> Vec<String> {
    let mut errors = Vec::new();
    for c in changes {
        match asok::call(&c.socket, &asok::cmd_set(&c.var, "true"), timeout) {
            Ok(_) => RESTORE.lock().unwrap().push(c.clone()),
            Err(e) => errors.push(format!("{}: set {}: {}", c.daemon, c.var, e)),
        }
    }
    errors
}

/// Put every changed switch back. Idempotent.
pub fn restore_all() -> Vec<String> {
    let changes: Vec<Change> = std::mem::take(&mut *RESTORE.lock().unwrap());
    let mut errors = Vec::new();
    for c in changes {
        if let Err(e) = asok::call(
            &c.socket,
            &asok::cmd_set(&c.var, &c.before),
            Duration::from_secs(3),
        ) {
            errors.push(format!(
                "{}: restore {}={}: {}",
                c.daemon, c.var, c.before, e
            ));
        }
    }
    errors
}
