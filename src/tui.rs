use std::io::{self, stderr, Stderr};

use crossterm::{execute, terminal::*};
use ratatui::prelude::*;

pub type Tui = Terminal<CrosstermBackend<Stderr>>;

pub fn init() -> io::Result<Tui> {
    execute!(stderr(), EnterAlternateScreen)?;
    enable_raw_mode()?;
    Terminal::new(CrosstermBackend::new(stderr()))
}

pub fn restore() -> io::Result<()> {
    execute!(stderr(), LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}
