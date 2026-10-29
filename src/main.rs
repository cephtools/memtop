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
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use ratatui::crossterm::ExecutableCommand;
use std::path::{Path, PathBuf};
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
    /// Largest acceptable residual, percent of the level's total; above it a sample fails its checks. A daemon's residual is
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
    /// Flat row lists instead of the tree layout.
    #[arg(long)]
    flat: bool,
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

/// Leave the terminal as it was found: mouse reporting off, then ratatui's own restore.
fn tui_restore() {
    let _ = std::io::stdout().execute(DisableMouseCapture);
    ratatui::restore();
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

/// The collector on its own thread, one sample per interval.
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

/// The host as the source: discover the daemons, settle the kv-stats consent, start sampling.
fn live_source(cli: &Cli, interval: Duration, stop: Arc<AtomicBool>) -> Source {
    let timeout = Duration::from_millis(cli.timeout);
    let mut c = collect::Collector::new(collect::Options {
        asok: cli.asok.clone(),
        asok_dirs: cli.asok_dir.clone(),
        timeout,
        budget: Duration::from_millis(
            (interval.as_millis() as u64 * 3 / 4).clamp(300, cli.timeout.max(300) * 2),
        ),
        kv_stats: cli.enable_kv_stats,
        kv_consented: false,
        heap: !cli.no_heap,
    });
    let root = Path::new("/");
    c.discover(&proc::list_processes(root), &proc::asok_sockets(root));
    if cli.enable_kv_stats {
        let osds: Vec<(String, String)> = c
            .known
            .iter()
            .filter(|d| d.kind == "osd" && d.unreadable.is_none())
            .filter_map(|d| d.socket.clone().map(|s| (d.name.clone(), s)))
            .collect();
        c.opts.kv_consented = kvstats::consent(&osds, timeout, cli.yes);
    }
    Source::Live(start_sampler(c, interval, stop))
}

fn replay_source(dir: &Path) -> Source {
    match fixture::load(dir) {
        Ok(v) if !v.is_empty() => Source::Replay(v, 0),
        Ok(_) => {
            eprintln!("no samples in {}", dir.display());
            std::process::exit(2);
        }
        Err(e) => {
            eprintln!("replay: {}", e);
            std::process::exit(2);
        }
    }
}

/// Write the sample as a fixture when --record asks for it; the error text if that failed.
fn record(cli: &Cli, s: &raw::RawSample) -> Option<String> {
    let dir = cli.record.as_ref()?;
    fixture::record(dir, &fixture::lean(s))
        .err()
        .map(|e| format!("record: {}", e))
}

fn plain_text(v: &state::View, o: render::Opts) -> String {
    let mut s = String::new();
    for l in render::node_summary(v) {
        s.push_str(&l);
        s.push('\n');
    }
    s.push_str(&render::plain(&if o.tree {
        render::node_tree_table(v, o)
    } else {
        render::node_table(v, o)
    }));
    s.push_str(&render::plain(&render::daemons_table(v, o)));
    for d in &v.daemons {
        s.push('\n');
        for l in render::daemon_summary(d, v.sampling_ms) {
            s.push_str(&l);
            s.push('\n');
        }
        s.push_str(&render::plain(&if o.tree {
            render::daemon_tree_table(d, o)
        } else {
            render::daemon_table(d, o)
        }));
    }
    s
}

/// --once, --plain, --json: text or JSON per sample until the source ends or a signal arrives.
/// Returns (samples checked, samples that failed their checks).
fn run_plain(
    cli: &Cli,
    source: &mut Source,
    st: &mut state::State,
    opts: render::Opts,
    stop: &AtomicBool,
) -> (usize, usize) {
    let (mut checked, mut failed) = (0usize, 0usize);
    while !stop.load(Ordering::SeqCst) {
        let Some(s) = source.next() else {
            break;
        };
        if let Some(e) = record(cli, &s) {
            eprintln!("{}", e);
        }
        let v = st.ingest(&s);
        checked += 1;
        if !v.checks.ok {
            failed += 1;
        }
        if cli.json {
            println!("{}", serde_json::to_string(&v).unwrap());
        } else {
            print!("{}", plain_text(&v, opts));
        }
        if cli.once {
            break;
        }
    }
    (checked, failed)
}

/// The terminal UI: draw when the view or the UI state changed, take a sample when one is due
/// (a replay is paced at the interval), and hand every key and mouse event to the UI.
fn run_tui(
    cli: &Cli,
    source: &mut Source,
    st: &mut state::State,
    opts: render::Opts,
    stop: &AtomicBool,
    interval: Duration,
) {
    let mut term = ratatui::init();
    let _ = std::io::stdout().execute(EnableMouseCapture);
    let mut u = ui::Ui::new(opts);
    u.pg_conf = cli.conf.clone();
    u.pg_keyring = cli.keyring.clone();
    let mut view: Option<state::View> = None;
    let mut next_tick = Instant::now();
    let replay_pace = cli.replay.is_some();
    let mut dirty = true;
    while !stop.load(Ordering::SeqCst) {
        if let Some(rx) = &u.pglog_rx {
            if let Ok(r) = rx.try_recv() {
                u.pglog = Some(r);
                u.pglog_rx = None;
                u.message.clear();
                dirty = true;
            }
        }
        if !replay_pace || Instant::now() >= next_tick {
            if let Some(s) = source.try_next() {
                if let Some(e) = record(cli, &s) {
                    u.message = e;
                }
                view = Some(st.ingest(&s));
                next_tick = Instant::now() + interval;
                dirty = true;
            } else if replay_pace {
                u.message = "end of fixture".into();
                next_tick = Instant::now() + interval;
                dirty = true;
            }
        }
        if dirty {
            if let Some(v) = &view {
                let _ = term.draw(|f| ui::draw(f, v, &mut u));
            }
            dirty = false;
        }
        let wait = if view.is_none() {
            Duration::from_millis(50)
        } else {
            next_tick
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100))
        };
        if !event::poll(wait).unwrap_or(false) {
            continue;
        }
        let after = match event::read() {
            Ok(Event::Resize(..)) => ui::After::Redraw,
            Ok(Event::Mouse(m)) => u.on_mouse(m, &view),
            Ok(Event::Key(k)) => u.on_key(k, &mut view, st),
            _ => ui::After::Nothing,
        };
        match after {
            ui::After::Quit => break,
            ui::After::Redraw => dirty = true,
            ui::After::Nothing => {}
        }
    }
    tui_restore();
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
    let tui = !(cli.once || cli.json || cli.plain);
    {
        // a panic must not leave the terminal raw or the daemons' switches changed
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if tui {
                tui_restore();
            }
            for e in kvstats::restore_all() {
                eprintln!("restore: {}", e);
            }
            prev(info);
        }));
    }
    let mut source = match &cli.replay {
        Some(dir) => replay_source(dir),
        None => live_source(&cli, interval, stop.clone()),
    };
    let mut st = state::State::new(max_spread, cli.tolerance);
    let opts = render::Opts {
        peaks: !cli.no_peaks,
        raw: cli.raw,
        tree: !cli.flat,
    };
    let mut failed = 0;
    let mut checked = 0;
    if tui {
        run_tui(&cli, &mut source, &mut st, opts, &stop, interval);
    } else {
        (checked, failed) = run_plain(&cli, &mut source, &mut st, opts, &stop);
    }
    for e in kvstats::restore_all() {
        eprintln!("restore: {}", e);
    }
    if failed > 0 {
        eprintln!("{} of {} samples failed their checks", failed, checked);
        std::process::exit(1);
    }
}
