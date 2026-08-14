//! TUI application state and event loop.
//!
//! The loop shape, which is the part worth deciding now:
//!
//! ```text
//! loop {
//!     poll for input, with a timeout of (next refresh - now)
//!     if input arrived    -> handle it, redraw
//!     if timeout elapsed  -> collect a snapshot, update rates and history, redraw
//! }
//! ```
//!
//! Polling with a computed timeout rather than sleeping a fixed interval means
//! keypresses feel instant while collection still happens on a steady cadence.
//! Sleeping the full interval and checking input afterwards makes the UI feel
//! laggy at a 2 s refresh; a busy-poll burns the CPU the tool is measuring.
//! The loop itself lives in [`crate::ui::run_loop`], since it also needs the
//! `ratatui`/`crossterm` types this module deliberately stays free of — see
//! [`KeyPress`]'s own doc for why.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::delta::{RateTracker, Rates};
use crate::error::Result;
use crate::sample::{ConnProtocol, Connection, Snapshot};
use crate::sysfs::{sanitize_kernel_string, SysfsReader};

/// Cap on the hostname/kernel-release strings read once at startup. Both are
/// always short in practice; this is the same defensive cap every other
/// kernel-supplied string in this crate gets before display.
const MAX_IDENTITY_LEN: usize = 256;

/// Which panel has focus, for scrolling and detail views.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Panel {
    #[default]
    Overview,
    Cpu,
    Memory,
    Thermal,
    Disk,
    Net,
    Gpu,
    Connections,
}

impl Panel {
    const ALL: [Panel; 8] = [
        Panel::Overview,
        Panel::Cpu,
        Panel::Memory,
        Panel::Thermal,
        Panel::Disk,
        Panel::Net,
        Panel::Gpu,
        Panel::Connections,
    ];

    fn index(self) -> usize {
        Self::ALL.iter().position(|p| *p == self).unwrap_or(0)
    }

    pub fn next(self) -> Panel {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Panel {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    /// `'1'..='7'` map onto the seven data panels in the order they're
    /// listed in `ALL` (skipping `Overview`, which has no digit of its own
    /// — it's reached only by cycling with `Tab`/`Shift-Tab`). `7` was
    /// appended for `Connections` rather than renumbering `1`-`6`, so every
    /// existing binding stays exactly as it was. Any other character is
    /// `None`.
    pub fn from_digit(c: char) -> Option<Panel> {
        match c {
            '1' => Some(Panel::Cpu),
            '2' => Some(Panel::Memory),
            '3' => Some(Panel::Thermal),
            '4' => Some(Panel::Disk),
            '5' => Some(Panel::Net),
            '6' => Some(Panel::Gpu),
            '7' => Some(Panel::Connections),
            _ => None,
        }
    }
}

/// One connection's row identity, for cursor/checkbox tracking in the
/// connections panel — stable across refreshes as long as the underlying
/// connection stays open. Deliberately *not* the whole [`Connection`]
/// (which also carries `state`/`uid`/`pid`/`program`, none of which affect
/// what row this is).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnKey {
    pub protocol: ConnProtocol,
    pub local_addr: IpAddr,
    pub local_port: u16,
    pub remote_addr: IpAddr,
    pub remote_port: u16,
}

impl ConnKey {
    pub fn of(c: &Connection) -> Self {
        ConnKey {
            protocol: c.protocol,
            local_addr: c.local_addr,
            local_port: c.local_port,
            remote_addr: c.remote_addr,
            remote_port: c.remote_port,
        }
    }
}

/// One visible row in the connections panel's process tree — either a
/// process's own header (grouping its direct connections and any nested
/// subprocesses) or one actual connection, at some indentation `depth`.
/// `checked`/`collapsed` are computed once, here, rather than re-derived by
/// [`crate::ui::widgets::draw_connections`] — that function only renders
/// what it's given, matching this crate's existing "collectors resolve,
/// widgets draw" split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnRow {
    Process {
        pid: u32,
        program: Option<String>,
        depth: usize,
        collapsed: bool,
        /// `true` only if every connection in this process's entire
        /// subtree (its own connections plus every descendant process's)
        /// is checked — an all-or-nothing summary, not a tri-state.
        checked: bool,
        own_connections: usize,
        subprocesses: usize,
    },
    Connection {
        /// Index into the current snapshot's `connections.connections`.
        index: usize,
        depth: usize,
        checked: bool,
    },
}

impl ConnRow {
    pub fn depth(&self) -> usize {
        match self {
            ConnRow::Process { depth, .. } | ConnRow::Connection { depth, .. } => *depth,
        }
    }
}

/// One process in the connections-panel tree: its own directly-owned
/// connections (by index into the flat connection list) plus any nested
/// subprocess nodes. Built fresh from the current snapshot each time it's
/// needed (see [`App::connections_rows`]) rather than cached — cheap enough
/// (at most a few hundred connections, a couple dozen distinct pids) that
/// caching would be premature.
struct ProcNode {
    pid: u32,
    program: Option<String>,
    own_indices: Vec<usize>,
    children: Vec<ProcNode>,
}

