//! ceph-memtop: where is this Ceph node's memory? See README.md.

mod asok;
mod collect;
mod fixture;
mod kvstats;
mod model;
mod peaks;
mod pglog;
mod proc;
mod raw;
mod render;
mod state;
mod ui;
mod units;

use clap::Parser;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Where is this Ceph node's memory? Node and per-daemon accounting that adds up.
#[derive(Parser, Debug, Clone)]
#[command(name = "ceph-memtop", version, about)]
struct Cli {
    /// Refresh period in seconds: the resolution of rates and observed peaks.
    #[arg(long, default_value_t = 2.0)]
    interval: f64,
    /// Admin socket path (repeatable) when discovery is not enough.
    #[arg(long = "asok")]
    asok: Vec<String>,
    /// Directory to search for admin sockets (repeatable). Defaults: /var/run/ceph and its cluster
    /// subdirectories, $VSTART_DEST/asok, ./asok, ./build/asok, ./out, ./build/out, and every
    /// bound *.asok the kernel lists.
    #[arg(long = "asok-dir")]
    asok_dir: Vec<String>,
    /// Per-call socket timeout, ms.
    #[arg(long, default_value_t = 2000)]
    timeout: u64,
    /// Reads spread wider than this within one tick make the sample degraded, ms (default: half the interval).
    #[arg(long)]
    max_spread: Option<u64>,
    /// Take one sample and exit (text, or JSON with --json).
    #[arg(long)]
    once: bool,
    /// Structured output: one JSON document per sample, with current values, peaks and peak times.
    #[arg(long)]
    json: bool,
    /// Text tables every tick instead of the terminal UI.
    #[arg(long)]
    plain: bool,
    /// Verify the two identities (node rows = MemTotal, daemon rows = RSS) and the residuals; exit 1 on failure.
    #[arg(long)]
    check: bool,
    /// Largest acceptable residual for --check, percent of the level's total. A daemon's residual is
    /// what no mempool, cache or allocator counter claims; 30-60% is normal for an OSD with RocksDB
    /// statistics off, or for the mgr's Python.
    #[arg(long, default_value_t = 60)]
    tolerance: u64,
    /// Write every sample to this directory as a fixture.
    #[arg(long)]
    record: Option<PathBuf>,
    /// Replay fixtures from this directory instead of reading the host.
    #[arg(long)]
    replay: Option<PathBuf>,
    /// Hide the peak columns (narrow terminals).
    #[arg(long)]
    no_peaks: bool,
    /// Show the raw source keys (mempool names, meminfo fields) instead of notes.
    #[arg(long)]
    raw: bool,
    /// Do not ask daemons for tcmalloc heap statistics.
    #[arg(long)]
    no_heap: bool,
    /// Turn on rocksdb_perf and rocksdb_collect_memory_stats on OSDs that lack them (asks first;
    /// restored on exit) so RocksDB memtable usage can be read.
    #[arg(long)]
    enable_kv_stats: bool,
    /// Answer yes to the confirmation of --enable-kv-stats.
    #[arg(long)]
    yes: bool,
    /// Ceph conf for the on-request per-PG query (default: the daemon's own -c, or $CEPH_CONF).
    #[arg(long)]
    conf: Option<String>,
    /// Keyring for the on-request per-PG query.
    #[arg(long)]
    keyring: Option<String>,
}

enum Source {
    /// Samples arrive from the collector's thread, paced at the interval.
    Live(std::sync::mpsc::Receiver<raw::RawSample>),
    Replay(Vec<raw::RawSample>, usize),
}

impl Source {
    /// The next sample: blocking for a live source, immediate for a replay.
    fn next(&mut self) -> Option<raw::RawSample> {
        match self {
            Source::Live(rx) => rx.recv().ok(),
            Source::Replay(v, i) => {
                let s = v.get(*i).cloned();
                *i += 1;
                s
            }
        }
    }

    /// A sample if one is ready now, without waiting.
    fn try_next(&mut self) -> Option<raw::RawSample> {
        match self {
            Source::Live(rx) => rx.try_recv().ok(),
            Source::Replay(..) => self.next(),
        }
    }
}

