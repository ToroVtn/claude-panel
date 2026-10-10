//! Reading Claude Code's files: transcripts (token usage), skills, plugins, MCP servers, plan limits.
//! Plain functions over plain data, no terminal code, so `cargo test` can check them on their own.

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_default()
}

pub fn claude_dir() -> PathBuf {
    home().join(".claude")
}

/// Missing or half-written files are normal (Claude Code rewrites them live), so they read as `{}`.
pub fn read_json(path: &Path) -> Value {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| Value::Object(Default::default()))
}

pub fn get_str<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn get_u64(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// Keys of a JSON object (empty for anything else).
fn keys(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_object).map(|m| m.keys().cloned().collect()).unwrap_or_default()
}

/// name/description etc. from the `---` header of a SKILL.md, without a YAML library.
/// Covers flat keys and folded (indented continuation) values, not lists or nesting.
pub fn frontmatter(md: &Path) -> HashMap<String, String> {
    let mut meta = HashMap::new();
    let Ok(text) = fs::read_to_string(md) else { return meta };
    let mut lines = text.lines();
    if lines.next() != Some("---") {
        return meta;
    }
    let mut key: Option<String> = None;
    for line in lines {
        if line == "---" {
            break;
        }
        if line.starts_with([' ', '\t']) {
            if let Some(k) = &key {
                // Indented line = continuation of a multi-line value (YAML "description: >" style)
                let v: &mut String = meta.entry(k.clone()).or_default();
                *v = format!("{v} {}", line.trim()).trim().to_string();
            }
            continue;
        }
        key = None;
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim().to_string(), v.trim());
            let v = if matches!(v, ">" | "|" | ">-" | "|-") { "" } else { v.trim_matches(['"', '\'']) };
            meta.insert(k.clone(), v.to_string());
            key = Some(k);
        }
    }
    meta
}

pub fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_millis() as f64
}

/// "3h ago" for a millisecond timestamp (0 = never).
pub fn ago(ms: f64) -> String {
    if ms <= 0.0 {
        return "never".into();
    }
    let secs = (now_ms() - ms) / 1000.0;
    for (size, unit) in [(86400.0, "d"), (3600.0, "h"), (60.0, "m")] {
        if secs >= size {
            return format!("{}{unit} ago", (secs / size) as u64);
        }
    }
    "just now".into()
}

/// 1234567 -> "1.2M": compact, and no locale question about "." vs "," grouping.
pub fn short(n: u64) -> String {
    match n {
        n if n >= 1_000_000_000 => format!("{:.1}G", n as f64 / 1e9),
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{}K", (n as f64 / 1e3).round()),
        n => n.to_string(),
    }
}

/// 1234567 -> "1 234 567" (exact numbers in the details box).
pub fn grouped(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

// ---------- token usage from transcripts ----------

#[derive(Default, Clone, Debug, PartialEq)]
pub struct Session {
    pub project: String, // the cwd it started in (a later `cd subdir` mustn't move it)
    pub title: String,   // AI-made title
    pub custom: String,  // title the user set
}

impl Session {
    pub fn label(&self, id: &str) -> String {
        [&self.custom, &self.title].into_iter().find(|s| !s.is_empty()).cloned()
            .unwrap_or_else(|| id.chars().take(8).collect())
    }
}

/// Token totals from all transcripts, kept up to date incrementally: `update` reads only the bytes
/// appended since last time, so refreshing on every Claude Code action stays cheap.
#[derive(Default)]
pub struct Usage {
    offsets: HashMap<PathBuf, u64>, // file -> bytes already read
    seen: HashSet<String>,          // message ids already counted
    pub by_model: HashMap<String, [u64; 4]>, // input, output, cache read, cache write
    pub by_day_session: HashMap<(String, String), u64>, // (day, session) -> input+output+cache write
    pub sessions: HashMap<String, Session>,
    pub files: usize,
}

fn jsonl_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            jsonl_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
}

impl Usage {
    /// Read what was appended to the transcripts under `root` since the last call.
    /// A file that shrank or vanished was rewritten, so then start over (rare: Claude Code appends).
    pub fn update(&mut self, root: &Path) {
        let mut files = Vec::new();
        jsonl_files(root, &mut files);
        let rewritten = self.offsets.iter().any(|(p, &off)| fs::metadata(p).map(|m| m.len() < off).unwrap_or(true));
        if rewritten {
            *self = Usage::default();
        }
        self.files = files.len();
        for f in &files {
            self.read_new(f);
        }
    }