/// Group `connections` into a forest of [`ProcNode`]s plus a flat list of
/// indices for connections with no attributed `pid` at all (shown
/// ungrouped, as today).
///
/// A pid is a root if its `ppid` is `None` or isn't itself a pid present in
/// this connection set — this crate's connections collector only walks
/// `/proc/<pid>` for processes that themselves own a filtered connection
/// (see `collectors::connections`' own doc for why), so an
/// internet-connected process whose parent isn't *also* internet-connected
/// has no real ancestor to nest under here; it becomes its own root rather
/// than nesting under a synthetic placeholder.
///
/// Cycle-safe by construction even though a real OS process tree can never
/// actually cycle: nothing guarantees the two separate `/proc` reads behind
/// `pid` and `ppid` are perfectly consistent with each other (a pid could
/// be reused by a new, unrelated process between them). A `visited` set
/// ensures no pid is ever descended into twice; any pid a plain root-down
/// walk never reaches (only possible if two pids' `ppid`s point at each
/// other) is appended as its own extra root afterward, so a cycle can
/// neither hang this function nor silently drop connections from the view
/// — it just gets broken at an arbitrary point.
fn build_forest(connections: &[Connection]) -> (Vec<ProcNode>, Vec<usize>) {
    let mut by_pid: HashMap<u32, Vec<usize>> = HashMap::new();
    let mut ppid_of: HashMap<u32, Option<u32>> = HashMap::new();
    let mut program_of: HashMap<u32, Option<String>> = HashMap::new();
    let mut unattributed: Vec<usize> = Vec::new();

    for (index, c) in connections.iter().enumerate() {
        match c.pid {
            Some(pid) => {
                by_pid.entry(pid).or_default().push(index);
                ppid_of.entry(pid).or_insert(c.ppid);
                program_of.entry(pid).or_insert_with(|| c.program.clone());
            }
            None => unattributed.push(index),
        }
    }

    let mut all_pids: Vec<u32> = by_pid.keys().copied().collect();
    all_pids.sort_unstable();

    let mut children_of: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut roots: Vec<u32> = Vec::new();
    for &pid in &all_pids {
        match ppid_of.get(&pid).copied().flatten() {
            Some(ppid) if by_pid.contains_key(&ppid) => children_of.entry(ppid).or_default().push(pid),
            _ => roots.push(pid),
        }
    }
    for kids in children_of.values_mut() {
        kids.sort_unstable();
    }

    fn build_node(
        pid: u32,
        by_pid: &HashMap<u32, Vec<usize>>,
        program_of: &HashMap<u32, Option<String>>,
        children_of: &HashMap<u32, Vec<u32>>,
        visited: &mut HashSet<u32>,
    ) -> ProcNode {
        visited.insert(pid);
        let own_indices = by_pid.get(&pid).cloned().unwrap_or_default();
        let program = program_of.get(&pid).cloned().flatten();
        let mut children = Vec::new();
        if let Some(child_pids) = children_of.get(&pid) {
            for &child in child_pids {
                if !visited.contains(&child) {
                    children.push(build_node(child, by_pid, program_of, children_of, visited));
                }
            }
        }
        ProcNode { pid, program, own_indices, children }
    }

    let mut visited: HashSet<u32> = HashSet::new();
    let mut result: Vec<ProcNode> = Vec::new();
    for &pid in &roots {
        if !visited.contains(&pid) {
            result.push(build_node(pid, &by_pid, &program_of, &children_of, &mut visited));
        }
    }
    // Cycle fallback: any pid a normal root-down walk never reached.
    for &pid in &all_pids {
        if !visited.contains(&pid) {
            result.push(build_node(pid, &by_pid, &program_of, &children_of, &mut visited));
        }
    }

    (result, unattributed)
}

/// Every connection index in `node`'s subtree — its own plus every nested
/// child process's, regardless of that child's collapse state. Used for
/// bulk-checking a whole process group at once (see [`App::toggle_checked`]):
/// collapsing is purely visual, so it must never shrink what a bulk check
/// reaches.
fn subtree_indices(node: &ProcNode, out: &mut Vec<usize>) {
    out.extend_from_slice(&node.own_indices);
    for child in &node.children {
        subtree_indices(child, out);
    }
}

fn find_node(nodes: &[ProcNode], pid: u32) -> Option<&ProcNode> {
    for node in nodes {
        if node.pid == pid {
            return Some(node);
        }
        if let Some(found) = find_node(&node.children, pid) {
            return Some(found);
        }
    }
    None
}

/// Flatten a forest into the display/cursor row list, skipping a
/// collapsed process's entire subtree (that's what makes collapsing
/// actually shrink the visible list).
fn flatten_forest(
    nodes: &[ProcNode],
    depth: usize,
    collapsed: &HashSet<u32>,
    connections: &[Connection],
    checked: &HashSet<ConnKey>,
    out: &mut Vec<ConnRow>,
) {
    for node in nodes {
        let mut subtree = Vec::new();
        subtree_indices(node, &mut subtree);
        let all_checked = !subtree.is_empty()
            && subtree
                .iter()
                .all(|&i| connections.get(i).is_some_and(|c| checked.contains(&ConnKey::of(c))));
        let is_collapsed = collapsed.contains(&node.pid);

        out.push(ConnRow::Process {
            pid: node.pid,
            program: node.program.clone(),
            depth,
            collapsed: is_collapsed,
            checked: all_checked,
            own_connections: node.own_indices.len(),
            subprocesses: node.children.len(),
        });

        if !is_collapsed {
            for &index in &node.own_indices {
                let row_checked = connections.get(index).is_some_and(|c| checked.contains(&ConnKey::of(c)));
                out.push(ConnRow::Connection { index, depth: depth + 1, checked: row_checked });
            }
            flatten_forest(&node.children, depth + 1, collapsed, connections, checked, out);
        }
    }
}

/// Resolution state for one field of one remote IP's enrichment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum EnrichState<T> {
    #[default]
    NotRequested,
    Pending,
    Done(T),
    Failed,
}

/// What's known about one remote IP beyond the raw connection data —
/// resolved lazily, on request (see [`App::resolve_checked`]), and cached
/// here for the process's lifetime. Keyed by IP, not by connection: this is
/// a property of the remote address, so resolving it once for one
/// connection resolves it for every other connection sharing that address
/// too, and the result outlives any one connection closing.
#[derive(Debug, Clone, Default)]
pub struct Enrichment {
    pub domains: EnrichState<Vec<String>>,
    /// Route to this IP, one entry per hop (`None` = a silent hop — no
    /// reply within its timeout, shown as `*`). IPv4 only — see
    /// `net_probe::traceroute`'s own module doc for why — so this stays
    /// `NotRequested` forever for an IPv6 remote address; `resolve_checked`
    /// never even attempts to populate it in that case. Behind its own
    /// feature (separate from `tui`) since `traceroute` is a distinct
    /// crate-checklist-gated capability — `tui` without `traceroute` must
    /// stay a valid, useful build (DNS resolution alone, no route tracing).
    #[cfg(feature = "traceroute")]
    pub route: EnrichState<Vec<crate::net_probe::traceroute::Hop>>,
}

