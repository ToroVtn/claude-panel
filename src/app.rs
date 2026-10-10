//! App state and behaviour: what's loaded, what's selected, what each key/click does.
//! Drawing lives in ui.rs; reading Claude Code's files lives in data.rs.

use crate::data::{self, Day, Limit, Mcp, Plugin, Seg, Skill};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

const REFRESH_DELAY: Duration = Duration::from_millis(300); // let a burst of file writes settle first
const LIMITS_MIN_INTERVAL: Duration = Duration::from_secs(30); // don't hammer the usage endpoint

/// Messages from background threads (file watcher, network, `claude` commands) to the UI loop.
pub enum Msg {
    Changed { transcripts: bool, config: bool },
    Limits(Result<Value, String>),
    Cli { catalog: bool, ok_msg: String, result: Result<String, String> },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Skills = 0,
    Plugins = 1,
    Mcp = 2,
}
pub const TABS: [(Tab, &str); 3] = [(Tab::Skills, "Skills"), (Tab::Plugins, "Plugins"), (Tab::Mcp, "MCP")];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Usage,
    Table,
}

/// One table's view state: filter, sort, cursor, scroll, and (Skills) the marked rows.
#[derive(Default)]
pub struct Grid {
    pub view: Vec<usize>, // indices into the tab's items, filtered + sorted
    pub cursor: usize,    // position in `view`
    pub offset: usize,    // first visible row
    pub marked: HashSet<String>, // ids (multi-select)
    pub anchor: Option<usize>,   // where a Shift-range starts
    pub filter: String,
    pub sort: usize,
    pub asc: bool,
}

pub struct Col {
    pub title: &'static str,
    pub width: ratatui::layout::Constraint,
    pub num: bool,
}

/// Sort key for a cell: numbers sort as numbers, everything else as lowercase text.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Num(u64),
    Text(String),
}

pub struct Available {
    pub id: String,
    pub name: String,
    pub marketplace: String,
    pub installs: u64,
    pub description: String,
    pub installed: bool,
}

pub enum Popup {
    Confirm { title: String, lines: Vec<String>, action: Action },
    Input { title: String, text: String },  // add skill: folder path
    Pick { title: String, items: Vec<String>, values: Vec<String>, cursor: usize, next: PickFor },
    Catalog { filter: String, cursor: usize },
}

pub enum Action {
    RemoveSkills(Vec<(String, PathBuf)>), // (name, folder)
    Uninstall { name: String, scope: String, project: String },
}

pub enum PickFor {
    SkillTarget(PathBuf),
    PluginScope(String),
    PluginProject(String, String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Projects,
    Sessions,
}

/// Screen areas from the last draw, so mouse clicks can be matched to what's under them.
#[derive(Default)]
pub struct Hits {
    pub usage: Rect,
    pub days: Rect,                    // the day rows (first row = days_offset)
    pub segs: Vec<Vec<(u16, u16)>>,    // per visible day: x range of each bar segment
    pub tabs: Vec<(Rect, Tab)>,
    pub table: Rect,                   // whole table box
    pub rows: Rect,                    // the data rows (first row = grid.offset)
    pub header: Vec<(u16, u16)>,       // x range of each column header
    pub details: Rect,
}

pub struct App {
    pub tx: Sender<Msg>,
    pub cfg: Value,
    pub skills: Vec<Skill>,
    pub plugins: Vec<Plugin>,
    pub mcp: Vec<Mcp>,
    pub projects: Vec<String>,
    pub available: Option<Vec<Available>>,
    pub grids: [Grid; 3],
    pub tab: Tab,
    pub focus: Focus,
    pub usage: data::Usage,
    pub days: Vec<Day>,
    pub legend: Vec<Seg>,
    pub group: Group,
    pub day: usize,
    pub seg: usize,
    pub days_offset: usize,
    pub limits: Vec<Limit>,
    pub limits_note: String, // where the numbers come from / why they may be old
    pub limits_live: bool,
    pub details_scroll: u16,
    pub popup: Option<Popup>,
    pub searching: bool,
    pub status: String,
    pub busy: bool,      // a `claude` command is running (one at a time)
    pub fetching: bool,  // plan limits request in flight
    pub quit: bool,
    pub hits: Hits,
    watcher: Option<RecommendedWatcher>,
    watched: HashSet<PathBuf>,
    pending: (bool, bool), // (transcripts, config) changed and not reloaded yet
    changed_at: Instant,
    limits_due: bool,
    limits_at: Option<Instant>,
    loading_catalog: bool,
}

pub fn columns(tab: Tab) -> Vec<Col> {
    use ratatui::layout::Constraint::{Fill, Length};
    let c = |title, width, num| Col { title, width, num };
    match tab {
        Tab::Skills => vec![c("Name", Fill(3), false), c("Scope", Length(8), false), c("Plugin", Length(16), false),
                            c("Source", Fill(2), false), c("Uses", Length(6), true), c("Last used", Length(9), false)],
        Tab::Plugins => vec![c("On", Length(5), false), c("Plugin", Fill(3), false), c("Skills", Length(8), true),
                             c("Uses", Length(6), true), c("Version", Length(14), false), c("Scope", Length(8), false),
                             c("Project", Fill(2), false)],
        Tab::Mcp => vec![c("Name", Length(14), false), c("Scope", Length(8), false), c("Defined in", Fill(2), false),
                         c("Transport", Length(9), false), c("Command / URL", Fill(3), false)],
    }
}

fn is_text_key(k: &KeyEvent) -> Option<char> {
    match k.code {
        KeyCode::Char(c) if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => Some(c),
        _ => None,
    }
}

/// The `claude` executable on PATH. ponytail: on Windows npm installs a claude.cmd, which Command can't
/// run directly; use `cmd /C claude` there if that's how it's installed.
fn claude_bin() -> PathBuf {
    let names: &[&str] = if cfg!(windows) { &["claude.exe", "claude.cmd"] } else { &["claude"] };
    std::env::var_os("PATH").into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("claude"))
}

