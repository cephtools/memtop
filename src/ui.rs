//! The terminal UI: the node screen, a daemon's screen, the per-PG pglog popup, and what the keys
//! and the mouse do on them. Drawing is a function of the view and the UI state; the main loop
//! only decides when to draw.

use crate::render::{self, Opts, Table};
use crate::state::{State, View};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table as RTable, TableState, Wrap};
use ratatui::Frame;

#[derive(Clone, Debug, PartialEq)]
pub enum Screen {
    Node,
    Daemon(String),
}

/// The screen areas of the last draw, so mouse events can be placed.
#[derive(Clone, Copy, Debug)]
pub struct Panes {
    /// The summary box.
    pub summary: Rect,
    /// The table the cursor moves in, and the index of its first visible row.
    pub table: Rect,
    pub offset: usize,
    /// The terminal row of the border between the summary and the table, when it can be dragged.
    pub divider: Option<u16>,
}

impl Panes {
    /// The displayed row index under a terminal row, if it is on one of the table's rows.
    pub fn row_at(&self, y: u16) -> Option<usize> {
        let first = self.table.y + 2; // the border and the header
        let last = (self.table.y + self.table.height).saturating_sub(1); // the bottom border
        if y >= first && y < last {
            Some((y - first) as usize + self.offset)
        } else {
            None
        }
    }
}

/// A per-PG query's answer: the daemon it was made for, and its PGs or why it failed.
pub type PgLogReply = (String, Result<Vec<crate::pglog::PgLog>, String>);

pub struct Ui {
    pub screen: Screen,
    pub cursor: usize,
    pub opts: Opts,
    pub message: String,
    /// The daemon screen's summary height, set by dragging the border or with +/-; None fits the text.
    pub split: Option<u16>,
    /// A drag of that border in progress: how far below the summary's bottom border it was grabbed.
    pub drag: Option<u16>,
    /// Where the last draw put the summary and the table.
    pub panes: Option<Panes>,
    pub pglog: Option<PgLogReply>,
    /// A per-PG query in flight, answered on its own thread so the screen keeps refreshing.
    pub pglog_rx: Option<std::sync::mpsc::Receiver<PgLogReply>>,
    pub help: bool,
    /// --conf and --keyring for the per-PG query; the daemon's own -c is the fallback.
    pub pg_conf: Option<String>,
    pub pg_keyring: Option<String>,
}

/// What the main loop does after an event.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum After {
    Nothing,
    Redraw,
    Quit,
}

impl Ui {
    pub fn new(opts: Opts) -> Ui {
        Ui {
            screen: Screen::Node,
            cursor: 0,
            opts,
            message: String::new(),
            split: None,
            drag: None,
            panes: None,
            pglog: None,
            pglog_rx: None,
            help: false,
            pg_conf: None,
            pg_keyring: None,
        }
    }

    /// The accounting ids behind each row of the current screen, in display order: the cursor,
    /// the keys and the mouse act on them.
    pub fn shown_rows(&self, view: &Option<View>) -> Vec<Vec<String>> {
        let Some(v) = view else {
            return Vec::new();
        };
        let t = match &self.screen {
            Screen::Node => render::daemons_table(v, self.opts),
            Screen::Daemon(n) => match v.daemons.iter().find(|d| &d.account.name == n) {
                Some(d) if self.opts.tree => render::daemon_tree_table(d, self.opts),
                Some(d) => render::daemon_table(d, self.opts),
                None => return Vec::new(),
            },
        };
        t.rows.into_iter().map(|r| r.ids).collect()
    }