/// A background lookup's result, reported back over [`App`]'s enrichment
/// channel. `None` inside `Domains`/`Route` means the lookup completed but
/// found nothing (or failed) — see [`crate::net_probe::dns::resolve_ptr`]'s
/// and [`crate::net_probe::traceroute::trace`]'s own docs for the exact
/// distinction each draws.
enum EnrichmentUpdate {
    Domains(IpAddr, Option<Vec<String>>),
    #[cfg(feature = "traceroute")]
    Route(IpAddr, Option<Vec<crate::net_probe::traceroute::Hop>>),
}

/// Everything the UI needs between frames.
pub struct App {
    pub config: Config,
    pub registry: crate::collector::Registry,
    pub tracker: RateTracker,

    /// Most recent snapshot.
    pub current: Option<Snapshot>,
    /// Most recent rates, `None` on the first refresh or a dropped interval.
    pub rates: Option<Rates>,

    /// Fixed-capacity history for sparklines. A `VecDeque` with an explicit cap
    /// rather than a growing `Vec` — memory must stay bounded no matter how
    /// long the tool runs (security model item 8).
    pub history: VecDeque<Snapshot>,

    pub focus: Panel,
    pub paused: bool,
    pub show_help: bool,
    pub should_quit: bool,

    /// Machine identity, read once via the same [`SysfsReader`] every
    /// collector uses (`proc/sys/kernel/hostname`/`osrelease`), not
    /// refreshed every cycle — this is static machine identity, not a
    /// per-refresh reading, so it doesn't belong on [`Snapshot`]. Empty
    /// string if unreadable (masked `/proc`, permission denied); the header
    /// widget falls back to the crate name rather than showing a blank.
    pub hostname: String,
    pub kernel: String,

    /// Which row is highlighted in the connections panel.
    pub connections_cursor: usize,
    /// Which rows are checkbox-marked for enrichment, keyed by row
    /// identity rather than index — see [`ConnKey`].
    pub connections_checked: HashSet<ConnKey>,
    /// Which process-tree nodes are collapsed, keyed by pid — a pid not in
    /// this set is expanded (the default), so the tree starts fully open.
    pub connections_collapsed: HashSet<u32>,
    /// DNS (and, once traceroute lands, route) results, keyed by remote IP.
    pub enrichment: HashMap<IpAddr, Enrichment>,
    enrichment_tx: mpsc::Sender<EnrichmentUpdate>,
    enrichment_rx: mpsc::Receiver<EnrichmentUpdate>,

    last_refresh: Instant,
}

impl App {
    pub fn new(config: Config) -> Result<Self> {
        let reader = SysfsReader::with_root(config.sysfs_root.clone())?;
        let hostname = read_identity_field(&reader, "proc/sys/kernel/hostname");
        let kernel = read_identity_field(&reader, "proc/sys/kernel/osrelease");
        let registry = crate::collector::Registry::from_config(&config, reader)?;

        // Backdated so the very first `time_until_refresh()` call reports a
        // refresh as already due — the live TUI's first frame should show
        // real data immediately, not an interval's worth of blank panels.
        let last_refresh = Instant::now()
            .checked_sub(config.interval)
            .unwrap_or_else(Instant::now);

        let (enrichment_tx, enrichment_rx) = mpsc::channel();

        Ok(App {
            config,
            registry,
            tracker: RateTracker::new(),
            current: None,
            rates: None,
            history: VecDeque::new(),
            focus: Panel::default(),
            paused: false,
            show_help: false,
            should_quit: false,
            hostname,
            kernel,
            connections_cursor: 0,
            connections_checked: HashSet::new(),
            connections_collapsed: HashSet::new(),
            enrichment: HashMap::new(),
            enrichment_tx,
            enrichment_rx,
            last_refresh,
        })
    }

    /// One collection cycle: collect, update rates, push to history evicting
    /// the oldest past `config.history_len`.
    pub fn refresh(&mut self) -> Result<()> {
        let snapshot = self.registry.collect_all()?;
        self.rates = self.tracker.update(&snapshot);

        self.history.push_back(snapshot.clone());
        while self.history.len() > self.config.history_len {
            self.history.pop_front();
        }
        // `widgets::draw_cpu` wants a contiguous `&[Snapshot]`; doing the
        // rotation once here (only when the buffer actually changed) is
        // cheaper than a fresh `Vec` copy on every single frame draw.
        self.history.make_contiguous();

        self.current = Some(snapshot);
        self.clamp_connections_cursor();
        self.last_refresh = Instant::now();
        Ok(())
    }

    /// The connection list is ephemeral (sockets open and close every
    /// refresh), and collapsing/expanding a process group changes the
    /// visible row count without a new snapshot — either can leave the
    /// cursor past the end of a shorter row list, so this clamps it. Called
    /// from [`Self::refresh`] and from [`Self::collapse_cursor_row`]/
    /// [`Self::expand_cursor_row`] rather than at every read site.
    fn clamp_connections_cursor(&mut self) {
        let len = self.connections_rows().len();
        self.connections_cursor = if len == 0 { 0 } else { self.connections_cursor.min(len - 1) };
    }

