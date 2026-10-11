//! Drawing. Layout (btop-style boxes):
//!   ┌ Usage: plan limits, tokens by model, a bar per day ─────────┐
//!   ├ 1 Skills │ 2 Plugins │ 3 MCP │ 4 Hooks ┬ Details ───────────┤
//!   status line
//! Also records where things landed (App::hits) so mouse clicks can be matched to them.

use crate::app::{App, Focus, Group, InputFor, Popup, Tab, TABS, columns};
use crate::data::{self, Seg, grouped, short};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

// Same palette as the Qt version. Series order keeps neighbours distinguishable for colour-blind readers.
const ACCENT: Color = Color::Rgb(217, 119, 87);
const MUTED: Color = Color::Rgb(113, 113, 122);
const LINE: Color = Color::Rgb(63, 63, 70);
const TRACK: Color = Color::Rgb(39, 39, 42);
const FAINT: Color = Color::Rgb(110, 64, 50); // the accent at about half strength
const CURSOR_BG: Color = Color::Rgb(52, 52, 58);
const WARN: Color = Color::Rgb(234, 179, 8);
const SERIES: [Color; 8] = [Color::Rgb(0x39, 0x87, 0xe5), Color::Rgb(0xd9, 0x59, 0x26), Color::Rgb(0x19, 0x9e, 0x70),
                            Color::Rgb(0xc9, 0x85, 0x00), Color::Rgb(0xd5, 0x51, 0x81), Color::Rgb(0x00, 0x83, 0x00),
                            Color::Rgb(0x90, 0x85, 0xe9), Color::Rgb(0xe6, 0x67, 0x67)];
const OTHER: Color = Color::Rgb(0x52, 0x52, 0x5b);

fn series(c: Option<usize>) -> Color {
    c.map(|i| SERIES[i % SERIES.len()]).unwrap_or(OTHER)
}

fn boxed<'a>(focused: bool) -> Block<'a> {
    Block::bordered().border_type(BorderType::Rounded).border_style(if focused { ACCENT } else { LINE })
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let models = app.usage.by_model.len();
    let wanted = 2 + app.limits.len().max(1) + 1 + 1 + models + 1 + 1 + app.days.len().max(3);
    let usage_h = (wanted as u16).min(area.height * 11 / 20).max(10);
    let [top, bottom, status] = Layout::vertical([Constraint::Length(usage_h), Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    let details_w = (bottom.width * 2 / 5).clamp(30, 64);
    let [table, details] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(details_w)]).areas(bottom);
    draw_usage(f, app, top);
    draw_table(f, app, table);
    draw_details(f, app, details);
    draw_status(f, app, status);
    if app.popup.is_some() {
        draw_popup(f, app, area);
    }
}

