//! Orbit — a local MP3 player TUI with buckets and a real graphic EQ.

mod analyze;
mod app;
mod audio;
mod bucket;
mod config;
mod library;
mod media;
mod model;
mod queue;
mod remote;
mod stats;
mod theme;
mod ui;

use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event};

use app::App;

/// Target frame interval — drives progress, queue advancement, and the
/// spectrum animation (~20 fps).
const TICK: Duration = Duration::from_millis(50);

fn main() -> Result<()> {
    let mut app = App::new()?;

    let mut terminal = ratatui::try_init()
        .map_err(|e| anyhow::anyhow!("Orbit needs an interactive terminal: {e}"))?;
    let result = run(&mut terminal, &mut app);
    let _ = ratatui::try_restore();

    result
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        // Wait for input, but no longer than the remaining tick budget.
        let timeout = TICK.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                app.handle_key(key);
            }
        }

        if last_tick.elapsed() >= TICK {
            app.tick();
            last_tick = Instant::now();
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}