    pub fn on_key(&mut self, k: KeyEvent, view: &mut Option<View>, st: &mut State) -> After {
        if k.kind != KeyEventKind::Press {
            return After::Nothing;
        }
        if self.help {
            self.help = false;
            return After::Redraw;
        }
        let shown = self.shown_rows(view);
        let last = shown.len().saturating_sub(1);
        match (k.code, k.modifiers) {
            (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                return After::Quit
            }
            (KeyCode::Char('?'), _) | (KeyCode::Char('h'), _) => self.help = true,
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                self.cursor = (self.cursor + 1).min(last)
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                self.cursor = self.cursor.saturating_sub(1)
            }
            (KeyCode::Enter, _) => {
                if let (Screen::Node, Some(v)) = (&self.screen, &*view) {
                    if let Some(d) = v.daemons.get(self.cursor) {
                        self.screen = Screen::Daemon(d.account.name.clone());
                        self.cursor = 0;
                    }
                }
            }
            (KeyCode::Esc, _) | (KeyCode::Backspace, _) => {
                if self.pglog.is_some() {
                    self.pglog = None;
                } else if let Screen::Daemon(n) = &self.screen {
                    // back to the node screen, the cursor on the daemon just left
                    let n = n.clone();
                    self.screen = Screen::Node;
                    self.cursor = view
                        .as_ref()
                        .and_then(|v| v.daemons.iter().position(|d| d.account.name == n))
                        .unwrap_or(0);
                }
            }
            (KeyCode::Char('-'), _) => {
                if let Some(p) = self.panes.filter(|p| p.divider.is_some()) {
                    self.split = Some(p.summary.height.saturating_sub(1));
                }
            }
            (KeyCode::Char('+'), _) | (KeyCode::Char('='), _) => {
                if let Some(p) = self.panes.filter(|p| p.divider.is_some()) {
                    self.split = Some(p.summary.height + 1);
                }
            }
            (KeyCode::Char('p'), _) => self.opts.peaks = !self.opts.peaks,
            (KeyCode::Char('x'), _) => self.opts.raw = !self.opts.raw,
            (KeyCode::Char('t'), _) => self.opts.tree = !self.opts.tree,
            (KeyCode::Char('R'), _) => {
                st.reset_all_peaks();
                self.message = "all observed peaks reset".into();
                *view = st.refresh();
            }
            (KeyCode::Char('r'), _) => {
                self.reset_row(&shown, st);
                *view = st.refresh();
            }
            (KeyCode::Char('g'), _) => self.query_pglog(view),
            _ => {}
        }
        After::Redraw
    }

    /// `r`: the observed peaks of the row under the cursor (a daemon's RSS on the node screen).
    fn reset_row(&mut self, shown: &[Vec<String>], st: &mut State) {
        match (&self.screen, shown.get(self.cursor)) {
            (Screen::Node, Some(ids)) => {
                for name in ids {
                    st.reset_peak(Some(name), "rss");
                    st.reset_peak(None, &format!("anon:{}", name));
                }
                self.message = format!("{}: observed peaks reset", ids.join(" "));
            }
            (Screen::Daemon(n), Some(ids)) => {
                for id in ids {
                    st.reset_peak(Some(n), id);
                }
                self.message = format!(
                    "{} row{} reset",
                    ids.len(),
                    if ids.len() == 1 { "" } else { "s" }
                );
            }
            _ => {}
        }
    }

    /// `g`: the per-PG query for the daemon on screen, answered on its own thread.
    fn query_pglog(&mut self, view: &Option<View>) {
        let (Screen::Daemon(n), Some(v)) = (&self.screen, view) else {
            return;
        };
        let Some(d) = v.daemons.iter().find(|d| &d.account.name == n) else {
            return;
        };
        if d.account.kind != "osd" {
            self.message = "per-PG pglog applies to OSDs".into();
            return;
        }
        if self.pglog_rx.is_some() {
            self.message = "pg dump query still running".into();
            return;
        }
        let id: u32 = n.trim_start_matches("osd.").parse().unwrap_or(0);
        let q = crate::pglog::PgQuery {
            conf: self
                .pg_conf
                .clone()
                .or(d.conf.clone())
                .or_else(|| std::env::var("CEPH_CONF").ok()),
            keyring: self.pg_keyring.clone(),
            bindir: d.bindir.clone(),
        };
        let name = n.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send((name.clone(), crate::pglog::fetch(&q, id)));
        });
        self.pglog_rx = Some(rx);
        self.message = "querying pg dump ...".into();
    }

    pub fn on_mouse(&mut self, m: MouseEvent, view: &Option<View>) -> After {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.help {
                    self.help = false;
                    return After::Redraw;
                }
                let Some(p) = self.panes else {
                    return After::Nothing;
                };
                if let Some(div) = p.divider {
                    // either edge of the border between the two sections grabs it
                    if m.row == div || m.row == div + 1 {
                        self.drag = Some(m.row - div);
                        return After::Nothing;
                    }
                }
                if let Some(i) = p.row_at(m.row) {
                    if i < self.shown_rows(view).len() {
                        self.cursor = i;
                        return After::Redraw;
                    }
                }
                After::Nothing
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let (Some(off), Some(p)) = (self.drag, self.panes) {
                    // the grabbed border follows the pointer: the summary ends there
                    let height = (m.row.saturating_sub(off) + 1).saturating_sub(p.summary.y);
                    if self.split != Some(height) {
                        self.split = Some(height);
                        return After::Redraw;
                    }
                }
                After::Nothing
            }
            MouseEventKind::Up(_) => {
                self.drag = None;
                After::Nothing
            }
            MouseEventKind::ScrollDown => {
                let n = self.shown_rows(view).len();
                self.cursor = (self.cursor + 1).min(n.saturating_sub(1));
                After::Redraw
            }
            MouseEventKind::ScrollUp => {
                self.cursor = self.cursor.saturating_sub(1);
                After::Redraw
            }
            _ => After::Nothing,
        }
    }
}