fn draw_usage(f: &mut Frame, app: &mut App, area: Rect) {
    let stale = !app.limits_live || app.limits_note.contains("failed");
    let note = Line::from(format!(" {} · {} transcripts ", app.limits_note, app.usage.files))
        .style(if stale { WARN } else { MUTED }).right_aligned();
    let block = boxed(app.focus == Focus::Usage).title(" Usage ".bold()).title_top(note);
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.hits.usage = area;
    let w = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();

    // Plan limits: one meter each. Dimmed when the numbers aren't fresh from Anthropic.
    let gauge_w = w.saturating_sub(14 + 6 + 20).max(10);
    if app.limits.is_empty() {
        lines.push(Line::from("Plan limits: none yet".fg(MUTED)));
    }
    for l in &app.limits {
        let filled = ((l.pct / 100.0).clamp(0.0, 1.0) * gauge_w as f64).round() as usize;
        let fill = if stale { ACCENT.into() } else { Style::new().fg(ACCENT) };
        lines.push(Line::from(vec![
            Span::raw(format!("{:<14}", l.label)),
            Span::styled("█".repeat(filled), if stale { fill.add_modifier(Modifier::DIM) } else { fill }),
            Span::styled("░".repeat(gauge_w - filled), TRACK),
            Span::styled(format!("{:>5.0}%", l.pct), Style::new().bold()),
            Span::styled(format!("  {}", l.reset), MUTED),
        ]));
    }
    lines.push(Line::raw(""));

    // Tokens by model, each with a faint bar of its share. Measured like the daily bars
    // (input + output + cache write): cache reads would make every bar about conversation length.
    let num_w = 12;
    let name_w = w.saturating_sub(num_w * 4);
    lines.push(Line::from(format!("{:<name_w$}{:>num_w$}{:>num_w$}{:>num_w$}{:>num_w$}", "Model", "Input", "Output", "Cache read", "Cache write")).fg(MUTED));
    let weight = |v: &[u64; 4]| v[0] + v[1] + v[3];
    let mut models: Vec<_> = app.usage.by_model.iter().collect();
    models.sort_by_key(|(_, v)| std::cmp::Reverse(weight(v)));
    let label_w = (models.iter().map(|(m, _)| m.chars().count()).max().unwrap_or(5) + 2).min(name_w);
    let bar_w = name_w.saturating_sub(label_w + 2) as u64; // 2 spaces before the numbers
    let top = models.iter().map(|(_, v)| weight(v)).max().unwrap_or(1).max(1);
    for (m, v) in models {
        let len = (bar_w * weight(v) / top) as usize;
        lines.push(Line::from(vec![
            Span::raw(format!("{:<label_w$}", m.chars().take(label_w.saturating_sub(1)).collect::<String>())),
            Span::styled("█".repeat(len), FAINT),
            Span::raw(" ".repeat(name_w.saturating_sub(label_w + len))),
            Span::raw(format!("{:>num_w$}{:>num_w$}{:>num_w$}{:>num_w$}", short(v[0]), short(v[1]), short(v[2]), short(v[3]))),
        ]));
    }
    lines.push(Line::raw(""));

    // Daily bars: heading + legend (colour = project in both groupings)
    let mut legend = vec![Span::styled(
        format!("Daily tokens by {} (g switches) ", if app.group == Group::Projects { "project" } else { "session" }), MUTED)];
    for s in &app.legend {
        legend.push(Span::styled(" ■ ", series(s.color)));
        legend.push(Span::raw(s.label.clone()));
    }
    lines.push(Line::from(legend));
    let fixed = lines.len() as u16;
    f.render_widget(Paragraph::new(lines), inner);

    // One bar per day, scrolled so the selected day stays visible
    let days_area = Rect { y: inner.y + fixed, height: inner.height.saturating_sub(fixed), ..inner };
    app.hits.days = days_area;
    app.hits.segs.clear();
    let h = days_area.height as usize;
    if app.day < app.days_offset {
        app.days_offset = app.day;
    } else if h > 0 && app.day >= app.days_offset + h {
        app.days_offset = app.day + 1 - h;
    }
    let max = app.days.iter().map(|d| d.tokens).max().unwrap_or(1).max(1);
    let bar_x = days_area.x + 19;
    let bar_w = days_area.width.saturating_sub(19) as u64;
    let focused = app.focus == Focus::Usage;
    let mut rows = Vec::new();
    for (row, (di, day)) in app.days.iter().enumerate().skip(app.days_offset).take(h).enumerate() {
        let current = focused && di == app.day;
        let mut spans = vec![Span::styled(format!("{} ", day.day), MUTED), Span::raw(format!("{:>7} ", short(day.tokens)))];
        // segment edges by cumulative rounding, so the pieces add up exactly to the bar's length
        let full = bar_w * day.tokens / max;
        let (mut acc, mut x0, mut ranges) = (0u64, 0u64, Vec::new());
        for (si, s) in app.segs(day).iter().enumerate() {
            acc += s.tokens;
            let x1 = if day.tokens > 0 { full * acc / day.tokens } else { 0 };
            let mut style = Style::new().fg(series(s.color));
            if current && si != app.seg {
                style = style.add_modifier(Modifier::DIM);
            }
            spans.push(Span::styled("█".repeat((x1 - x0) as usize), style));
            ranges.push((bar_x + x0 as u16, bar_x + x1.max(x0 + 1) as u16));
            x0 = x1;
        }
        app.hits.segs.push(ranges);
        let line = Line::from(spans);
        rows.push(if current { line.bg(CURSOR_BG) } else { line });
        let _ = row;
    }
    f.render_widget(Paragraph::new(rows), days_area);
}