    fn read_new(&mut self, path: &Path) {
        let start = self.offsets.get(path).copied().unwrap_or(0);
        let Ok(mut fh) = fs::File::open(path) else { return };
        if fh.metadata().map(|m| m.len() == start).unwrap_or(true) || fh.seek(SeekFrom::Start(start)).is_err() {
            return; // nothing new (or unreadable)
        }
        let mut reader = BufReader::new(fh);
        let (mut pos, mut line) = (start, Vec::new());
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                // Stop before a line without its newline: Claude Code may still be writing it.
                Ok(n) if n > 0 && line.ends_with(b"\n") => {
                    pos += n as u64;
                    self.line(&line);
                }
                _ => break,
            }
        }
        self.offsets.insert(path.to_path_buf(), pos);
    }

    /// One transcript line. One API response is logged as several lines (one per content block),
    /// each repeating the same usage, so each message.id counts once (else totals are ~2-3x too high).
    /// Subagent transcripts carry their parent's sessionId, so their tokens count toward it.
    fn line(&mut self, raw: &[u8]) {
        let Ok(text) = std::str::from_utf8(raw) else { return };
        // cheap pre-filter: only usage lines and title lines (aiTitle/customTitle) need JSON parsing
        if !text.contains("\"usage\"") && !text.contains("Title\"") {
            return;
        }
        let Ok(rec) = serde_json::from_str::<Value>(text) else { return };
        let sid = match get_str(&rec, "sessionId") { "" => "?", s => s }.to_string();
        let s = self.sessions.entry(sid.clone()).or_default();
        match get_str(&rec, "type") {
            "ai-title" => s.title = get_str(&rec, "aiTitle").into(),
            "custom-title" => s.custom = get_str(&rec, "customTitle").into(),
            _ => {}
        }
        let Some(msg) = rec.get("message").filter(|m| m.is_object()) else { return };
        let Some(u) = msg.get("usage") else { return };
        if !self.seen.insert(get_str(msg, "id").to_string()) {
            return;
        }
        if s.project.is_empty() {
            s.project = get_str(&rec, "cwd").into();
        }
        let (i, o, cr, cw) = (get_u64(u, "input_tokens"), get_u64(u, "output_tokens"),
                              get_u64(u, "cache_read_input_tokens"), get_u64(u, "cache_creation_input_tokens"));
        let model = match get_str(msg, "model") { "" => "?", m => m };
        if model != "<synthetic>" {  // local placeholder messages, not real API calls
            let row = self.by_model.entry(model.to_string()).or_default();
            for (slot, n) in row.iter_mut().zip([i, o, cr, cw]) {
                *slot += n;
            }
        }
        let day = get_str(&rec, "timestamp").get(..10).unwrap_or("").to_string();
        *self.by_day_session.entry((day, sid)).or_default() += i + o + cw;
    }
}

/// One coloured piece of a day's bar (a project or a session). color: palette slot, None = "Other".
#[derive(Clone, Debug, PartialEq)]
pub struct Seg {
    pub label: String,
    pub sub: String, // project path (or "N smaller projects")
    pub tokens: u64,
    pub color: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct Day {
    pub day: String,
    pub tokens: u64,
    pub projects: Vec<Seg>,
    pub sessions: Vec<Seg>,
}

fn project_name(p: &str) -> String {
    Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "(unknown)".into())
}