/// The four caches the tuner sizes (bluestore-pricache data, meta, kv, kv_onode; the mon's kv).
const TUNER_CACHES: [&str; 4] = ["cache:data", "cache:meta", "cache:kv", "cache:kv_onode"];

/// A table row that stands for the tuner's caches and nothing else: a cache leaf, or a tree row
/// whose leaves are all caches. Drawn in colour so the tuned memory stands out from the rest.
fn is_tuner_cache(r: &render::TableRow) -> bool {
    !r.ids.is_empty() && r.ids.iter().all(|id| TUNER_CACHES.contains(&id.as_str()))
}

fn table_widget(t: &Table) -> RTable<'static> {
    let header = Row::new(
        t.header
            .iter()
            .map(|h| Cell::from(h.clone()))
            .collect::<Vec<_>>(),
    )
    .style(Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED));
    let rows: Vec<Row> = t
        .rows
        .iter()
        .map(|r| {
            let mut st = Style::default();
            if is_tuner_cache(r) {
                st = st.fg(Color::Cyan);
            }
            if r.dim {
                st = st.add_modifier(Modifier::DIM);
            }
            if r.warn {
                st = st.add_modifier(Modifier::ITALIC);
            }
            Row::new(
                r.cells
                    .iter()
                    .map(|c| Cell::from(c.clone()))
                    .collect::<Vec<_>>(),
            )
            .style(st)
        })
        .collect();
    let widths: Vec<Constraint> = t.widths.iter().map(|w| Constraint::Length(*w)).collect();
    RTable::new(rows, widths)
        .header(header)
        .block(Block::bordered().title(t.title.clone()))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ")
}

pub fn draw(f: &mut Frame, v: &View, ui: &mut Ui) {
    let area = f.area();
    match &ui.screen {
        Screen::Node => draw_node(f, area, v, ui),
        Screen::Daemon(name) => match v.daemons.iter().find(|d| &d.account.name == name) {
            Some(d) => draw_daemon(f, area, v, d, ui),
            None => draw_node(f, area, v, ui),
        },
    }
    if ui.help {
        draw_help(f, area);
    }
}