fn draw_table(f: &mut Frame, app: &mut App, area: Rect) {
    // Title = the tabs; remember where each label landed for clicks
    let mut title = vec![Span::raw(" ")];
    let mut x = area.x + 2;
    app.hits.tabs.clear();
    for (i, (tab, name)) in TABS.iter().enumerate() {
        let label = format!(" {} {name} ", i + 1);
        let w = label.chars().count() as u16;
        app.hits.tabs.push((Rect::new(x, area.y, w, 1), *tab));
        let style = if *tab == app.tab { Style::new().fg(ACCENT).bold() } else { Style::new().fg(MUTED) };
        title.push(Span::styled(label, style));
        title.push(Span::styled("│", LINE));
        x += w + 1;
    }
    title.pop();
    let tab = app.tab;
    let g = &app.grids[tab as usize]; // the field, not app.grid(): that would borrow all of app
    let what = match tab { Tab::Skills => "skills", Tab::Plugins => "plugins", Tab::Mcp => "servers", Tab::Hooks => "hooks" };
    let mut info = format!(" {} of {} {what}", g.view.len(), app.len(tab));
    if tab == Tab::Plugins {
        info += &format!(" · {} enabled", app.plugins.iter().filter(|p| p.enabled).count());
    }
    if !g.marked.is_empty() {
        info += &format!(" · {} marked", g.marked.len());
    }
    if !g.filter.is_empty() {
        info += &format!(" · filter \"{}\"", g.filter);
    }
    let block = boxed(app.focus == Focus::Table).title(Line::from(title)).title_bottom(Line::from(info + " ").fg(MUTED));
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.hits.table = area;

    let cols = columns(tab);
    let widths: Vec<Constraint> = cols.iter().map(|c| c.width).collect();
    let header = Row::new(cols.iter().enumerate().map(|(i, c)| {
        let arrow = if g.sort == i { if g.asc { " ↑" } else { " ↓" } } else { "" };
        let text = Line::from(format!("{}{arrow}", c.title));
        Cell::from(if c.num { text.right_aligned() } else { text })
    })).style(Style::new().fg(MUTED).bold());
    let rows: Vec<Row> = g.view.iter().map(|&i| {
        let marked = tab == Tab::Skills && g.marked.contains(&app.skills[i].id);
        let cells = app.texts(tab, i).into_iter().zip(&cols).enumerate().map(|(ci, (t, c))| {
            let line = Line::from(t);
            let line = if c.num { line.right_aligned() } else { line };
            match (tab, ci) {
                (Tab::Plugins, 0) => Cell::from(line.fg(if app.plugins[i].enabled { ACCENT } else { MUTED })),
                _ => Cell::from(line),
            }
        });
        let row = Row::new(cells);
        if marked { row.fg(ACCENT).bold() } else { row }  // marked (multi-select) rows: accent text
    }).collect();
    let cursor_bg = if app.focus == Focus::Table { CURSOR_BG } else { TRACK };
    let table = Table::new(rows, widths.clone()).header(header).column_spacing(1).row_highlight_style(Style::new().bg(cursor_bg));
    let mut state = TableState::default().with_offset(g.offset).with_selected(Some(g.cursor));
    f.render_stateful_widget(table, inner, &mut state);
    let offset = state.offset();
    app.grids[tab as usize].offset = offset;
    app.hits.rows = Rect { y: inner.y + 1, height: inner.height.saturating_sub(1), ..inner };
    app.hits.header = Layout::horizontal(widths).spacing(1).split(inner).iter().map(|r| (r.x, r.x + r.width)).collect();
}

fn kv(k: &str, v: impl Into<String>) -> Line<'static> {
    Line::from(vec![Span::styled(format!("{k:<10}"), MUTED), Span::raw(v.into())])
}

/// Several lines for a multi-line value (e.g. a plugin's skill list), the key on the first.
fn kv_list(lines: &mut Vec<Line<'static>>, k: &str, items: &[String]) {
    if items.is_empty() {
        lines.push(kv(k, "none"));
    }
    for (i, it) in items.iter().enumerate() {
        lines.push(kv(if i == 0 { k } else { "" }, it.clone()));
    }
}