    /// Handle one key press.
    ///
    /// Bindings: `q`/`Esc` quit, `Tab`/`Shift-Tab` cycle panels, `1`-`7` jump
    /// to a panel, `space` pause, `r` reset the rate tracker, `?` help,
    /// `+`/`-` adjust the interval (clamped to [`crate::config::MIN_INTERVAL`]).
    /// With the connections panel focused: `Up`/`Down` move the row cursor,
    /// `x` toggles the cursor row's checkbox (a process row toggles every
    /// connection in its whole subtree at once), `Left`/`Right`
    /// collapse/expand the cursor row's process group, `Enter` resolves
    /// every checked row's remote IP (see [`Self::resolve_checked`]).
    pub fn on_key(&mut self, key: KeyPress) -> Result<()> {
        match key {
            KeyPress::CtrlC | KeyPress::Esc => self.should_quit = true,
            KeyPress::Tab => self.focus = self.focus.next(),
            KeyPress::BackTab => self.focus = self.focus.prev(),
            KeyPress::Up if self.focus == Panel::Connections => self.move_connections_cursor(-1),
            KeyPress::Down if self.focus == Panel::Connections => self.move_connections_cursor(1),
            KeyPress::Left if self.focus == Panel::Connections => self.collapse_cursor_row(),
            KeyPress::Right if self.focus == Panel::Connections => self.expand_cursor_row(),
            KeyPress::Enter if self.focus == Panel::Connections => self.resolve_checked(),
            KeyPress::Char(c) => match c {
                'q' => self.should_quit = true,
                '?' => self.show_help = !self.show_help,
                ' ' => self.paused = !self.paused,
                'r' => self.tracker.reset(),
                'x' if self.focus == Panel::Connections => self.toggle_checked(),
                '+' | '=' => {
                    self.config.interval = self.config.interval.saturating_add(Duration::from_millis(250));
                }
                '-' => {
                    self.config.interval = self
                        .config
                        .interval
                        .saturating_sub(Duration::from_millis(250))
                        .max(crate::config::MIN_INTERVAL);
                }
                '1'..='7' => {
                    if let Some(panel) = Panel::from_digit(c) {
                        self.focus = panel;
                    }
                }
                _ => {}
            },
            _ => {}
        }
        Ok(())
    }

    /// Move the connections-panel row cursor by `delta`, wrapping — a no-op
    /// if there's no connection data (or none) to move a cursor over.
    fn move_connections_cursor(&mut self, delta: isize) {
        let len = self.connections_rows().len();
        if len == 0 {
            self.connections_cursor = 0;
            return;
        }
        let next = (self.connections_cursor as isize + delta).rem_euclid(len as isize);
        self.connections_cursor = next as usize;
    }

    /// Toggle the cursor row's checkbox. On a [`ConnRow::Connection`] this
    /// toggles just that one connection, same as always. On a
    /// [`ConnRow::Process`] this is a bulk, all-or-nothing toggle over its
    /// *entire* subtree (own connections plus every nested subprocess's,
    /// regardless of their own collapse state — collapsing only affects
    /// what's drawn, never what a bulk check reaches): if every connection
    /// in the subtree is already checked, uncheck them all; otherwise check
    /// them all.
    fn toggle_checked(&mut self) {
        let rows = self.connections_rows();
        let Some(row) = rows.get(self.connections_cursor) else { return };
        let Some(sample) = self.current.as_ref().and_then(|s| s.connections.as_ref()) else { return };

        match row {
            ConnRow::Connection { index, .. } => {
                let Some(conn) = sample.connections.get(*index) else { return };
                let key = ConnKey::of(conn);
                if !self.connections_checked.remove(&key) {
                    self.connections_checked.insert(key);
                }
            }
            ConnRow::Process { pid, .. } => {
                let (roots, _) = build_forest(&sample.connections);
                let Some(node) = find_node(&roots, *pid) else { return };
                let mut indices = Vec::new();
                subtree_indices(node, &mut indices);
                let keys: Vec<ConnKey> =
                    indices.iter().filter_map(|&i| sample.connections.get(i)).map(ConnKey::of).collect();
                let all_checked = !keys.is_empty() && keys.iter().all(|k| self.connections_checked.contains(k));
                for k in keys {
                    if all_checked {
                        self.connections_checked.remove(&k);
                    } else {
                        self.connections_checked.insert(k);
                    }
                }
            }
        }
    }

    /// Collapse the cursor row's process group — a no-op on a `Connection`
    /// row or a row that isn't currently the cursor's.
    fn collapse_cursor_row(&mut self) {
        if let Some(ConnRow::Process { pid, .. }) = self.connections_rows().get(self.connections_cursor) {
            self.connections_collapsed.insert(*pid);
        }
        self.clamp_connections_cursor();
    }

    /// Expand the cursor row's process group.
    fn expand_cursor_row(&mut self) {
        if let Some(ConnRow::Process { pid, .. }) = self.connections_rows().get(self.connections_cursor) {
            self.connections_collapsed.remove(pid);
        }
        self.clamp_connections_cursor();
    }

    /// The connections panel's current process tree, flattened into display
    /// rows — see [`ConnRow`], [`build_forest`], and [`flatten_forest`].
    /// Rebuilt on demand rather than cached: at most a few hundred
    /// connections and a couple dozen distinct pids, cheap enough that
    /// caching would be premature, and this keeps the row list, cursor
    /// position, and checkbox/collapse state trivially always in sync.
    pub fn connections_rows(&self) -> Vec<ConnRow> {
        let Some(sample) = self.current.as_ref().and_then(|s| s.connections.as_ref()) else {
            return Vec::new();
        };

        let (roots, unattributed) = build_forest(&sample.connections);
        let mut rows = Vec::new();
        flatten_forest(&roots, 0, &self.connections_collapsed, &sample.connections, &self.connections_checked, &mut rows);
        for index in unattributed {
            let checked = sample
                .connections
                .get(index)
                .is_some_and(|c| self.connections_checked.contains(&ConnKey::of(c)));
            rows.push(ConnRow::Connection { index, depth: 0, checked });
        }
        rows
    }