/// Per-day bars split two ways (by project, by session) plus the colour legend.
/// Each project gets a fixed colour slot, biggest first. Past max_colors the smallest fold into
/// "Other": more hues than that stop being distinguishable.
pub fn daily_breakdown(by_ds: &HashMap<(String, String), u64>, sessions: &HashMap<String, Session>,
                       max_days: usize, max_colors: usize) -> (Vec<Day>, Vec<Seg>) {
    let project = |sid: &str| sessions.get(sid).map(|s| s.project.clone()).unwrap_or_default();
    let mut days: Vec<&String> = by_ds.keys().map(|(d, _)| d).collect::<HashSet<_>>().into_iter().collect();
    days.sort_unstable_by(|a, b| b.cmp(a));
    days.truncate(max_days);
    let mut per_day: HashMap<&str, Vec<(&str, u64)>> = HashMap::new();
    let mut proj_total: HashMap<String, u64> = HashMap::new();
    for ((d, sid), &n) in by_ds {
        if n > 0 && days.contains(&d) {
            per_day.entry(d).or_default().push((sid, n));
            *proj_total.entry(project(sid)).or_default() += n;
        }
    }
    let mut ranked: Vec<String> = proj_total.keys().cloned().collect();
    ranked.sort_by(|a, b| proj_total[b].cmp(&proj_total[a]).then(a.cmp(b)));
    let top = if ranked.len() <= max_colors { ranked.len() } else { max_colors - 1 };
    let slot: HashMap<&str, usize> = ranked[..top].iter().enumerate().map(|(i, p)| (p.as_str(), i)).collect();
    let others = ranked.len() - top;
    // Two projects can share a folder name (/a/app, /b/app): then show parent/name for those
    let short_names: Vec<String> = ranked[..top].iter().map(|p| project_name(p)).collect();
    let name = |i: usize| -> String {
        if short_names.iter().filter(|n| **n == short_names[i]).count() > 1 {
            let path = Path::new(&ranked[i]);
            let parent = path.parent().map(|p| project_name(&p.to_string_lossy())).unwrap_or_default();
            format!("{parent}/{}", short_names[i])
        } else {
            short_names[i].clone()
        }
    };
    let other_seg = |tokens| Seg { label: "Other".into(), sub: format!("{others} smaller projects"), tokens, color: None };

    let rows = days.iter().map(|d| {
        let mut sess = per_day.remove(d.as_str()).unwrap_or_default();
        // group by project (colour order), biggest session first within a project
        sess.sort_by_key(|&(sid, n)| (slot.get(project(sid).as_str()).copied().unwrap_or(99), std::cmp::Reverse(n), sid));
        let mut by_proj: Vec<(Option<usize>, u64)> = Vec::new();
        for &(sid, n) in &sess {
            let c = slot.get(project(sid).as_str()).copied();
            match by_proj.iter_mut().find(|(k, _)| *k == c) {
                Some((_, t)) => *t += n,
                None => by_proj.push((c, n)),
            }
        }
        by_proj.sort_by_key(|(c, _)| c.unwrap_or(99));
        Day {
            day: d.to_string(),
            tokens: sess.iter().map(|(_, n)| n).sum(),
            projects: by_proj.into_iter().map(|(c, n)| match c {
                Some(i) => Seg { label: name(i), sub: ranked[i].clone(), tokens: n, color: Some(i) },
                None => other_seg(n),
            }).collect(),
            sessions: sess.iter().map(|&(sid, n)| {
                let s = sessions.get(sid).cloned().unwrap_or_default();
                Seg { label: s.label(sid), sub: s.project.clone(), tokens: n, color: slot.get(s.project.as_str()).copied() }
            }).collect(),
        }
    }).collect();
    let mut legend: Vec<Seg> = ranked[..top].iter().enumerate()
        .map(|(i, p)| Seg { label: name(i), sub: p.clone(), tokens: proj_total[p], color: Some(i) }).collect();
    if others > 0 {
        legend.push(other_seg(ranked[top..].iter().map(|p| proj_total[p]).sum()));
    }
    (rows, legend)
}

// ---------- plan limits ----------

pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage"; // what Claude Code calls for plan limits

/// Fresh plan utilization using the login Claude Code already stored. Never refreshes the token
/// itself: that rotates credentials Claude Code owns and could log it out.
pub fn fetch_limits() -> Result<Value, String> {
    let cred = read_json(&claude_dir().join(".credentials.json"));
    let cred = cred.get("claudeAiOauth").cloned().unwrap_or_default();
    let token = get_str(&cred, "accessToken");
    if token.is_empty() {
        return Err("not logged in".into());
    }
    if (get_u64(&cred, "expiresAt") as f64) < now_ms() {
        return Err("login token expired; open Claude Code to renew it".into());
    }
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).build().into();
    let body = agent.get(USAGE_URL)
        .header("Authorization", &format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .call().map_err(|e| e.to_string())?
        .body_mut().read_to_string().map_err(|e| e.to_string())?;
    serde_json::from_str(&body).map_err(|e| e.to_string())
}

#[derive(Clone, Debug)]
pub struct Limit {
    pub label: &'static str,
    pub pct: f64,
    pub reset: String,
}