fn start_sampler(
    mut c: collect::Collector,
    interval: Duration,
    stop: Arc<AtomicBool>,
) -> std::sync::mpsc::Receiver<raw::RawSample> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<raw::RawSample>(2);
    std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            let t0 = Instant::now();
            let s = c.sample();
            if tx.send(s).is_err() {
                break;
            }
            let el = t0.elapsed();
            if el < interval {
                std::thread::sleep(interval - el);
            }
        }
    });
    rx
}

fn check_view(v: &state::View, tol: u64) -> Vec<String> {
    let mut bad = Vec::new();
    let n = &v.node;
    let sum: Option<i128> = n
        .rows
        .iter()
        .filter_map(|r| r.row.contribution())
        .fold(Some(0i128), |a, x| a.map(|y| y + x));
    match (sum, n.total) {
        (Some(s), Some(t)) if s == t as i128 => {}
        (Some(s), Some(t)) => bad.push(format!("node: rows sum {} != MemTotal {}", s, t)),
        _ => bad.push("node: identity not closable (missing input)".into()),
    }
    match n.residual_pct {
        Some(p) if p > tol => bad.push(format!("node: residual {}% > {}%", p, tol)),
        None => bad.push("node: residual unknown".into()),
        _ => {}
    }
    for p in &n.problems {
        bad.push(format!("node: {}", p));
    }
    for d in &v.daemons {
        let a = &d.account;
        let Some(rss) = a.rss else {
            continue;
        };
        if d.socket.is_none() || !d.errors.is_empty() {
            continue; // unreadable daemons are reported, not checked
        }
        let complete = d
            .rows
            .iter()
            .filter(|r| !r.row.reference && !r.row.optional)
            .all(|r| r.row.value.is_some() || r.row.signed.is_some());
        let sum: Option<i128> = d
            .rows
            .iter()
            .filter_map(|r| r.row.contribution())
            .fold(Some(0i128), |a, x| a.map(|y| y + x));
        match sum {
            Some(s) if complete && s == rss as i128 => {}
            Some(s) if complete => bad.push(format!("{}: rows sum {} != rss {}", a.name, s, rss)),
            _ => bad.push(format!(
                "{}: identity not closable: {:?}",
                a.name, a.problems
            )),
        }
        match a.residual_pct {
            Some(p) if p > tol => bad.push(format!("{}: residual {}% > {}%", a.name, p, tol)),
            None => bad.push(format!("{}: residual unknown", a.name)),
            _ => {}
        }
        for p in &a.problems {
            bad.push(format!("{}: {}", a.name, p));
        }
    }
    bad
}

fn plain_text(v: &state::View, o: render::Opts) -> String {
    let mut s = String::new();
    for l in render::node_summary(v) {
        s.push_str(&l);
        s.push('\n');
    }
    s.push_str(&render::plain(&render::node_table(v, o)));
    s.push_str(&render::plain(&render::daemons_table(v, o)));
    for d in &v.daemons {
        for l in render::daemon_summary(d, v.sampling_ms) {
            s.push_str(&l);
            s.push('\n');
        }
        s.push_str(&render::plain(&render::daemon_table(d, o)));
    }
    s
}