    /// Kick off DNS resolution for every checked row's remote IP that isn't
    /// already `Pending`/`Done` — one background thread per newly-triggered
    /// IP, the same thread-spawn-with-a-channel shape
    /// `collectors::disk::read_capacity` uses for a single bounded wait,
    /// generalised here to one bounded wait per triggered lookup. Results
    /// are collected later by [`Self::drain_enrichment`], never blocking
    /// this call or the event loop that calls it.
    pub fn resolve_checked(&mut self) {
        let Some(sample) = self.current.as_ref().and_then(|s| s.connections.as_ref()) else {
            return;
        };

        let targets: Vec<IpAddr> = sample
            .connections
            .iter()
            .filter(|c| self.connections_checked.contains(&ConnKey::of(c)))
            .map(|c| c.remote_addr)
            .collect();

        for addr in targets {
            // Skip anything already in flight or already resolved; a
            // `NotRequested`/`Failed`/absent entry is fair game (a
            // `Failed` lookup is deliberately retriable — pressing `Enter`
            // again on a still-checked row is the retry mechanism, there's
            // no separate "retry" key).
            let in_flight_or_done = matches!(
                self.enrichment.get(&addr).map(|e| &e.domains),
                Some(EnrichState::Pending) | Some(EnrichState::Done(_))
            );
            if in_flight_or_done {
                continue;
            }

            self.enrichment.entry(addr).or_default().domains = EnrichState::Pending;

            // A fresh reader, not a shared one: `SysfsReader` is `Clone`
            // (cheap — a `PathBuf` and a few `usize`s) precisely so a
            // background thread can own one outright rather than needing
            // `Arc`/`Mutex` around the one `App`/`Registry` already holds.
            let Ok(reader) = SysfsReader::with_root(self.config.sysfs_root.clone()) else {
                continue;
            };
            let tx = self.enrichment_tx.clone();
            thread::spawn(move || {
                let result = crate::net_probe::dns::resolve_ptr(&reader, addr);
                let _ = tx.send(EnrichmentUpdate::Domains(addr, result));
            });

            #[cfg(feature = "traceroute")]
            self.spawn_trace(addr);
        }
    }

    /// Kick off a traceroute for `addr` on its own background thread —
    /// IPv4 only (see [`Enrichment::route`]'s own doc), and only if one
    /// isn't already in flight or done for this IP. Split out from
    /// [`Self::resolve_checked`] so the `#[cfg(feature = "traceroute")]`
    /// gate stays in one place rather than wrapping half of that method's
    /// body.
    #[cfg(feature = "traceroute")]
    fn spawn_trace(&mut self, addr: IpAddr) {
        let IpAddr::V4(v4) = addr else { return };

        let in_flight_or_done = matches!(
            self.enrichment.get(&addr).map(|e| &e.route),
            Some(EnrichState::Pending) | Some(EnrichState::Done(_))
        );
        if in_flight_or_done {
            return;
        }

        self.enrichment.entry(addr).or_default().route = EnrichState::Pending;

        let tx = self.enrichment_tx.clone();
        thread::spawn(move || {
            let result = crate::net_probe::traceroute::trace(v4);
            let _ = tx.send(EnrichmentUpdate::Route(addr, result));
        });
    }

    /// Fold in any enrichment results that have arrived since the last
    /// call. Non-blocking (`try_recv`), cheap enough to call every
    /// event-loop iteration.
    pub fn drain_enrichment(&mut self) {
        while let Ok(update) = self.enrichment_rx.try_recv() {
            match update {
                EnrichmentUpdate::Domains(addr, result) => {
                    let entry = self.enrichment.entry(addr).or_default();
                    entry.domains = match result {
                        Some(domains) => EnrichState::Done(domains),
                        None => EnrichState::Failed,
                    };
                }
                #[cfg(feature = "traceroute")]
                EnrichmentUpdate::Route(addr, result) => {
                    let entry = self.enrichment.entry(addr).or_default();
                    entry.route = match result {
                        Some(hops) => EnrichState::Done(hops),
                        None => EnrichState::Failed,
                    };
                }
            }
        }
    }

    /// Handle a terminal resize.
    ///
    /// A deliberate no-op: [`crate::ui::layout::compute`] is called fresh
    /// from the live terminal size on every frame (`Frame::area()`), so a
    /// resize is already picked up by the very next draw with no state to
    /// update here. Kept as a real method (not deleted) to preserve the
    /// event-loop's dispatch shape documented in this module's own doc, and
    /// in case a later feature (e.g. persisting a user's manual column
    /// choice) needs to react to a resize explicitly.
    pub fn on_resize(&mut self, _width: u16, _height: u16) -> Result<()> {
        Ok(())
    }

    /// Time until the next scheduled refresh, for the poll timeout.
    /// `Duration::ZERO` when a refresh is due. When paused, returns a large
    /// duration instead — there is no scheduled refresh to wait for, so the
    /// caller's `poll` call ends up blocking on the next key press for all
    /// practical purposes, which is what "the caller blocks on input
    /// instead" (this method's original chunk-0 doc) means: paused doesn't
    /// special-case the caller's poll loop, it just makes a refresh never
    /// come due.
    pub fn time_until_refresh(&self) -> Duration {
        if self.paused {
            return Duration::from_secs(3600);
        }
        self.config.interval.saturating_sub(self.last_refresh.elapsed())
    }
}

fn read_identity_field(reader: &SysfsReader, path: &str) -> String {
    match reader.read_first_line(Path::new(path)) {
        Ok(Ok(line)) => sanitize_kernel_string(&line, MAX_IDENTITY_LEN),
        _ => String::new(),
    }
}