pub fn limits_from(util: &Value) -> Vec<Limit> {
    [("five_hour", "5-hour"), ("seven_day", "7-day"), ("seven_day_opus", "7-day Opus"), ("seven_day_sonnet", "7-day Sonnet")]
        .into_iter()
        .filter_map(|(key, label)| {
            let v = util.get(key).filter(|v| v.is_object())?;
            let reset = chrono::DateTime::parse_from_rfc3339(get_str(v, "resets_at")).ok()
                .map(|t| t.with_timezone(&chrono::Local).format("resets %a %H:%M").to_string())
                .unwrap_or_default();
            Some(Limit { label, pct: v.get("utilization").and_then(Value::as_f64).unwrap_or(0.0), reset })
        })
        .collect()
}

// ---------- skills / plugins / MCP ----------

pub fn known_projects(cfg: &Value) -> Vec<String> {
    let mut p: Vec<String> = keys(cfg.get("projects")).into_iter().filter(|p| Path::new(p).is_dir()).collect();
    p.sort();
    p
}

/// chain: (scope, settings), most specific first. The first that mentions the plugin decides, the way
/// Claude Code layers settings (a false in settings.local.json beats a true in settings.json).
/// Returns (enabled, scope of the deciding file, or None if no file mentions it).
pub fn plugin_enabled(name: &str, chain: &[(&str, Value)]) -> (bool, Option<String>) {
    for (scope, s) in chain {
        if let Some(v) = s.get("enabledPlugins").and_then(|e| e.get(name)) {
            return (v.as_bool().unwrap_or(false), Some(scope.to_string()));
        }
    }
    (false, None)
}

#[derive(Clone, Debug, Default)]
pub struct Plugin {
    pub id: String,
    pub name: String,
    pub version: String,
    pub scope: String,
    pub project: String,
    pub enabled: bool,
    pub toggle_scope: String, // flip it in the deciding file, or a more specific one still overrides us
    pub uses: u64,
    pub skills: Vec<String>,
    pub servers: Vec<String>,
    pub author: String,
    pub path: PathBuf,
    pub description: String,
}

pub fn load_plugins(cfg: &Value, settings: &Value) -> Vec<Plugin> {
    let installed = read_json(&claude_dir().join("plugins/installed_plugins.json"));
    let mut rows = Vec::new();
    let Some(installed) = installed.get("plugins").and_then(Value::as_object) else { return rows };
    for (name, installs) in installed {
        for inst in installs.as_array().into_iter().flatten() {
            let (scope, project) = (get_str(inst, "scope").to_string(), get_str(inst, "projectPath").to_string());
            let mut chain = vec![("user", settings.clone())]; // ~/.claude/settings.json
            if scope != "user" {  // project/local installs: that project's files come first
                let dir = Path::new(&project).join(".claude");
                chain.insert(0, ("project", read_json(&dir.join("settings.json"))));
                chain.insert(0, ("local", read_json(&dir.join("settings.local.json"))));
            }
            let (enabled, decided_by) = plugin_enabled(name, &chain);
            let path = PathBuf::from(get_str(inst, "installPath"));
            // Most plugins keep the manifest in .claude-plugin/, some (brag) at the top level
            let mut manifest = read_json(&path.join(".claude-plugin/plugin.json"));
            if manifest.as_object().is_none_or(|m| m.is_empty()) {
                manifest = read_json(&path.join("plugin.json"));
            }
            let author = match manifest.get("author") {
                Some(Value::String(s)) => s.clone(),
                Some(a) => get_str(a, "name").to_string(), // {"name": ...}
                None => String::new(),
            };
            rows.push(Plugin {
                id: format!("{name}|{scope}|{project}"),
                name: name.clone(),
                version: get_str(inst, "version").into(),
                enabled,
                toggle_scope: decided_by.unwrap_or_else(|| scope.clone()),
                uses: cfg.get("pluginUsage").and_then(|u| u.get(name)).map(|u| get_u64(u, "usageCount")).unwrap_or(0),
                skills: skill_dirs(&path.join("skills")).iter()
                    .map(|d| frontmatter(&d.join("SKILL.md")).remove("name").unwrap_or_else(|| file_name(d))).collect(),
                servers: keys(read_json(&path.join(".mcp.json")).get("mcpServers")),
                author,
                description: get_str(&manifest, "description").into(),
                scope,
                project,
                path,
            });
        }
    }
    rows
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Sub-folders of `root` that hold a SKILL.md, sorted.
fn skill_dirs(root: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(root).into_iter().flatten().flatten()
        .map(|e| e.path()).filter(|p| p.join("SKILL.md").is_file()).collect();
    v.sort();
    v
}

#[derive(Clone, Debug, Default)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub scope: String,
    pub plugin: String, // full plugin id, or ""
    pub source: String, // where it's active: ~/.claude/skills, a project, "all projects"...
    pub uses: u64,
    pub last: f64, // ms
    pub description: String,
    pub path: PathBuf,
    pub hint: String, // why it can't be removed here, or ""
}