fn draw_details(f: &mut Frame, app: &mut App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    let mut desc = String::new();
    let mut title = " Details ".to_string();
    if app.focus == Focus::Usage {
        if let Some(day) = app.days.get(app.day) {
            title = format!(" {} ", day.day);
            lines.push(kv("Total", grouped(day.tokens) + " tokens"));
            lines.push(Line::raw(""));
            let segs = app.segs(day);
            for (i, s) in segs.iter().enumerate() {
                let Seg { label, sub, tokens, color } = s;
                let pct = 100.0 * *tokens as f64 / day.tokens.max(1) as f64;
                let line = Line::from(vec![Span::styled("■ ", series(*color)), Span::raw(format!("{label}  ")),
                                           Span::styled(format!("{} ({pct:.0}%)", short(*tokens)), MUTED)]);
                lines.push(if i == app.seg { line.bg(CURSOR_BG) } else { line });
                if i == app.seg && !sub.is_empty() {
                    lines.push(Line::from(format!("  {sub}")).fg(MUTED));
                }
            }
            desc = "↑↓ day · ←→ project/session · g: group by project or session".into();
        }
    } else if let Some(i) = app.current() {
        match app.tab {
            Tab::Skills => {
                let marked: Vec<&data::Skill> = app.skills.iter().filter(|s| app.grid().marked.contains(&s.id)).collect();
                if marked.len() > 1 {
                    title = format!(" {} marked ", marked.len());
                    let names: Vec<String> = marked.iter().map(|s| s.name.clone()).collect();
                    kv_list(&mut lines, "Marked", &names);
                    let blocked = marked.iter().filter(|s| !s.removable()).count();
                    desc = format!("d / Delete moves the removable ones to the trash.{}",
                        if blocked > 0 { format!(" {blocked} of them can't be removed here and will be skipped.") } else { String::new() });
                } else {
                    let s = &app.skills[i];
                    title = format!(" {} ", s.name);
                    lines.extend([kv("Scope", &*s.scope), kv("Plugin", if s.plugin.is_empty() { "—" } else { &s.plugin }),
                                  kv("Source", &*s.source), kv("Uses", s.uses.to_string()), kv("Last used", data::ago(s.last)),
                                  kv("Path", s.path.display().to_string())]);
                    desc = if s.description.is_empty() { "No description.".into() } else { s.description.clone() };
                    if !s.hint.is_empty() {
                        desc += &format!("\n\n{}", s.hint);
                    }
                }
            }
            Tab::Plugins => {
                let p = &app.plugins[i];
                title = format!(" {} ", p.name);
                lines.extend([kv("Enabled", if p.enabled { "yes" } else { "no" }), kv("Scope", &*p.scope),
                              kv("Project", if p.project.is_empty() { "all projects" } else { &p.project }),
                              kv("Version", &*p.version), kv("Author", if p.author.is_empty() { "—" } else { &p.author }),
                              kv("Uses", p.uses.to_string())]);
                kv_list(&mut lines, "Skills", &p.skills);
                kv_list(&mut lines, "MCP", &p.servers);
                lines.push(kv("Path", p.path.display().to_string()));
                desc = p.description.clone();
            }
            Tab::Mcp => {
                let m = &app.mcp[i];
                title = format!(" {} ", m.name);
                lines.extend([kv("Scope", &*m.scope), kv("Defined in", &*m.place), kv("Transport", &*m.kind)]);
                desc = m.target.clone();
            }
            Tab::Hooks => {
                let h = &app.hooks[i];
                title = format!(" {} ", h.event);
                lines.extend([kv("Matcher", if h.matcher.is_empty() { "* (all)" } else { &h.matcher }), kv("Scope", &*h.scope),
                              kv("Type", data::get_str(&h.raw, "type")), kv("Timeout", h.raw.get("timeout").map(|t| t.to_string()).unwrap_or("default".into())),
                              kv("File", h.file.display().to_string())]);
                desc = h.command.clone();
                if !h.hint.is_empty() {
                    desc += &format!("\n\n{}", h.hint);
                }
            }
        }
    }
    if !desc.is_empty() {
        lines.push(Line::raw(""));
        lines.extend(Text::from(desc).lines);
    }
    let block = boxed(false).title(Line::from(title).bold());
    app.hits.details = area;
    f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: false }).scroll((app.details_scroll, 0)), area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let keys = if app.searching {
        "type to filter · Enter keep · Esc clear"
    } else if app.focus == Focus::Usage {
        "Tab table · ↑↓ day · ←→ part · g group · r reload · q quit"
    } else {
        match app.tab {
            Tab::Skills => "Tab usage · 1-4 tab · / search · Space mark · Shift+↑↓ range · d remove · a add · o open · s sort · q quit",
            Tab::Plugins => "Tab usage · 1-4 tab · / search · Space on/off · d uninstall · a add · o open · s sort · q quit",
            Tab::Mcp => "Tab usage · 1-4 tab · / search · s sort · q quit",
            Tab::Hooks => "Tab usage · 1-4 tab · / search · d remove · a add · o open file · s sort · q quit",
        }
    };
    let left = if app.searching {
        Line::from(vec![Span::styled("/", ACCENT), Span::raw(app.grid().filter.clone()), Span::styled("▏", ACCENT)])
    } else {
        let spin = if app.busy || app.fetching { "◌ " } else { "" };
        Line::from(format!("{spin}{}", app.status))
    };
    let [l, r] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(keys.chars().count() as u16)]).areas(area);
    f.render_widget(Paragraph::new(left), l);
    f.render_widget(Paragraph::new(Line::from(keys).fg(MUTED).alignment(Alignment::Right)), r);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let (w, h) = (w.min(area.width), h.min(area.height));
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