pub fn open_folder(path: &Path) {
    let opener = if cfg!(windows) { "explorer" } else if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = Command::new(opener).arg(path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

impl App {
    pub fn new(tx: Sender<Msg>) -> Self {
        let mut app = App {
            tx, cfg: Value::Null, skills: vec![], plugins: vec![], mcp: vec![], projects: vec![], available: None,
            grids: Default::default(), tab: Tab::Skills, focus: Focus::Table,
            usage: Default::default(), days: vec![], legend: vec![], group: Group::Projects, day: 0, seg: 0, days_offset: 0,
            limits: vec![], limits_note: String::new(), limits_live: false, details_scroll: 0,
            popup: None, searching: false, status: String::new(), busy: false, fetching: false, quit: false,
            hits: Default::default(), watcher: None, watched: HashSet::new(), pending: (false, false),
            changed_at: Instant::now(), limits_due: true, limits_at: None, loading_catalog: false,
        };
        // default sorts: most used first (skills, plugins), by name (MCP)
        (app.grids[0].sort, app.grids[1].sort, app.grids[2].asc) = (4, 3, true);
        app.reload_config();
        app.reload_usage();
        app
    }

    // ---------- loading ----------

    pub fn reload_config(&mut self) {
        self.cfg = data::read_json(&data::home().join(".claude.json"));
        let settings = data::read_json(&data::claude_dir().join("settings.json"));
        self.plugins = data::load_plugins(&self.cfg, &settings);
        self.skills = data::load_skills(&self.cfg, &self.plugins);
        self.mcp = data::load_mcp(&self.cfg, &self.plugins);
        self.projects = data::known_projects(&self.cfg);
        let ids: HashSet<String> = self.skills.iter().map(|s| s.id.clone()).collect();
        self.grids[0].marked.retain(|id| ids.contains(id)); // removed skills can't stay marked
        if let Some(av) = &mut self.available {
            let installed: HashSet<&str> = self.plugins.iter().map(|p| p.name.as_str()).collect();
            av.iter_mut().for_each(|a| a.installed = installed.contains(a.id.as_str()));
        }
        if !self.limits_live {  // until a live fetch works, show what Claude Code cached
            let c = self.cfg.get("cachedUsageUtilization").cloned().unwrap_or_default();
            self.limits = data::limits_from(c.get("utilization").unwrap_or(&Value::Null));
            let at = c.get("fetchedAtMs").and_then(Value::as_f64).unwrap_or(0.0);
            self.limits_note = format!("cached by Claude Code {}", data::ago(at));
        }
        for t in [Tab::Skills, Tab::Plugins, Tab::Mcp] {
            self.rebuild(t);
        }
        self.sync_watches();
    }

    pub fn reload_usage(&mut self) {
        self.usage.update(&data::claude_dir().join("projects"));
        (self.days, self.legend) = data::daily_breakdown(&self.usage.by_day_session, &self.usage.sessions, 30, 8);
        self.day = self.day.min(self.days.len().saturating_sub(1));
        self.clamp_seg();
    }

    /// Everything that should trigger a refresh. Transcripts change on every Claude Code message and
    /// tool call; settings/plugin files when plugins or skills change. Watching them = refresh on
    /// anything you do, in any session, without hooking into Claude Code itself.
    pub fn start_watching(&mut self) {
        let tx = self.tx.clone();
        let (projects_dir, claude, home) = (data::claude_dir().join("projects"), data::claude_dir(), data::home());
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            if matches!(ev.kind, EventKind::Access(_)) {
                return;
            }
            let (mut transcripts, mut config) = (false, false);
            for p in &ev.paths {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if p.starts_with(&projects_dir) {
                    transcripts |= name.ends_with(".jsonl");
                } else if p.parent() == Some(home.as_path()) || p.parent() == Some(claude.as_path()) {
                    // watched non-recursively for single files: ignore the rest of ~ and ~/.claude
                    config |= matches!(name, ".claude.json" | "settings.json");
                } else {
                    config |= !name.ends_with(".tmp"); // skills, plugins, project .claude folders, .mcp.json
                }
            }
            if transcripts || config {
                let _ = tx.send(Msg::Changed { transcripts, config });
            }
        });
        let Ok(mut w) = watcher else {
            self.status = "Couldn't watch files for changes; press r to refresh.".into();
            return;
        };
        let _ = w.watch(&data::claude_dir().join("projects"), RecursiveMode::Recursive);
        let _ = w.watch(&data::claude_dir().join("skills"), RecursiveMode::Recursive);
        let _ = w.watch(&data::claude_dir().join("plugins"), RecursiveMode::NonRecursive);
        let _ = w.watch(&data::claude_dir(), RecursiveMode::NonRecursive);
        let _ = w.watch(&data::home(), RecursiveMode::NonRecursive); // for ~/.claude.json (rewritten via rename)
        self.watcher = Some(w);
        self.sync_watches();
    }

    /// Projects come and go: watch each one's .claude folder (settings, skills).
    fn sync_watches(&mut self) {
        let Some(w) = &mut self.watcher else { return };
        for p in &self.projects {
            let dir = Path::new(p).join(".claude");
            if dir.is_dir() && self.watched.insert(dir.clone()) {
                let _ = w.watch(&dir, RecursiveMode::Recursive);
            }
        }
    }

    pub fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Changed { transcripts, config } => {
                self.pending.0 |= transcripts;
                self.pending.1 |= config;
                self.changed_at = Instant::now();
            }
            Msg::Limits(r) => {
                self.fetching = false;
                self.limits_at = Some(Instant::now());
                match r {
                    Ok(util) => {
                        self.limits = data::limits_from(&util);
                        self.limits_live = true;
                        self.limits_note = format!("live {}", chrono::Local::now().format("%H:%M:%S"));
                    }
                    // keep the last numbers but say they may be old and why
                    Err(e) => self.limits_note = format!("{} · refresh failed: {e}", self.limits_note.split(" · ").next().unwrap_or("")),
                }
            }
            Msg::Cli { catalog, ok_msg, result } => {
                if catalog {
                    self.loading_catalog = false;
                } else {
                    self.busy = false;
                    self.reload_config(); // success or failure: show what's really on disk now
                }
                match result {
                    Err(e) => self.status = e,
                    Ok(out) if catalog => self.set_catalog(&out),
                    Ok(_) => self.status = ok_msg,
                }
            }
        }
    }

    /// Called every loop turn: debounced reloads and plan-limit fetches.
    pub fn tick(&mut self) -> bool {
        let mut changed = false;
        if (self.pending.0 || self.pending.1) && self.changed_at.elapsed() >= REFRESH_DELAY {
            if std::mem::take(&mut self.pending.1) {
                self.reload_config();
            }
            if std::mem::take(&mut self.pending.0) {
                self.reload_usage();
                self.limits_due = true; // new usage: the plan percentages moved too
            }
            changed = true;
        }
        let waited = self.limits_at.is_none_or(|t| t.elapsed() >= LIMITS_MIN_INTERVAL);
        if self.limits_due && waited && !self.fetching {
            self.fetch_limits();
            changed = true;
        }
        changed
    }

    fn fetch_limits(&mut self) {
        self.limits_due = false;
        self.fetching = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Limits(data::fetch_limits()));
        });
    }

    /// Run `claude <args>` in the background; the result comes back as Msg::Cli.
    fn cli(&mut self, args: Vec<String>, cwd: &str, ok_msg: String, catalog: bool) {
        if !catalog {
            if self.busy {
                self.status = "A claude command is still running; try again when it's done.".into();
                return;
            }
            self.busy = true;
            self.status = format!("Running: claude {}", args.join(" "));
        }
        let cwd = if cwd.is_empty() { data::home() } else { PathBuf::from(cwd) };
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = match Command::new(claude_bin()).args(&args).current_dir(cwd).stdin(Stdio::null()).output() {
                Err(e) => Err(format!("Couldn't start `claude` ({e}); is it on PATH?")),
                Ok(o) if !o.status.success() => {
                    // the CLI's own message is the most useful thing to show
                    let text = String::from_utf8_lossy(if o.stderr.is_empty() { &o.stdout } else { &o.stderr }).trim().to_string();
                    let tail: String = text.chars().rev().take(300).collect::<Vec<_>>().into_iter().rev().collect();
                    Err(if tail.is_empty() { format!("claude exited with {}", o.status) } else { tail })
                }
                Ok(o) => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
            };
            let _ = tx.send(Msg::Cli { catalog, ok_msg, result });
        });
    }

    fn set_catalog(&mut self, out: &str) {
        let Ok(v) = serde_json::from_str::<Value>(out) else {
            self.status = "Couldn't read the plugin catalog from the CLI.".into();
            return;
        };
        let installed: HashSet<&str> = self.plugins.iter().map(|p| p.name.as_str()).collect();
        let mut list: Vec<Available> = v.get("available").and_then(Value::as_array).into_iter().flatten().map(|p| Available {
            id: data::get_str(p, "pluginId").into(),
            name: data::get_str(p, "name").into(),
            marketplace: data::get_str(p, "marketplaceName").into(),
            installs: p.get("installCount").and_then(Value::as_u64).unwrap_or(0),
            description: data::get_str(p, "description").into(),
            installed: installed.contains(data::get_str(p, "pluginId")),
        }).collect();
        list.sort_by(|a, b| b.installs.cmp(&a.installs));
        self.available = Some(list);
    }

    // ---------- tables ----------

    pub fn len(&self, tab: Tab) -> usize {
        match tab {
            Tab::Skills => self.skills.len(),
            Tab::Plugins => self.plugins.len(),
            Tab::Mcp => self.mcp.len(),
        }
    }

    pub fn id(&self, tab: Tab, i: usize) -> &str {
        match tab {
            Tab::Skills => &self.skills[i].id,
            Tab::Plugins => &self.plugins[i].id,
            Tab::Mcp => &self.mcp[i].id,
        }
    }

    /// The text of each column for item i (also what the filter searches).
    pub fn texts(&self, tab: Tab, i: usize) -> Vec<String> {
        match tab {
            Tab::Skills => {
                let s = &self.skills[i];
                vec![s.name.clone(), s.scope.clone(), s.plugin_short().into(), s.source.clone(), s.uses.to_string(), data::ago(s.last)]
            }
            Tab::Plugins => {
                let p = &self.plugins[i];
                vec![(if p.enabled { "● on" } else { "○ off" }).into(), p.name.clone(), p.skills.len().to_string(),
                     p.uses.to_string(), p.version.clone(), p.scope.clone(), p.project.clone()]
            }
            Tab::Mcp => {
                let m = &self.mcp[i];
                vec![m.name.clone(), m.scope.clone(), m.place.clone(), m.kind.clone(), m.target.clone()]
            }
        }
    }

    fn key(&self, tab: Tab, i: usize, col: usize) -> Key {
        match (tab, col) {
            (Tab::Skills, 4) => Key::Num(self.skills[i].uses),
            (Tab::Skills, 5) => Key::Num(self.skills[i].last as u64),
            (Tab::Plugins, 0) => Key::Num(self.plugins[i].enabled as u64),
            (Tab::Plugins, 2) => Key::Num(self.plugins[i].skills.len() as u64),
            (Tab::Plugins, 3) => Key::Num(self.plugins[i].uses),
            _ => Key::Text(self.texts(tab, i).swap_remove(col).to_lowercase()),
        }
    }

    pub fn grid(&self) -> &Grid {
        &self.grids[self.tab as usize]
    }

    fn grid_mut(&mut self) -> &mut Grid {
        &mut self.grids[self.tab as usize]
    }

    /// Re-filter and re-sort a table, keeping the cursor on the same item if it's still there.
    pub fn rebuild(&mut self, tab: Tab) {
        let g = &self.grids[tab as usize];
        let keep = g.view.get(g.cursor).filter(|&&i| i < self.len(tab)).map(|&i| self.id(tab, i).to_string());
        let f = g.filter.to_lowercase();
        let (col, asc) = (g.sort, g.asc);
        let mut view: Vec<usize> = (0..self.len(tab))
            .filter(|&i| f.is_empty() || self.texts(tab, i).iter().any(|t| t.to_lowercase().contains(&f)))
            .collect();
        view.sort_by_cached_key(|&i| self.key(tab, i, col));
        if !asc {
            view.reverse();
        }
        let cursor = keep.and_then(|id| view.iter().position(|&i| self.id(tab, i) == id)).unwrap_or(0);
        let g = &mut self.grids[tab as usize];
        g.view = view;
        g.cursor = cursor.min(g.view.len().saturating_sub(1));
        g.anchor = None;
    }

    /// Index (into the tab's items) under the cursor.
    pub fn current(&self) -> Option<usize> {
        let g = self.grid();
        g.view.get(g.cursor).copied()
    }

    fn move_cursor(&mut self, delta: isize) {
        let g = self.grid_mut();
        if !g.view.is_empty() {
            g.cursor = (g.cursor as isize + delta).clamp(0, g.view.len() as isize - 1) as usize;
        }
        self.details_scroll = 0;
    }

    fn sort_by(&mut self, col: usize) {
        let num = columns(self.tab)[col].num || matches!((self.tab, col), (Tab::Skills, 5) | (Tab::Plugins, 0));
        let g = self.grid_mut();
        if g.sort == col {
            g.asc = !g.asc;
        } else {
            (g.sort, g.asc) = (col, !num); // numbers/dates: biggest first
        }
        self.rebuild(self.tab);
    }

    // ---------- multi-select (Skills) ----------

    fn mark_range(&mut self, to: usize, keep: bool) {
        let tab = self.tab;
        let g = &self.grids[tab as usize];
        let from = g.anchor.unwrap_or(g.cursor);
        let ids: Vec<String> = g.view[from.min(to)..=from.max(to)].iter().map(|&i| self.id(tab, i).to_string()).collect();
        let g = &mut self.grids[tab as usize];
        if !keep {
            g.marked.clear();
        }
        g.marked.extend(ids);
        g.anchor = Some(from);
    }

    fn toggle_mark(&mut self, pos: usize) {
        let Some(&i) = self.grid().view.get(pos) else { return };
        let id = self.id(self.tab, i).to_string();
        let g = self.grid_mut();
        if !g.marked.remove(&id) {
            g.marked.insert(id);
        }
        g.anchor = Some(pos);
    }

    /// What an action applies to: the marked skills if any, else the one under the cursor.
    fn targets(&self) -> Vec<usize> {
        let g = self.grid();
        if self.tab == Tab::Skills && !g.marked.is_empty() {
            g.view.iter().copied().filter(|&i| g.marked.contains(&self.skills[i].id)).collect()
        } else {
            self.current().into_iter().collect()
        }
    }

    // ---------- actions ----------

    fn ask_remove(&mut self) {
        match self.tab {
            Tab::Skills => {
                let rows: Vec<&Skill> = self.targets().into_iter().map(|i| &self.skills[i]).collect();
                let ok: Vec<(String, PathBuf)> = rows.iter().filter(|s| s.removable()).map(|s| (s.name.clone(), s.path.clone())).collect();
                if ok.is_empty() {
                    self.status = rows.first().map(|s| s.hint.clone()).unwrap_or_default();
                    return;
                }
                let skipped = rows.len() - ok.len();
                let mut lines = vec!["Moved to the trash, so you can restore them from there.".to_string(), String::new()];
                lines.extend(ok.iter().take(15).map(|(n, _)| n.clone()));
                if ok.len() > 15 {
                    lines.push(format!("…and {} more", ok.len() - 15));
                }
                if skipped > 0 {
                    lines.push(String::new());
                    lines.push(format!("{skipped} can't be removed here (plugin or claude.ai skills) and will be skipped."));
                }
                let title = if ok.len() == 1 { format!("Remove {}?", ok[0].0) } else { format!("Remove {} skills?", ok.len()) };
                self.popup = Some(Popup::Confirm { title, lines, action: Action::RemoveSkills(ok) });
            }
            Tab::Plugins => {
                let Some(i) = self.current() else { return };
                let p = &self.plugins[i];
                self.popup = Some(Popup::Confirm {
                    title: format!("Uninstall {}?", p.name),
                    lines: vec![format!("This runs: claude plugin uninstall {} --scope {}", p.name, p.scope)],
                    action: Action::Uninstall { name: p.name.clone(), scope: p.scope.clone(), project: p.project.clone() },
                });
            }
            Tab::Mcp => {}
        }
    }

    fn run(&mut self, action: Action) {
        match action {
            Action::RemoveSkills(rows) => {
                let failed: Vec<String> = rows.iter().filter(|(_, p)| trash::delete(p).is_err()).map(|(_, p)| p.display().to_string()).collect();
                let moved = rows.len() - failed.len();
                self.status = match (moved, failed.is_empty()) {
                    (1, true) => format!("Moved {} to the trash.", rows[0].0),
                    (n, true) => format!("Moved {n} skills to the trash."),
                    (n, false) => format!("Moved {n} to the trash; couldn't move {}.", failed.join(", ")),
                };
                self.grids[0].marked.clear();
                self.reload_config();
            }
            Action::Uninstall { name, scope, project } => {
                let msg = format!("Uninstalled {name}.");
                self.cli(vec!["plugin".into(), "uninstall".into(), name, "--scope".into(), scope], &project, msg, false);
            }
        }
    }

    fn toggle_plugin(&mut self) {
        let Some(i) = self.current() else { return };
        let p = &self.plugins[i];
        let on = !p.enabled;
        let (name, scope) = (p.name.clone(), p.toggle_scope.clone());
        let cwd = if scope == "user" { String::new() } else { p.project.clone() };
        let msg = format!("{} {name} ({scope}). New Claude Code sessions will pick it up.", if on { "Enabled" } else { "Disabled" });
        let verb = if on { "enable" } else { "disable" };
        self.cli(vec!["plugin".into(), verb.into(), name, "--scope".into(), scope], &cwd, msg, false);
    }

    fn open_catalog(&mut self) {
        self.popup = Some(Popup::Catalog { filter: String::new(), cursor: 0 });
        if self.available.is_none() && !self.loading_catalog {
            self.loading_catalog = true;
            self.cli(vec!["plugin".into(), "list".into(), "--available".into(), "--json".into()], "", String::new(), true);
        }
    }

    /// Catalog entries matching the popup's filter.
    pub fn catalog_view(&self, filter: &str) -> Vec<&Available> {
        let f = filter.to_lowercase();
        self.available.iter().flatten()
            .filter(|a| f.is_empty() || [&a.name, &a.marketplace, &a.description].iter().any(|t| t.to_lowercase().contains(&f)))
            .collect()
    }

    fn picked(&mut self, next: PickFor, value: String) {
        match next {
            PickFor::SkillTarget(src) => {
                let dest = if value == "user" { data::claude_dir().join("skills") } else { Path::new(&value).join(".claude/skills") };
                self.status = match data::copy_skill(&src, &dest) {
                    Ok(d) => format!("Added {}. New Claude Code sessions will load it.", d.display()),
                    Err(e) => e,
                };
                self.reload_config();
            }
            PickFor::PluginScope(id) if value == "user" => self.install(id, value, String::new()),
            PickFor::PluginScope(id) => {
                let title = format!("Install {id} for which project?");
                self.popup = Some(Popup::Pick { title, items: self.projects.clone(), values: self.projects.clone(),
                                                cursor: 0, next: PickFor::PluginProject(id, value) });
            }
            PickFor::PluginProject(id, scope) => self.install(id, scope, value),
        }
    }

    fn install(&mut self, id: String, scope: String, project: String) {
        // project/local installs are written relative to the CLI's working directory
        let msg = format!("Installed {id}. Restart Claude Code sessions to load it.");
        self.cli(vec!["plugin".into(), "install".into(), id, "--scope".into(), scope], &project, msg, false);
    }

    // ---------- usage chart ----------

    pub fn segs(&self, day: &Day) -> Vec<Seg> {
        if self.group == Group::Projects { day.projects.clone() } else { day.sessions.clone() }
    }

    fn clamp_seg(&mut self) {
        let n = self.days.get(self.day).map(|d| self.segs(d).len()).unwrap_or(0);
        self.seg = self.seg.min(n.saturating_sub(1));
    }

    fn move_day(&mut self, delta: isize) {
        if !self.days.is_empty() {
            self.day = (self.day as isize + delta).clamp(0, self.days.len() as isize - 1) as usize;
            self.clamp_seg();
        }
    }

    // ---------- input ----------

    pub fn on_key(&mut self, k: KeyEvent) {
        if self.popup.is_some() {
            return self.popup_key(k);
        }
        if self.searching {
            return self.search_key(k);
        }
        let (ctrl, shift) = (k.modifiers.contains(KeyModifiers::CONTROL), k.modifiers.contains(KeyModifiers::SHIFT));
        match k.code {
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = if self.focus == Focus::Usage { Focus::Table } else { Focus::Usage };
                self.details_scroll = 0;
            }
            KeyCode::Char(c @ '1'..='3') => {
                self.tab = TABS[(c as u8 - b'1') as usize].0;
                self.focus = Focus::Table;
                self.details_scroll = 0;
            }
            KeyCode::Char('r') | KeyCode::F(5) => {
                self.reload_config();
                self.reload_usage();
                self.limits_at = None; // fetch now, skipping the 30s wait
                self.limits_due = true;
                self.status = "Reloaded.".into();
            }
            KeyCode::Char('g') => {
                self.group = if self.group == Group::Projects { Group::Sessions } else { Group::Projects };
                self.seg = 0;
            }
            KeyCode::Char(']') => self.details_scroll = self.details_scroll.saturating_add(3),
            KeyCode::Char('[') => self.details_scroll = self.details_scroll.saturating_sub(3),
            _ if self.focus == Focus::Usage => match k.code {
                KeyCode::Up | KeyCode::Char('k') => self.move_day(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_day(1),
                KeyCode::PageUp => self.move_day(-10),
                KeyCode::PageDown => self.move_day(10),
                KeyCode::Left | KeyCode::Char('h') => self.seg = self.seg.saturating_sub(1),
                KeyCode::Right | KeyCode::Char('l') => {
                    self.seg += 1;
                    self.clamp_seg();
                }
                _ => {}
            },
            _ => self.table_key(k, ctrl, shift),
        }
    }

    fn table_key(&mut self, k: KeyEvent, ctrl: bool, shift: bool) {
        let skills = self.tab == Tab::Skills;
        let cur = self.grid().cursor;
        let last = self.grid().view.len().saturating_sub(1);
        match k.code {
            // Shift+arrows: range from the anchor (Space marks single rows)
            KeyCode::Up | KeyCode::Down if skills && shift && !self.grid().view.is_empty() => {
                let to = if k.code == KeyCode::Up { cur.saturating_sub(1) } else { (cur + 1).min(last) };
                if self.grid().anchor.is_none() || self.grid().marked.is_empty() {
                    self.grid_mut().anchor = Some(cur);
                }
                self.mark_range(to, false);
                self.grid_mut().cursor = to;
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-15),
            KeyCode::PageDown => self.move_cursor(15),
            KeyCode::Home => self.move_cursor(-(cur as isize)),
            KeyCode::End => self.move_cursor(last as isize),
            KeyCode::Char('a') if ctrl && skills => {
                let ids: Vec<String> = self.grid().view.iter().map(|&i| self.skills[i].id.clone()).collect();
                self.grid_mut().marked.extend(ids);
            }
            KeyCode::Char(' ') if skills => {
                self.toggle_mark(cur);
                self.move_cursor(1);
            }
            KeyCode::Char(' ') | KeyCode::Enter if self.tab == Tab::Plugins => self.toggle_plugin(),
            KeyCode::Esc => {
                let g = self.grid_mut();
                if !g.marked.is_empty() {
                    g.marked.clear();
                } else if !g.filter.is_empty() {
                    g.filter.clear();
                    self.rebuild(self.tab);
                }
            }
            KeyCode::Char('/') => self.searching = true,
            KeyCode::Char('s') => self.sort_by((self.grid().sort + 1) % columns(self.tab).len()),
            KeyCode::Char('S') => self.sort_by(self.grid().sort),
            KeyCode::Char('d') | KeyCode::Delete => self.ask_remove(),
            KeyCode::Char('o') => {
                if let Some(i) = self.current() {
                    match self.tab {
                        Tab::Skills => open_folder(&self.skills[i].path),
                        Tab::Plugins => open_folder(&self.plugins[i].path),
                        Tab::Mcp => {}
                    }
                }
            }
            KeyCode::Char('a') if skills => {
                self.popup = Some(Popup::Input { title: "Add skill: folder that contains SKILL.md".into(), text: String::new() });
            }
            KeyCode::Char('a') if self.tab == Tab::Plugins => self.open_catalog(),
            _ => {}
        }
    }

    fn search_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Enter => self.searching = false,
            KeyCode::Esc => {
                self.searching = false;
                self.grid_mut().filter.clear();
            }
            KeyCode::Backspace => {
                self.grid_mut().filter.pop();
            }
            _ => match is_text_key(&k) {
                Some(c) => self.grid_mut().filter.push(c),
                None => return,
            },
        }
        self.rebuild(self.tab);
    }

    fn popup_key(&mut self, k: KeyEvent) {
        let Some(popup) = self.popup.take() else { return };
        let esc = k.code == KeyCode::Esc;
        self.popup = match popup {
            Popup::Confirm { action, .. } if matches!(k.code, KeyCode::Char('y') | KeyCode::Enter) => {
                self.run(action);
                None
            }
            Popup::Confirm { .. } if esc || k.code == KeyCode::Char('n') => None,
            Popup::Input { .. } if esc => None,
            Popup::Input { title, mut text } => match k.code {
                KeyCode::Enter => {
                    // ~/ means home, like in a shell
                    let path = match text.strip_prefix("~/") { Some(rest) => data::home().join(rest), None => PathBuf::from(&text) };
                    if path.join("SKILL.md").is_file() {
                        let mut values = vec!["user".to_string()];
                        values.extend(self.projects.iter().cloned());
                        let items = values.iter().map(|v| if v == "user" { "User: ~/.claude/skills (all projects)".into() } else { format!("Project: {v}") }).collect();
                        Some(Popup::Pick { title: format!("Copy {} into", path.display()), items, values, cursor: 0, next: PickFor::SkillTarget(path) })
                    } else {
                        self.status = format!("{} has no SKILL.md, so it isn't a skill folder.", path.display());
                        Some(Popup::Input { title, text })
                    }
                }
                KeyCode::Backspace => {
                    text.pop();
                    Some(Popup::Input { title, text })
                }
                _ => {
                    text.extend(is_text_key(&k));
                    Some(Popup::Input { title, text })
                }
            },
            Popup::Pick { .. } if esc => None,
            Popup::Pick { title, items, values, mut cursor, next } => match k.code {
                KeyCode::Enter if !values.is_empty() => {
                    self.picked(next, values[cursor].clone());
                    self.popup.take() // picked() may have opened the next step
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    cursor = cursor.saturating_sub(1);
                    Some(Popup::Pick { title, items, values, cursor, next })
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    cursor = (cursor + 1).min(values.len().saturating_sub(1));
                    Some(Popup::Pick { title, items, values, cursor, next })
                }
                _ => Some(Popup::Pick { title, items, values, cursor, next }),
            },
            Popup::Catalog { .. } if esc => None,
            Popup::Catalog { mut filter, mut cursor } => {
                let n = self.catalog_view(&filter).len();
                match k.code {
                    KeyCode::Enter => {
                        if let Some(a) = self.catalog_view(&filter).get(cursor) {
                            let id = a.id.clone();
                            let items = vec!["user: every project, just you".into(),
                                             "project: one project, shared with anyone using it (.claude/settings.json)".into(),
                                             "local: one project, just you (.claude/settings.local.json)".into()];
                            let values = vec!["user".into(), "project".into(), "local".into()];
                            return self.popup = Some(Popup::Pick { title: format!("Install {id} for"), items, values, cursor: 0, next: PickFor::PluginScope(id) });
                        }
                    }
                    KeyCode::Up => cursor = cursor.saturating_sub(1),
                    KeyCode::Down => cursor = (cursor + 1).min(n.saturating_sub(1)),
                    KeyCode::PageUp => cursor = cursor.saturating_sub(10),
                    KeyCode::PageDown => cursor = (cursor + 10).min(n.saturating_sub(1)),
                    KeyCode::Backspace => {
                        filter.pop();
                        cursor = 0;
                    }
                    _ => {
                        if let Some(c) = is_text_key(&k) {
                            filter.push(c);
                            cursor = 0;
                        }
                    }
                }
                Some(Popup::Catalog { filter, cursor })
            }
            other => Some(other),
        };
    }

    pub fn on_mouse(&mut self, m: MouseEvent) {
        if self.popup.is_some() {
            return;
        }
        let at = Position::new(m.column, m.row);
        let (ctrl, shift) = (m.modifiers.contains(KeyModifiers::CONTROL), m.modifiers.contains(KeyModifiers::SHIFT));
        let wheel = match m.kind {
            MouseEventKind::ScrollUp => -3,
            MouseEventKind::ScrollDown => 3,
            _ => 0,
        };
        if wheel != 0 {
            if self.hits.details.contains(at) {
                self.details_scroll = (self.details_scroll as isize + wheel).max(0) as u16;
            } else if self.hits.usage.contains(at) {
                self.move_day(wheel);
            } else if self.hits.table.contains(at) {
                self.move_cursor(wheel);
            }
            return;
        }
        if m.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        if let Some(&(_, tab)) = self.hits.tabs.iter().find(|(r, _)| r.contains(at)) {
            self.tab = tab;
            self.focus = Focus::Table;
        } else if self.hits.days.contains(at) {
            self.focus = Focus::Usage;
            let row = (m.row - self.hits.days.y) as usize;
            self.day = (self.days_offset + row).min(self.days.len().saturating_sub(1));
            if let Some(i) = self.hits.segs.get(row).and_then(|s| s.iter().position(|&(a, b)| (a..b).contains(&m.column))) {
                self.seg = i;
            }
            self.clamp_seg();
        } else if self.hits.rows.contains(at) {
            self.focus = Focus::Table;
            let pos = self.grid().offset + (m.row - self.hits.rows.y) as usize;
            if pos >= self.grid().view.len() {
                return;
            }
            if self.tab == Tab::Skills && shift {
                self.mark_range(pos, ctrl);
            } else if self.tab == Tab::Skills && ctrl {
                self.toggle_mark(pos);
            } else {
                self.grid_mut().marked.clear();
                self.grid_mut().anchor = Some(pos);
            }
            self.grid_mut().cursor = pos;
            self.details_scroll = 0;
        } else if self.hits.table.contains(at) {
            self.focus = Focus::Table;
            if let Some(col) = self.hits.header.iter().position(|&(a, b)| (a..b).contains(&m.column)) {
                if m.row == self.hits.rows.y.saturating_sub(1) {
                    self.sort_by(col);
                }
            }
        } else if self.hits.usage.contains(at) {
            self.focus = Focus::Usage;
        }
    }
}