impl Skill {
    pub fn removable(&self) -> bool {
        self.hint.is_empty()
    }
    pub fn plugin_short(&self) -> &str {
        self.plugin.split('@').next().unwrap_or("")
    }
}

pub fn load_skills(cfg: &Value, plugins: &[Plugin]) -> Vec<Skill> {
    let usage = cfg.get("skillUsage").cloned().unwrap_or_default();
    // (scope, source, project, name prefix, folder holding <skill>/SKILL.md, hint, plugin id)
    let mut sources: Vec<(String, String, String, String, PathBuf, String, String)> =
        vec![("user".into(), "~/.claude/skills".into(), "".into(), "".into(), claude_dir().join("skills"), "".into(), "".into())];
    for p in known_projects(cfg) {
        sources.push(("project".into(), p.clone(), p.clone(), "".into(), Path::new(&p).join(".claude/skills"), "".into(), "".into()));
    }
    for d in fs::read_dir(claude_dir().join("skills/synced")).into_iter().flatten().flatten().map(|e| e.path()) {
        if d.is_dir() && !file_name(&d).starts_with('.') {
            sources.push(("account".into(), "claude.ai (synced)".into(), "".into(), "anthropic-skills:".into(), d,
                "Synced from your claude.ai account; removing it here would just sync back. Manage it on claude.ai.".into(), "".into()));
        }
    }
    for p in plugins {  // the Plugin column says which plugin, so Source says where it's active
        sources.push((p.scope.clone(), if p.project.is_empty() { "all projects".into() } else { p.project.clone() },
            p.project.clone(), format!("{}:", p.name.split('@').next().unwrap_or("")), p.path.join("skills"),
            format!("Part of the plugin {}. Remove the plugin to remove its skills.", p.name), p.name.clone()));
    }
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    for (scope, source, project, prefix, root, hint, plugin) in sources {
        for dir in skill_dirs(&root) {
            let mut meta = frontmatter(&dir.join("SKILL.md"));
            let name = prefix.clone() + &meta.remove("name").unwrap_or_else(|| file_name(&dir));
            if !seen.insert((scope.clone(), project.clone(), name.clone())) {
                continue; // same plugin listed twice for one scope
            }
            let u = usage.get(&name).cloned().unwrap_or_default();
            rows.push(Skill {
                id: format!("{scope}|{project}|{}", dir.display()),
                uses: get_u64(&u, "usageCount"),
                last: u.get("lastUsedAt").and_then(Value::as_f64).unwrap_or(0.0),
                description: meta.remove("description").unwrap_or_default(),
                name,
                scope: scope.clone(),
                plugin: plugin.clone(),
                source: source.clone(),
                path: dir,
                hint: hint.clone(),
            });
        }
    }
    rows
}

#[derive(Clone, Debug)]
pub struct Mcp {
    pub id: String,
    pub name: String,
    pub scope: String,
    pub place: String, // where it's defined
    pub kind: String,  // transport
    pub target: String,
}

/// MCP servers defined on disk. claude.ai connectors (Gmail, Drive...) live server-side, so they aren't here.
pub fn load_mcp(cfg: &Value, plugins: &[Plugin]) -> Vec<Mcp> {
    let mut rows = Vec::new();
    let mut add = |servers: Option<&Value>, scope: &str, place: &str| {
        for (name, spec) in servers.and_then(Value::as_object).into_iter().flatten() {
            let target = match get_str(spec, "url") {
                "" => std::iter::once(get_str(spec, "command"))
                    .chain(spec.get("args").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str))
                    .collect::<Vec<_>>().join(" "),
                url => url.to_string(),
            };
            let kind = match get_str(spec, "type") { "" => "stdio", t => t };
            rows.push(Mcp { id: format!("{name}|{scope}|{place}"), name: name.clone(), scope: scope.into(),
                            place: place.into(), kind: kind.into(), target });
        }
    };
    add(cfg.get("mcpServers"), "user", "~/.claude.json");
    for (proj, pcfg) in cfg.get("projects").and_then(Value::as_object).into_iter().flatten() {
        add(pcfg.get("mcpServers"), "local", proj);
        add(read_json(&Path::new(proj).join(".mcp.json")).get("mcpServers"), "project", proj);
    }
    let mut seen = HashSet::new();
    for p in plugins.iter().filter(|p| p.enabled) {
        if seen.insert(&p.name) {
            add(read_json(&p.path.join(".mcp.json")).get("mcpServers"), "plugin", &p.name);
        }
    }
    rows
}