fn draw_popup(f: &mut Frame, app: &App, area: Rect) {
    let Some(popup) = &app.popup else { return };
    let hint = |s: &'static str| Line::from(s).fg(MUTED);
    match popup {
        Popup::Confirm { title, lines, .. } => {
            let r = centered(area, 70, lines.len() as u16 + 4);
            let mut text: Vec<Line> = lines.iter().map(|l| Line::from(l.clone())).collect();
            text.push(Line::raw(""));
            text.push(hint("y / Enter: yes    n / Esc: no"));
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(text).block(boxed(true).title(format!(" {title} ").bold())).wrap(Wrap { trim: false }), r);
        }
        Popup::Input { title, text, next } => {
            let r = centered(area, 80, 5);
            let keys = if matches!(next, InputFor::SkillPath) { "Enter: next · Esc: cancel · ~/ = your home folder" } else { "Enter: next · Esc: cancel" };
            let body = vec![Line::from(vec![Span::raw(text.clone()), Span::styled("▏", ACCENT)]), Line::raw(""), hint(keys)];
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(body).block(boxed(true).title(format!(" {title} ").bold())), r);
        }
        Popup::Pick { title, items, cursor, .. } => {
            let r = centered(area, 90, (items.len() as u16 + 4).min(20));
            let rows = items.iter().map(|i| Row::new([Cell::from(i.clone())]));
            let table = Table::new(rows, [Constraint::Fill(1)]).row_highlight_style(Style::new().bg(CURSOR_BG).fg(ACCENT))
                .block(boxed(true).title(format!(" {title} ").bold()).title_bottom(hint(" ↑↓ choose · Enter ok · Esc cancel ")));
            f.render_widget(Clear, r);
            f.render_stateful_widget(table, r, &mut TableState::default().with_selected(Some(*cursor)));
        }
        Popup::Catalog { filter, cursor } => {
            let r = centered(area, area.width.saturating_sub(8), area.height.saturating_sub(4));
            f.render_widget(Clear, r);
            let view = app.catalog_view(filter);
            let total = app.available.as_ref().map(|a| a.len());
            let count = match total { Some(t) => format!("{} of {t}", view.len()), None => "loading catalog…".into() };
            let block = boxed(true).title(" Add plugin ".bold())
                .title_bottom(hint(" type to search · ↑↓ choose · Enter install… · Esc close "));
            let inner = block.inner(r);
            f.render_widget(block, r);
            let [search, list, about] = Layout::vertical([Constraint::Length(2), Constraint::Fill(1), Constraint::Length(4)]).areas(inner);
            f.render_widget(Paragraph::new(Line::from(vec![Span::styled("Search: ", MUTED), Span::raw(filter.clone()),
                                                           Span::styled("▏", ACCENT), Span::styled(format!("   {count}"), MUTED)])), search);
            let rows = view.iter().map(|a| Row::new([
                Cell::from(a.name.clone()), Cell::from(a.marketplace.clone()).fg(MUTED),
                Cell::from(Line::from(short(a.installs)).right_aligned()),
                Cell::from(if a.installed { "installed" } else { "" }).fg(ACCENT),
                Cell::from(a.description.clone()).fg(MUTED)]));
            let widths = [Constraint::Length(28), Constraint::Length(26), Constraint::Length(7), Constraint::Length(10), Constraint::Fill(1)];
            let table = Table::new(rows, widths).column_spacing(1).row_highlight_style(Style::new().bg(CURSOR_BG))
                .header(Row::new(["Plugin", "Marketplace", "Installs", "", "Description"]).fg(MUTED).bold());
            f.render_stateful_widget(table, list, &mut TableState::default().with_selected(Some(*cursor)));
            let desc = view.get(*cursor).map(|a| a.description.clone()).unwrap_or_default();
            f.render_widget(Paragraph::new(desc).wrap(Wrap { trim: true }), about);
        }
    }
}