/// Two lines: the keys, then the status message, so a message is never cut off on the right.
fn footer(ui: &Ui, extra: &str) -> Paragraph<'static> {
    let keys = format!(
        "q quit  ? help  j/k move  Enter open  Esc back  r reset peak(s) of row  R reset all  p peaks:{}  x keys:{}  t {}  {}",
        if ui.opts.peaks { "on" } else { "off" },
        if ui.opts.raw { "on" } else { "off" },
        if ui.opts.tree { "tree" } else { "flat" },
        extra
    );
    Paragraph::new(vec![Line::from(keys), Line::from(ui.message.clone())])
        .style(Style::default().add_modifier(Modifier::DIM))
}

fn draw_node(f: &mut Frame, area: Rect, v: &View, ui: &mut Ui) {
    let summary = render::node_summary(v);
    let nt = if ui.opts.tree {
        render::node_tree_table(v, ui.opts)
    } else {
        render::node_table(v, ui.opts)
    };
    let dt = render::daemons_table(v, ui.opts);
    let chunks = Layout::vertical([
        Constraint::Length(summary.len() as u16 + 2),
        Constraint::Length(nt.rows.len() as u16 + 3),
        Constraint::Min(5),
        Constraint::Length(2),
    ])
    .split(area);
    let title = format!(
        "ceph-memtop  {}  seq {}",
        crate::units::clock(v.wall_ms),
        v.seq
    );
    f.render_widget(
        Paragraph::new(
            summary
                .iter()
                .map(|s| Line::from(s.clone()))
                .collect::<Vec<_>>(),
        )
        .block(Block::bordered().title(title))
        .wrap(Wrap { trim: false }),
        chunks[0],
    );
    f.render_widget(table_widget(&nt), chunks[1]);
    let mut st = TableState::default();
    if !dt.rows.is_empty() {
        st.select(Some(ui.cursor.min(dt.rows.len() - 1)));
    }
    f.render_stateful_widget(table_widget(&dt), chunks[2], &mut st);
    ui.panes = Some(Panes {
        summary: chunks[0],
        table: chunks[2],
        offset: st.offset(),
        divider: None,
    });
    f.render_widget(
        footer(
            ui,
            "(peaks: seen = since start, resolution one sample; kernel = VmHWM / memory.peak)",
        ),
        chunks[3],
    );
}

fn draw_daemon(f: &mut Frame, area: Rect, v: &View, d: &crate::state::DaemonView, ui: &mut Ui) {
    let summary = render::daemon_summary(d, v.sampling_ms);
    let t = if ui.opts.tree {
        render::daemon_tree_table(d, ui.opts)
    } else {
        render::daemon_table(d, ui.opts)
    };
    // the summary box fits its text unless the border was moved; it keeps at least one line and
    // leaves the table a few rows and the footer its two
    let natural = summary.len() as u16 + 2;
    let top = ui
        .split
        .unwrap_or(natural)
        .clamp(3, area.height.saturating_sub(7).max(3));
    let chunks = Layout::vertical([
        Constraint::Length(top),
        Constraint::Min(5),
        Constraint::Length(2),
    ])
    .split(area);
    f.render_widget(
        Paragraph::new(
            summary
                .iter()
                .map(|s| Line::from(s.clone()))
                .collect::<Vec<_>>(),
        )
        .block(Block::bordered().title(format!(
            "ceph-memtop  {}  {}",
            d.account.name,
            crate::units::clock(v.wall_ms)
        )))
        .wrap(Wrap { trim: false }),
        chunks[0],
    );
    let mut st = TableState::default();
    if !t.rows.is_empty() {
        st.select(Some(ui.cursor.min(t.rows.len() - 1)));
    }
    f.render_stateful_widget(table_widget(&t), chunks[1], &mut st);
    ui.panes = Some(Panes {
        summary: chunks[0],
        table: chunks[1],
        offset: st.offset(),
        divider: Some(chunks[0].y + chunks[0].height - 1),
    });
    f.render_widget(
        footer(
            ui,
            if d.account.kind == "osd" {
                "g per-PG pglog (cluster query)"
            } else {
                ""
            },
        ),
        chunks[2],
    );
    if let Some((name, res)) = &ui.pglog {
        if name == &d.account.name {
            draw_pglog(f, area, name, res);
        }
    }
}

