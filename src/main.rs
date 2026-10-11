//! claude-panel: a terminal dashboard for Claude Code (plan limits, token usage, skills, plugins, MCP, hooks).
//! Refreshes by itself whenever Claude Code writes its files. Run it, press q to quit; `--uninstall` removes it.

mod app;
mod data;
mod launcher;
mod ui;

use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use ratatui::crossterm::execute;
use std::time::{Duration, Instant};

fn main() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--uninstall") {
        launcher::uninstall();
        return Ok(());
    }
    launcher::install();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = app::App::new(tx); // first transcript scan happens here (~0.3s), before the screen switches
    app.start_watching();
    let mut terminal = ratatui::init(); // raw mode + alternate screen; restored on exit or panic
    execute!(std::io::stdout(), EnableMouseCapture)?;
    let result = run(&mut terminal, &mut app, &rx);
    execute!(std::io::stdout(), DisableMouseCapture)?;
    ratatui::restore();
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut app::App, rx: &std::sync::mpsc::Receiver<app::Msg>) -> std::io::Result<()> {
    let mut dirty = true;
    let mut drawn_at = Instant::now();
    while !app.quit {
        // redraw on any change, and once a second anyway so "3m ago" style texts stay current
        if dirty || drawn_at.elapsed() >= Duration::from_secs(1) {
            terminal.draw(|f| ui::draw(f, app))?;
            (dirty, drawn_at) = (false, Instant::now());
        }
        // wait up to 200ms for a key/mouse/resize; idle CPU stays near zero
        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(k), // Windows also sends releases
                Event::Mouse(m) => app.on_mouse(m),
                _ => {}
            }
            dirty = true;
        }
        while let Ok(msg) = rx.try_recv() {
            app.on_msg(msg);
            dirty = true;
        }
        dirty |= app.tick();
    }
    Ok(())
}