/// Backend-independent key event.
///
/// Deliberately not `crossterm::event::KeyEvent`: keeping the app's own type
/// means [`App::on_key`] is unit-testable with no terminal, and swapping the
/// backend later touches one conversion function instead of the whole event
/// handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPress {
    Char(char),
    Enter,
    Esc,
    Tab,
    BackTab,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    /// Ctrl-C, which must exit as cleanly as `q` — restoring the terminal, not
    /// dying mid-frame.
    CtrlC,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Panel --------------------------------------------------------------

    #[test]
    fn next_and_prev_cycle_through_every_panel_and_wrap() {
        let mut p = Panel::Overview;
        for _ in 0..Panel::ALL.len() {
            p = p.next();
        }
        assert_eq!(p, Panel::Overview, "a full cycle of next() must return to the start");

        let mut p = Panel::Overview;
        for _ in 0..Panel::ALL.len() {
            p = p.prev();
        }
        assert_eq!(p, Panel::Overview, "a full cycle of prev() must return to the start");
    }

    #[test]
    fn next_and_prev_are_inverses() {
        for &p in &Panel::ALL {
            assert_eq!(p.next().prev(), p);
            assert_eq!(p.prev().next(), p);
        }
    }

    #[test]
    fn from_digit_covers_one_through_seven_and_nothing_else() {
        assert_eq!(Panel::from_digit('1'), Some(Panel::Cpu));
        assert_eq!(Panel::from_digit('2'), Some(Panel::Memory));
        assert_eq!(Panel::from_digit('3'), Some(Panel::Thermal));
        assert_eq!(Panel::from_digit('4'), Some(Panel::Disk));
        assert_eq!(Panel::from_digit('5'), Some(Panel::Net));
        assert_eq!(Panel::from_digit('6'), Some(Panel::Gpu));
        assert_eq!(Panel::from_digit('7'), Some(Panel::Connections));
        assert_eq!(Panel::from_digit('0'), None);
        assert_eq!(Panel::from_digit('8'), None);
        assert_eq!(Panel::from_digit('a'), None);
    }

    // ---- App::new / refresh --------------------------------------------------

    fn test_config() -> Config {
        let mut c = Config::new();
        c.sysfs_root = std::env::temp_dir();
        c.only = vec!["cpu".to_string()];
        c.validate(crate::collectors::ALL).expect("temp dir is a valid sysfs_root");
        c
    }

    #[test]
    fn new_reads_identity_fields_without_erroring_even_when_absent() {
        // `std::env::temp_dir()` has no `proc/sys/kernel/hostname` under it,
        // so both fields must come back empty rather than making `App::new`
        // fail — identity is decoration, not something the whole UI should
        // refuse to start over.
        let app = App::new(test_config()).expect("App::new must not fail on missing identity files");
        assert_eq!(app.hostname, "");
        assert_eq!(app.kernel, "");
    }

    #[test]
    fn a_fresh_app_has_no_snapshot_yet() {
        let app = App::new(test_config()).expect("constructs");
        assert!(app.current.is_none());
        assert!(app.rates.is_none());
        assert!(app.history.is_empty());
    }

    #[test]
    fn time_until_refresh_is_zero_immediately_after_construction() {
        let app = App::new(test_config()).expect("constructs");
        // Backdated `last_refresh` means a refresh is already due.
        assert_eq!(app.time_until_refresh(), Duration::ZERO);
    }

    #[test]
    fn time_until_refresh_is_large_when_paused_regardless_of_the_interval() {
        let mut app = App::new(test_config()).expect("constructs");
        app.paused = true;
        assert!(app.time_until_refresh() > app.config.interval);
    }

    // ---- on_key ---------------------------------------------------------------

    #[test]
    fn q_and_esc_and_ctrl_c_all_request_quit() {
        for key in [KeyPress::Char('q'), KeyPress::Esc, KeyPress::CtrlC] {
            let mut app = App::new(test_config()).expect("constructs");
            app.on_key(key).expect("on_key must not fail");
            assert!(app.should_quit, "{key:?} did not set should_quit");
        }
    }

    #[test]
    fn space_toggles_paused() {
        let mut app = App::new(test_config()).expect("constructs");
        assert!(!app.paused);
        app.on_key(KeyPress::Char(' ')).unwrap();
        assert!(app.paused);
        app.on_key(KeyPress::Char(' ')).unwrap();
        assert!(!app.paused);
    }

    #[test]
    fn question_mark_toggles_help() {
        let mut app = App::new(test_config()).expect("constructs");
        assert!(!app.show_help);
        app.on_key(KeyPress::Char('?')).unwrap();
        assert!(app.show_help);
    }

    #[test]
    fn digit_keys_jump_focus_directly() {
        let mut app = App::new(test_config()).expect("constructs");
        app.on_key(KeyPress::Char('3')).unwrap();
        assert_eq!(app.focus, Panel::Thermal);
    }

    #[test]
    fn tab_and_backtab_cycle_focus() {
        let mut app = App::new(test_config()).expect("constructs");
        let start = app.focus;
        app.on_key(KeyPress::Tab).unwrap();
        assert_eq!(app.focus, start.next());
        app.on_key(KeyPress::BackTab).unwrap();
        assert_eq!(app.focus, start);
    }

    #[test]
    fn plus_and_minus_adjust_the_interval() {
        let mut app = App::new(test_config()).expect("constructs");
        let start = app.config.interval;
        app.on_key(KeyPress::Char('+')).unwrap();
        assert!(app.config.interval > start);
        app.on_key(KeyPress::Char('-')).unwrap();
        app.on_key(KeyPress::Char('-')).unwrap();
        assert!(app.config.interval >= crate::config::MIN_INTERVAL);
    }

    #[test]
    fn minus_never_drops_the_interval_below_the_configured_minimum() {
        let mut app = App::new(test_config()).expect("constructs");
        app.config.interval = crate::config::MIN_INTERVAL;
        for _ in 0..10 {
            app.on_key(KeyPress::Char('-')).unwrap();
        }
        assert_eq!(app.config.interval, crate::config::MIN_INTERVAL);
    }

    #[test]
    fn r_resets_the_rate_tracker() {
        let mut app = App::new(test_config()).expect("constructs");
        app.refresh().expect("first refresh");
        app.refresh().expect("second refresh");
        // Reset must not error and must be callable at any time, including
        // with no snapshots collected yet.
        app.on_key(KeyPress::Char('r')).expect("reset must not fail");
    }

    #[test]
    fn an_unbound_key_is_a_no_op() {
        let mut app = App::new(test_config()).expect("constructs");
        let focus = app.focus;
        app.on_key(KeyPress::Char('z')).unwrap();
        assert_eq!(app.focus, focus);
        assert!(!app.should_quit);
    }

    // ---- refresh + history bound ----------------------------------------------

    #[test]
    fn refresh_populates_current_and_bounds_history_to_history_len() {
        let mut config = test_config();
        config.history_len = 2;
        let mut app = App::new(config).expect("constructs");

        for _ in 0..5 {
            app.refresh().expect("refresh must not fail against a real sysfs_root");
        }

        assert!(app.current.is_some());
        assert!(app.history.len() <= 2, "history grew past history_len: {}", app.history.len());
    }

    // ---- connections panel: cursor, checkbox, enrichment -----------------------

    fn fake_connection(remote_port: u16) -> Connection {
        Connection {
            protocol: ConnProtocol::Tcp,
            local_addr: "10.0.0.1".parse().unwrap(),
            local_port: 5000,
            remote_addr: "8.8.8.8".parse().unwrap(),
            remote_port,
            state: Some(crate::sample::TcpState::Established),
            uid: 1000,
            pid: None,
            program: None,
            ppid: None,
        }
    }

    fn app_with_connections(conns: Vec<Connection>) -> App {
        let mut app = App::new(test_config()).expect("constructs");
        let mut snapshot = Snapshot::now();
        snapshot.connections = Some(crate::sample::ConnectionSample { connections: conns });
        app.current = Some(snapshot);
        app
    }

    fn fake_owned_connection(pid: u32, ppid: Option<u32>, program: &str, remote_port: u16) -> Connection {
        Connection { pid: Some(pid), ppid, program: Some(program.to_string()), ..fake_connection(remote_port) }
    }

    #[test]
    fn connections_cursor_wraps_in_both_directions() {
        let mut app = app_with_connections(vec![fake_connection(1), fake_connection(2), fake_connection(3)]);
        app.focus = Panel::Connections;

        app.on_key(KeyPress::Up).unwrap();
        assert_eq!(app.connections_cursor, 2, "moving up from 0 must wrap to the last row");
        app.on_key(KeyPress::Down).unwrap();
        assert_eq!(app.connections_cursor, 0);
        app.on_key(KeyPress::Down).unwrap();
        assert_eq!(app.connections_cursor, 1);
    }

    #[test]
    fn cursor_movement_is_ignored_outside_the_connections_panel() {
        let mut app = app_with_connections(vec![fake_connection(1), fake_connection(2)]);
        // focus stays at its default (Overview).
        app.on_key(KeyPress::Down).unwrap();
        assert_eq!(app.connections_cursor, 0, "Up/Down must be inert outside the connections panel");
    }

    #[test]
    fn x_toggles_the_cursor_rows_checkbox() {
        let mut app = app_with_connections(vec![fake_connection(1), fake_connection(2)]);
        app.focus = Panel::Connections;
        let key0 = ConnKey::of(&fake_connection(1));

        app.on_key(KeyPress::Char('x')).unwrap();
        assert!(app.connections_checked.contains(&key0));
        app.on_key(KeyPress::Char('x')).unwrap();
        assert!(!app.connections_checked.contains(&key0), "toggled again, must uncheck");
    }

    #[test]
    fn x_is_ignored_outside_the_connections_panel() {
        let mut app = app_with_connections(vec![fake_connection(1)]);
        app.on_key(KeyPress::Char('x')).unwrap();
        assert!(app.connections_checked.is_empty());
    }

    #[test]
    fn resolve_checked_with_nothing_checked_spawns_nothing() {
        let mut app = app_with_connections(vec![fake_connection(1)]);
        app.resolve_checked();
        assert!(app.enrichment.is_empty());
    }

    /// `resolve_checked` spawns a background thread per checked row's IP;
    /// `drain_enrichment` picks up its result once it lands. `test_config`
    /// points `sysfs_root` at a plain temp dir with no `etc/resolv.conf`,
    /// so the lookup fails immediately with no real network I/O — keeping
    /// this test fast and deterministic rather than depending on outside
    /// network access.
    #[test]
    fn resolve_checked_marks_pending_then_drains_to_a_final_state() {
        let mut app = app_with_connections(vec![fake_connection(1)]);
        app.focus = Panel::Connections;
        app.on_key(KeyPress::Char('x')).unwrap(); // check the only row

        app.resolve_checked();
        let addr: IpAddr = "8.8.8.8".parse().unwrap();
        assert_eq!(
            app.enrichment.get(&addr).map(|e| &e.domains),
            Some(&EnrichState::Pending)
        );

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            app.drain_enrichment();
            if !matches!(app.enrichment.get(&addr).map(|e| &e.domains), Some(EnrichState::Pending)) {
                break;
            }
            assert!(Instant::now() < deadline, "enrichment never resolved");
            std::thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(
            app.enrichment.get(&addr).map(|e| &e.domains),
            Some(&EnrichState::Failed),
            "no etc/resolv.conf under the test root, so this lookup must fail, not hang"
        );
    }

    #[test]
    fn resolve_checked_does_not_respawn_a_pending_or_done_lookup() {
        let mut app = app_with_connections(vec![fake_connection(1)]);
        let addr: IpAddr = "8.8.8.8".parse().unwrap();
        app.enrichment.entry(addr).or_default().domains = EnrichState::Done(vec!["example.com".to_string()]);
        app.focus = Panel::Connections;
        app.on_key(KeyPress::Char('x')).unwrap();

        app.resolve_checked();
        // Must still be exactly the `Done` value set above, not reset to
        // `Pending` by a redundant spawn.
        assert_eq!(
            app.enrichment.get(&addr).map(|e| &e.domains),
            Some(&EnrichState::Done(vec!["example.com".to_string()]))
        );
    }

    #[test]
    fn clamp_connections_cursor_pulls_the_cursor_back_when_the_list_shrinks() {
        let mut app = app_with_connections(vec![fake_connection(1), fake_connection(2), fake_connection(3)]);
        app.connections_cursor = 2;

        app.current.as_mut().unwrap().connections = Some(crate::sample::ConnectionSample {
            connections: vec![fake_connection(1)],
        });
        app.clamp_connections_cursor();
        assert_eq!(app.connections_cursor, 0);
    }

    #[test]
    fn clamp_connections_cursor_resets_to_zero_when_the_list_becomes_empty() {
        let mut app = app_with_connections(vec![fake_connection(1), fake_connection(2)]);
        app.connections_cursor = 1;

        app.current.as_mut().unwrap().connections = Some(crate::sample::ConnectionSample { connections: vec![] });
        app.clamp_connections_cursor();
        assert_eq!(app.connections_cursor, 0);
    }

    // ---- connections panel: process tree ---------------------------------------

    #[test]
    fn an_unattributed_connection_is_a_flat_top_level_row() {
        let app = app_with_connections(vec![fake_connection(1)]);
        let rows = app.connections_rows();
        assert_eq!(rows, vec![ConnRow::Connection { index: 0, depth: 0, checked: false }]);
    }

    #[test]
    fn a_pid_owned_connection_is_grouped_under_a_process_header() {
        let app = app_with_connections(vec![fake_owned_connection(42, None, "curl", 1)]);
        let rows = app.connections_rows();
        assert_eq!(
            rows,
            vec![
                ConnRow::Process {
                    pid: 42,
                    program: Some("curl".to_string()),
                    depth: 0,
                    collapsed: false,
                    checked: false,
                    own_connections: 1,
                    subprocesses: 0,
                },
                ConnRow::Connection { index: 0, depth: 1, checked: false },
            ]
        );
    }

    #[test]
    fn a_child_process_nests_under_its_real_parent() {
        // pid 100 is the parent (no ppid of its own here); pid 200's ppid
        // points at 100, and 100 owns a connection too — matching a real
        // Electron-style main-process-plus-renderer shape.
        let app = app_with_connections(vec![
            fake_owned_connection(100, None, "app", 1),
            fake_owned_connection(200, Some(100), "app", 2),
        ]);
        let rows = app.connections_rows();
        assert_eq!(rows.len(), 4, "{rows:?}");
        assert!(matches!(rows[0], ConnRow::Process { pid: 100, depth: 0, .. }), "{rows:?}");
        assert!(matches!(rows[1], ConnRow::Connection { index: 0, depth: 1, .. }), "{rows:?}");
        assert!(matches!(rows[2], ConnRow::Process { pid: 200, depth: 1, .. }), "{rows:?}");
        assert!(matches!(rows[3], ConnRow::Connection { index: 1, depth: 2, .. }), "{rows:?}");
    }

    #[test]
    fn a_process_whose_parent_owns_no_connection_becomes_its_own_root() {
        // pid 200's ppid (999) never owns a filtered connection itself, so
        // there's nothing real to nest 200 under — it's a root, not nested
        // under a synthetic placeholder.
        let app = app_with_connections(vec![fake_owned_connection(200, Some(999), "app", 1)]);
        let rows = app.connections_rows();
        assert!(matches!(rows[0], ConnRow::Process { pid: 200, depth: 0, .. }), "{rows:?}");
    }

    #[test]
    fn collapsing_a_process_hides_its_entire_subtree() {
        let mut app = app_with_connections(vec![
            fake_owned_connection(100, None, "app", 1),
            fake_owned_connection(200, Some(100), "app", 2),
        ]);
        app.focus = Panel::Connections;
        assert_eq!(app.connections_rows().len(), 4);

        app.connections_cursor = 0; // the pid-100 header
        app.on_key(KeyPress::Left).unwrap();
        let rows = app.connections_rows();
        assert_eq!(rows, vec![ConnRow::Process {
            pid: 100,
            program: Some("app".to_string()),
            depth: 0,
            collapsed: true,
            checked: false,
            own_connections: 1,
            subprocesses: 1,
        }]);
    }

    #[test]
    fn expanding_a_collapsed_process_restores_its_subtree() {
        let mut app = app_with_connections(vec![fake_owned_connection(42, None, "curl", 1)]);
        app.focus = Panel::Connections;
        app.connections_collapsed.insert(42);
        assert_eq!(app.connections_rows().len(), 1, "collapsed: header only");

        app.connections_cursor = 0;
        app.on_key(KeyPress::Right).unwrap();
        assert_eq!(app.connections_rows().len(), 2, "expanded: header + its connection");
    }

    #[test]
    fn left_and_right_are_no_ops_on_a_connection_row() {
        let mut app = app_with_connections(vec![fake_connection(1)]);
        app.focus = Panel::Connections;
        app.connections_cursor = 0; // the only row, a flat unattributed Connection
        app.on_key(KeyPress::Left).unwrap();
        app.on_key(KeyPress::Right).unwrap();
        assert!(app.connections_collapsed.is_empty());
    }

    #[test]
    fn checking_a_process_header_checks_its_entire_subtree() {
        let mut app = app_with_connections(vec![
            fake_owned_connection(100, None, "app", 1),
            fake_owned_connection(200, Some(100), "app", 2),
        ]);
        app.focus = Panel::Connections;
        app.connections_cursor = 0; // the pid-100 header, whose subtree includes pid 200

        app.on_key(KeyPress::Char('x')).unwrap();
        assert_eq!(app.connections_checked.len(), 2, "both connections in the subtree must be checked");

        // Toggling the (now fully-checked) header again must uncheck the
        // whole subtree, not check it further.
        app.on_key(KeyPress::Char('x')).unwrap();
        assert!(app.connections_checked.is_empty());
    }

    #[test]
    fn build_forest_is_cycle_safe() {
        // A ppid cycle can't happen in a real OS process tree, but nothing
        // guarantees the two separate `/proc` reads behind `pid`/`ppid`
        // stay consistent with each other (e.g. pid reuse mid-refresh) —
        // this must terminate and keep both connections visible rather
        // than hang or silently drop them.
        let app = app_with_connections(vec![
            fake_owned_connection(10, Some(20), "a", 1),
            fake_owned_connection(20, Some(10), "b", 2),
        ]);
        let rows = app.connections_rows();
        assert_eq!(rows.len(), 4, "2 process headers + 2 connections: {rows:?}");
        let seen_indices: HashSet<usize> = rows
            .iter()
            .filter_map(|r| match r {
                ConnRow::Connection { index, .. } => Some(*index),
                ConnRow::Process { .. } => None,
            })
            .collect();
        assert_eq!(seen_indices, HashSet::from([0, 1]), "every connection must still appear exactly once");
    }
}