fn draw_pglog(
    f: &mut Frame,
    area: Rect,
    name: &str,
    res: &Result<Vec<crate::pglog::PgLog>, String>,
) {
    let w = area.width.min(70);
    let h = area.height.min(30);
    let r = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(ratatui::widgets::Clear, r);
    let block = Block::bordered().title(format!(
        "{}: PG logs from `ceph pg dump` (Esc closes)",
        name
    ));
    match res {
        Err(e) => f.render_widget(
            Paragraph::new(format!("unavailable: {}", e))
                .block(block)
                .wrap(Wrap { trim: false }),
            r,
        ),
        Ok(pgs) => {
            let total_e: u64 = pgs.iter().map(|p| p.log_size).sum();
            let total_d: u64 = pgs.iter().map(|p| p.dups).sum();
            let header = Row::new(vec!["pgid", "entries", "dups", "on disk", "role"])
                .style(Style::default().add_modifier(Modifier::BOLD));
            let mut rows: Vec<Row> = vec![Row::new(vec![
                "all".to_string(),
                total_e.to_string(),
                total_d.to_string(),
                String::new(),
                format!("{} PGs", pgs.len()),
            ])
            .style(Style::default().add_modifier(Modifier::BOLD))];
            rows.extend(pgs.iter().map(|p| {
                Row::new(vec![
                    p.pgid.clone(),
                    p.log_size.to_string(),
                    p.dups.to_string(),
                    p.ondisk_log_size.to_string(),
                    if p.primary {
                        "primary".into()
                    } else {
                        "replica".into()
                    },
                ])
            }));
            let t = RTable::new(
                rows,
                [
                    Constraint::Length(12),
                    Constraint::Length(9),
                    Constraint::Length(9),
                    Constraint::Length(9),
                    Constraint::Length(10),
                ],
            )
            .header(header)
            .block(block);
            f.render_widget(t, r);
        }
    }
}

fn draw_help(f: &mut Frame, area: Rect) {
    let text = "ceph-memtop: where is this node's memory?\n\n\
        node view: rows add up to MemTotal. Ceph daemons are charged their anonymous memory; their file-backed\n\
        pages are in the page cache row. 'ref' rows overlap others and are not summed; '?' rows are residuals.\n\
        daemon view: rows add up to the daemon's RSS. Caches are named by the budget they charge; the rest by source.\n\
        cyan rows are the four caches the tuner sizes (data, meta, kv, kv_onode) and the tree rows made only of them.\n\n\
        peak:seen   = the largest value this tool has sampled since start/reset/restart; a spike between samples is missed\n\
        peak:kernel = VmHWM / memory.peak, recorded by the kernel; correct regardless of when the tool started\n\
        peaks never add up: the parts peak at different moments. Only current values reconcile.\n\n\
        keys: j/k or arrows move, Enter opens a daemon, Esc goes back, r resets the selected row's seen peak,\n\
        R resets all seen peaks, p toggles peak columns, x shows raw source keys, t tree/flat rows, g queries per-PG pglog (OSD), q quits.\n\
        mouse: a click selects a row, the wheel scrolls, dragging the border between a daemon's summary and its rows\n\
        resizes them (+/- do the same).";
    let w = area.width.min(110);
    let h = area.height.min(20);
    let r = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(ratatui::widgets::Clear, r);
    f.render_widget(
        Paragraph::new(text)
            .block(Block::bordered().title("help (any key closes)"))
            .wrap(Wrap { trim: false }),
        r,
    );
}