/// Copy a skill folder into a skills dir. Copy, not move: the original stays where it was.
pub fn copy_skill(src: &Path, dest_root: &Path) -> Result<PathBuf, String> {
    if !src.join("SKILL.md").is_file() {
        return Err(format!("{} has no SKILL.md, so it isn't a skill folder.", src.display()));
    }
    let dest = dest_root.join(src.file_name().ok_or("not a folder")?);
    if dest.exists() {
        return Err(format!("{} already exists.", dest.display()));
    }
    copy_tree(src, &dest).map_err(|e| e.to_string())?;
    Ok(dest)
}

/// Recursive copy that keeps symlinks as links (doesn't follow them out of the folder).
fn copy_tree(src: &Path, dest: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dest)?;
    for e in fs::read_dir(src)? {
        let e = e?;
        let (from, to) = (e.path(), dest.join(e.file_name()));
        let ty = e.file_type()?;
        if ty.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(&from)?, &to)?;
            #[cfg(not(unix))]
            fs::copy(&from, &to).map(|_| ())?; // ponytail: Windows symlinks need admin; copy the target instead
        } else if ty.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("claude-panel-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn usage_counts_each_message_once_and_reads_incrementally() {
        let dir = tmp("usage");
        let f = dir.join("t.jsonl");
        let lines = [
            json!({"timestamp": "2026-10-09T10:00:00Z", "sessionId": "s1", "cwd": "/p/one", "message": {"id": "a", "model": "m", "usage": {"input_tokens": 1, "output_tokens": 10}}}),
            // same response split per content block -> counted once
            json!({"timestamp": "2026-10-09T10:00:00Z", "sessionId": "s1", "cwd": "/p/one", "message": {"id": "a", "model": "m", "usage": {"input_tokens": 1, "output_tokens": 10}}}),
            // later cd into a subfolder: the session stays in /p/one
            json!({"timestamp": "2026-10-09T11:00:00Z", "sessionId": "s1", "cwd": "/p/one/sub", "message": {"id": "c", "model": "m", "usage": {}}}),
            json!({"timestamp": "2026-10-08T10:00:00Z", "sessionId": "s2", "cwd": "/p/two", "message": {"id": "b", "model": "m", "usage": {"input_tokens": 2, "output_tokens": 20, "cache_read_input_tokens": 5}}}),
            json!({"type": "user", "message": {"role": "user", "content": "usage"}}),
            json!({"type": "ai-title", "sessionId": "s1", "aiTitle": "first"}),
            json!({"type": "custom-title", "sessionId": "s2", "customTitle": "mine"}),
        ];
        let text: String = lines.iter().map(|l| l.to_string() + "\n").collect::<String>() + "not json usage\n";
        fs::write(&f, text).unwrap();
        let mut u = Usage::default();
        u.update(&dir);
        assert_eq!(u.by_model["m"], [3, 30, 5, 0]);
        assert_eq!(u.by_day_session[&("2026-10-09".into(), "s1".into())], 11);
        assert_eq!(u.by_day_session[&("2026-10-08".into(), "s2".into())], 22);
        assert_eq!((u.sessions["s1"].title.as_str(), u.sessions["s2"].custom.as_str()), ("first", "mine"));
        assert_eq!(u.sessions["s1"].project, "/p/one");

        // append a new message plus half a line (still being written): only the full line counts
        let more = json!({"timestamp": "2026-10-09T12:00:00Z", "sessionId": "s1", "message": {"id": "d", "model": "m", "usage": {"output_tokens": 100}}});
        let mut fh = fs::OpenOptions::new().append(true).open(&f).unwrap();
        std::io::Write::write_all(&mut fh, format!("{more}\n{{\"usage\": 1").as_bytes()).unwrap();
        u.update(&dir);
        assert_eq!(u.by_model["m"][1], 130);
        // finishing that half line later must not break or double count
        std::io::Write::write_all(&mut fh, b"}\n").unwrap();
        u.update(&dir);
        assert_eq!(u.by_model["m"][1], 130);
        // a rewritten (shorter) file is rescanned from scratch
        fs::write(&f, lines[3].to_string() + "\n").unwrap();
        u.update(&dir);
        assert_eq!(u.by_model["m"], [2, 20, 5, 0]);
    }

    #[test]
    fn daily_breakdown_folds_small_projects_into_other() {
        let by_ds = HashMap::from([(("d1".into(), "a".into()), 50), (("d1".into(), "b".into()), 30),
                                   (("d1".into(), "c".into()), 20), (("d0".into(), "a".into()), 5)]);
        let sess: HashMap<String, Session> = ["a", "b", "c"].iter()
            .map(|k| (k.to_string(), Session { project: format!("/p/{k}"), title: k.to_string(), custom: "".into() })).collect();
        let (days, legend) = daily_breakdown(&by_ds, &sess, 30, 2);
        assert_eq!(days.iter().map(|d| d.day.as_str()).collect::<Vec<_>>(), ["d1", "d0"]);
        let p: Vec<_> = days[0].projects.iter().map(|s| (s.label.as_str(), s.tokens, s.color)).collect();
        assert_eq!(p, [("a", 50, Some(0)), ("Other", 50, None)]);
        // sessions keep their project's colour
        assert_eq!(days[0].sessions.iter().map(|s| s.color).collect::<Vec<_>>(), [Some(0), None, None]);
        assert_eq!(legend.iter().map(|s| s.label.as_str()).collect::<Vec<_>>(), ["a", "Other"]);
        let sess2: HashMap<String, Session> = [("a", "/x/app"), ("b", "/y/app")].iter()
            .map(|(k, p)| (k.to_string(), Session { project: p.to_string(), ..Default::default() })).collect();
        let (_, legend) = daily_breakdown(&by_ds, &sess2, 30, 8);
        assert_eq!(legend.iter().map(|s| s.label.as_str()).collect::<Vec<_>>(), ["x/app", "y/app", "(unknown)"]); // c has no project here
    }

    #[test]
    fn most_specific_settings_file_decides_plugin_state() {
        let (local, proj) = (json!({"enabledPlugins": {"p": false}}), json!({"enabledPlugins": {"p": true}}));
        let empty = json!({});
        assert_eq!(plugin_enabled("p", &[("local", local), ("project", proj.clone()), ("user", empty.clone())]), (false, Some("local".into())));
        assert_eq!(plugin_enabled("p", &[("local", empty.clone()), ("project", proj), ("user", empty.clone())]), (true, Some("project".into())));
        assert_eq!(plugin_enabled("p", &[("local", empty.clone()), ("user", empty)]), (false, None)); // never mentioned = off
    }

    #[test]
    fn frontmatter_reads_flat_and_folded_values() {
        let d = tmp("fm");
        fs::write(d.join("s.md"), "---\nname: x\ndescription: \"hi: there\"\nlong: >\n  folded\n  text\n---\nbody: no\n").unwrap();
        let m = frontmatter(&d.join("s.md"));
        assert_eq!((m["name"].as_str(), m["description"].as_str(), m["long"].as_str()), ("x", "hi: there", "folded text"));
        assert!(!m.contains_key("body"));
    }

    #[test]
    fn copy_skill_copies_once_and_refuses_non_skills() {
        let d = tmp("copy");
        fs::create_dir_all(d.join("src/myskill")).unwrap();
        fs::write(d.join("src/myskill/SKILL.md"), "---\nname: myskill\n---\n").unwrap();
        assert_eq!(copy_skill(&d.join("src/myskill"), &d.join("dest")).unwrap(), d.join("dest/myskill"));
        assert!(d.join("dest/myskill/SKILL.md").is_file());
        assert!(copy_skill(&d.join("src/myskill"), &d.join("dest")).is_err()); // already exists
        assert!(copy_skill(&d.join("src"), &d.join("dest")).is_err()); // no SKILL.md
    }

    #[test]
    fn number_formats() {
        assert_eq!((short(999), short(12_345), short(2_779_458)), ("999".into(), "12K".into(), "2.8M".into()));
        assert_eq!(grouped(2_779_458), "2 779 458");
    }
}