fn main() {
    let cli = Cli::parse();
    let interval = Duration::from_secs_f64(cli.interval.max(0.2));
    let max_spread = cli
        .max_spread
        .unwrap_or((interval.as_millis() / 2).max(200) as u64);
    let stop = Arc::new(AtomicBool::new(false));
    for sig in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        let _ = signal_hook::flag::register(sig, stop.clone());
    }
    let tui = !(cli.once || cli.json || cli.plain || cli.check);
    {
        let tui = tui;
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if tui {
                ratatui::restore();
            }
            let errs = kvstats::restore_all();
            for e in errs {
                eprintln!("restore: {}", e);
            }
            prev(info);
        }));
    }
    let mut source = match &cli.replay {
        Some(dir) => match fixture::load(dir) {
            Ok(v) if !v.is_empty() => Source::Replay(v, 0),
            Ok(_) => {
                eprintln!("no samples in {}", dir.display());
                std::process::exit(2);
            }
            Err(e) => {
                eprintln!("replay: {}", e);
                std::process::exit(2);
            }
        },
        None => {
            let opts = collect::Options {
                asok: cli.asok.clone(),
                asok_dirs: cli.asok_dir.clone(),
                timeout: Duration::from_millis(cli.timeout),
                budget: Duration::from_millis(
                    (interval.as_millis() as u64 * 3 / 4).clamp(300, cli.timeout.max(300) * 2),
                ),
                kv_stats: cli.enable_kv_stats,
                heap: !cli.no_heap,
            };
            let mut c = collect::Collector::new(opts);
            let procs = proc::list_processes(std::path::Path::new("/"));
            let net = proc::asok_sockets(std::path::Path::new("/"));
            c.discover(&procs, &net);
            if cli.enable_kv_stats {
                let socks: Vec<(String, String)> = c
                    .known
                    .iter()
                    .filter(|d| d.kind == "osd" && d.unreadable.is_none())
                    .filter_map(|d| d.socket.clone().map(|s| (d.name.clone(), s)))
                    .collect();
                let plan = kvstats::plan(&socks, Duration::from_millis(cli.timeout));
                if !plan.is_empty() {
                    eprintln!("--enable-kv-stats would set, until exit:");
                    for p in &plan {
                        eprintln!("  {}: {} = true (now {})", p.daemon, p.var, p.before);
                    }
                    let ok = cli.yes || {
                        eprint!("proceed? [y/N] ");
                        let mut l = String::new();
                        std::io::stdin().read_line(&mut l).ok();
                        l.trim().eq_ignore_ascii_case("y")
                    };
                    if ok {
                        for e in kvstats::apply(&plan, Duration::from_millis(cli.timeout)) {
                            eprintln!("{}", e);
                        }
                    } else {
                        eprintln!("left as found; memtable rows will read n/a");
                    }
                }
            }
            Source::Live(start_sampler(c, interval, stop.clone()))
        }
    };
    let mut st = state::State::new(max_spread);
    let opts = render::Opts {
        peaks: !cli.no_peaks,
        raw: cli.raw,
    };
    let mut failures = 0usize;
    let mut checked = 0usize;
    if !tui {
        let replaying = cli.replay.is_some();
        // --check on a live host is one sample; on a fixture it checks every sample
        let once = cli.once || (cli.check && !replaying);
        loop {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let t0 = Instant::now();
            let Some(s) = source.next() else {
                break;
            };
            if let Some(dir) = &cli.record {
                if let Err(e) = fixture::record(dir, &fixture::lean(&s)) {
                    eprintln!("record: {}", e);
                }
            }
            let v = st.ingest(&s);
            if cli.check {
                checked += 1;
                let bad = check_view(&v, cli.tolerance);
                if bad.is_empty() {
                    println!(
                        "sample {}: node rows = MemTotal, daemon rows = RSS, residual node {}%{}",
                        v.seq,
                        v.node.residual_pct.unwrap_or(0),
                        v.daemons
                            .iter()
                            .filter(|d| d.account.rss.is_some())
                            .map(|d| format!(
                                ", {} {}%",
                                d.account.name,
                                d.account
                                    .residual_pct
                                    .map(|p| p.to_string())
                                    .unwrap_or_else(|| "?".into())
                            ))
                            .collect::<String>()
                    );
                } else {
                    failures += 1;
                    println!("sample {}: FAIL", v.seq);
                    for b in bad {
                        println!("  {}", b);
                    }
                }
            } else if cli.json {
                println!("{}", serde_json::to_string(&v).unwrap());
            } else {
                print!("{}", plain_text(&v, opts));
            }
            if once {
                break;
            }
            let _ = t0;
        }
        for e in kvstats::restore_all() {
            eprintln!("restore: {}", e);
        }
        if cli.check {
            println!("{} samples checked, {} failed", checked, failures);
            std::process::exit(if failures > 0 { 1 } else { 0 });
        }
        return;
    }
    // the terminal UI
    let mut term = ratatui::init();
    let mut u = ui::Ui::new(opts);
    let mut view: Option<state::View> = None;
    let mut next_tick = Instant::now();
    let replay_pace = cli.replay.is_some();
    'outer: loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let due = !replay_pace || Instant::now() >= next_tick;
        if due {
            if let Some(s) = source.try_next() {
                if let Some(dir) = &cli.record {
                    if let Err(e) = fixture::record(dir, &fixture::lean(&s)) {
                        u.message = format!("record: {}", e);
                    }
                }
                view = Some(st.ingest(&s));
                next_tick = Instant::now() + interval;
            } else if replay_pace {
                u.message = "end of fixture".into();
                next_tick = Instant::now() + interval;
            }
        }
        if let Some(v) = &view {
            let _ = term.draw(|f| ui::draw(f, v, &u));
        }
        let wait = if view.is_none() {
            Duration::from_millis(50)
        } else {
            next_tick
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100))
        };
        if event::poll(wait).unwrap_or(false) {
            if let Ok(Event::Key(k)) = event::read() {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                if u.help {
                    u.help = false;
                    continue;
                }
                let rows_len = match (&u.screen, &view) {
                    (ui::Screen::Node, Some(v)) => v.daemons.len(),
                    (ui::Screen::Daemon(n), Some(v)) => v
                        .daemons
                        .iter()
                        .find(|d| &d.account.name == n)
                        .map_or(0, |d| d.rows.len()),
                    _ => 0,
                };
                match (k.code, k.modifiers) {
                    (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        break 'outer
                    }
                    (KeyCode::Char('?'), _) | (KeyCode::Char('h'), _) => u.help = true,
                    (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                        u.cursor = (u.cursor + 1).min(rows_len.saturating_sub(1))
                    }
                    (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                        u.cursor = u.cursor.saturating_sub(1)
                    }
                    (KeyCode::Enter, _) => {
                        if let (ui::Screen::Node, Some(v)) = (&u.screen, &view) {
                            if let Some(d) = v.daemons.get(u.cursor) {
                                u.screen = ui::Screen::Daemon(d.account.name.clone());
                                u.cursor = 0;
                            }
                        }
                    }
                    (KeyCode::Esc, _) | (KeyCode::Backspace, _) => {
                        if u.pglog.is_some() {
                            u.pglog = None;
                        } else if let ui::Screen::Daemon(n) = &u.screen {
                            let n = n.clone();
                            u.screen = ui::Screen::Node;
                            u.cursor = view
                                .as_ref()
                                .and_then(|v| v.daemons.iter().position(|d| d.account.name == n))
                                .unwrap_or(0);
                        }
                    }
                    (KeyCode::Char('p'), _) => u.opts.peaks = !u.opts.peaks,
                    (KeyCode::Char('x'), _) => u.opts.raw = !u.opts.raw,
                    (KeyCode::Char('R'), _) => {
                        st.reset_all_peaks();
                        u.message = "all observed peaks reset".into();
                    }
                    (KeyCode::Char('r'), _) => match (&u.screen, &view) {
                        (ui::Screen::Node, Some(v)) => {
                            if let Some(d) = v.daemons.get(u.cursor) {
                                st.reset_peak(Some(&d.account.name), "rss");
                                st.reset_peak(None, &format!("anon:{}", d.account.name));
                                u.message = format!("{}: observed peaks reset", d.account.name);
                            }
                        }
                        (ui::Screen::Daemon(n), Some(v)) => {
                            if let Some(r) = v
                                .daemons
                                .iter()
                                .find(|d| &d.account.name == n)
                                .and_then(|d| d.rows.get(u.cursor))
                            {
                                st.reset_peak(Some(n), &r.row.id);
                                u.message = format!("{}: peak reset", r.row.label.trim());
                            }
                        }
                        _ => {}
                    },
                    (KeyCode::Char('g'), _) => {
                        if let (ui::Screen::Daemon(n), Some(v)) = (&u.screen, &view) {
                            if let Some(d) = v.daemons.iter().find(|d| &d.account.name == n) {
                                if d.account.kind == "osd" {
                                    let id: u32 = n.trim_start_matches("osd.").parse().unwrap_or(0);
                                    let (conf, bindir) = (d.conf.clone(), d.bindir.clone());
                                    let q = pglog::PgQuery {
                                        conf: cli
                                            .conf
                                            .clone()
                                            .or(conf)
                                            .or_else(|| std::env::var("CEPH_CONF").ok()),
                                        keyring: cli.keyring.clone(),
                                        bindir,
                                    };
                                    u.message = "querying pg dump ...".into();
                                    let _ = term.draw(|f| ui::draw(f, v, &u));
                                    u.pglog = Some((n.clone(), pglog::fetch(&q, id)));
                                    u.message.clear();
                                } else {
                                    u.message = "per-PG pglog applies to OSDs".into();
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    ratatui::restore();
    for e in kvstats::restore_all() {
        eprintln!("restore: {}", e);
    }
}
