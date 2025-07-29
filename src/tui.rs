use std::{
    io::{self, stderr, Stderr},
    sync::mpsc,
    time::Duration,
};

use color_eyre::{
    eyre::{eyre, Context},
    Result,
};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::{execute, terminal::*};
use ratatui::prelude::*;
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Modifier, Style, Stylize},
    symbols::border,
    text::{Line, Text},
    widgets::{
        Block, Borders, Cell, Clear, Paragraph, Row, StatefulWidget, Table, TableState, Widget,
    },
    Frame,
};
use secrecy::SecretString;
use tokio::sync::oneshot;

use crate::{
    app::{App, AppState},
    AgentMessage,
};

type TuiTerminal = Terminal<CrosstermBackend<Stderr>>;

pub struct Tui {
    app: App,
    glib_cancel: Option<oneshot::Sender<()>>,
}

impl Tui {
    pub fn new(
        agent_receiver: mpsc::Receiver<AgentMessage>,
        glib_cancel: oneshot::Sender<()>,
    ) -> Result<Self> {
        let app = App::new(agent_receiver)?;
        Ok(Self {
            app,
            glib_cancel: Some(glib_cancel),
        })
    }

    pub fn start(&mut self) -> Result<()> {
        let mut terminal = Self::init()?;
        let result = self.run_app(&mut terminal);
        Self::restore()?;
        result?;
        self.app.print_exit_mount_point();
        Ok(())
    }

    fn run_app(&mut self, terminal: &mut TuiTerminal) -> Result<()> {
        while !self.app.exit {
            terminal.draw(|frame| self.render(frame))?;
            self.app.tick()?;
            self.handle_events().wrap_err("failed handling events")?;
        }
        terminal.draw(|frame| {
            frame.render_widget(
                Paragraph::new(self.app.state_msg.as_deref().unwrap_or("exiting...")),
                frame.size(),
            )
        })?;

        // check remaining tasks
        while let Some(task) = self.app.tasks.pop_front() {
            match self.app.runtime.block_on(task)? {
                Ok(msg) => self.app.handle_message(msg)?,
                Err(err) => {
                    self.app.state_msg = Some(format!("Error: {err}"));
                    self.app.exit = false;
                    return self.run_app(terminal);
                }
            }
        }

        if !self.app.exit {
            return self.run_app(terminal);
        }

        self.glib_cancel.take().unwrap().send(()).ok();

        Ok(())
    }

    fn handle_events(&mut self) -> Result<()> {
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
                    self.handle_key_event(key_event)?;
                }
                _ => {}
            }
        };
        Ok(())
    }

    fn handle_key_event(&mut self, key_event: KeyEvent) -> Result<()> {
        if let AppState::ReadingPassphrase(passphrase) = &mut self.app.state {
            match key_event.code {
                KeyCode::Char(c) => {
                    passphrase.push(c);
                }
                KeyCode::Esc => {
                    self.app.state = AppState::DisksList;
                    self.app.state_msg = None;
                }
                KeyCode::Enter => {
                    let passphrase = passphrase.to_string();
                    self.app.state = AppState::DisksList;
                    self.app.mount(Some(SecretString::from(passphrase)))?;
                    if self.app.exit_after_passphrase {
                        self.app.exit = true;
                        self.app.exit_after_passphrase = false;
                    }
                }
                KeyCode::Backspace => {
                    passphrase.pop();
                }
                _ => {}
            }
            return Ok(());
        }

        if let AppState::ReadingAgentPassword {
            name: _,
            password,
            respond_to,
        } = &mut self.app.state
        {
            match key_event.code {
                KeyCode::Char(c) => {
                    password.push(c);
                }
                KeyCode::Esc => {
                    self.app.state = AppState::DisksList;
                    self.app.state_msg = None;
                }
                KeyCode::Enter => {
                    let password = password.to_string();
                    respond_to
                        .take()
                        .unwrap()
                        .send(SecretString::from(password))
                        .map_err(|_| eyre!("failed to send"))?;
                    self.app.state = AppState::DisksList;
                }
                KeyCode::Backspace => {
                    password.pop();
                }
                _ => {}
            }
            return Ok(());
        }

        match key_event.code {
            KeyCode::Char('q') | KeyCode::Esc => self.app.exit(),
            KeyCode::Char('j') | KeyCode::Down => self.app.next_device(),
            KeyCode::Char('k') | KeyCode::Up => self.app.prev_device(),
            KeyCode::Char('G') | KeyCode::End => self.app.last_device(),
            KeyCode::Char('g') | KeyCode::Home => self.app.first_device(),
            KeyCode::Char('m') => self.app.mount(None)?,
            KeyCode::Char('u') => self.app.unmount()?,
            KeyCode::Char('e') => self.app.eject()?,
            KeyCode::Char('r') => self.app.refresh()?,
            KeyCode::Enter => {
                self.app.mount(None)?;
                self.app.print_on_exit = true;
                self.app.exit();
            }
            _ => {}
        }
        Ok(())
    }

    fn init() -> io::Result<TuiTerminal> {
        execute!(stderr(), EnterAlternateScreen)?;
        enable_raw_mode()?;
        Terminal::new(CrosstermBackend::new(stderr()))
    }

    pub fn restore() -> io::Result<()> {
        execute!(stderr(), LeaveAlternateScreen)?;
        disable_raw_mode()?;
        Ok(())
    }

    fn render(&self, frame: &mut Frame) {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Fill(1),
                Constraint::Length(3),
                Constraint::Length(2),
            ])
            .split(frame.size());

        let header = Row::new(
            ["Name", "Label", "Mount Point", "Size", "Status"]
                .into_iter()
                .map(Cell::from),
        )
        .blue();
        let mut devices_rows: Vec<Row> = self
            .app
            .gui_devices
            .iter()
            .map(|d| {
                Row::new([
                    Cell::new(d.info.name.as_str()),
                    Cell::new(d.info.label.as_str()),
                    Cell::new(d.info.mount_point.as_str()),
                    Cell::new(d.info.size.as_str()),
                    Cell::new(d.state.to_string()),
                ])
            })
            .collect();
        let mut rows = vec![Row::default()];
        rows.append(&mut devices_rows);
        let widths = [
            Constraint::Fill(1),
            Constraint::Fill(1),
            Constraint::Fill(1),
            Constraint::Max(10),
            Constraint::Max(10),
        ];
        let mut state = TableState::new().with_selected(self.app.selected_device_index + 1);
        StatefulWidget::render(
            Table::new(rows, widths)
                .header(header)
                .highlight_style(Style::new().blue().add_modifier(Modifier::REVERSED)),
            layout[0],
            frame.buffer_mut(),
            &mut state,
        );

        if let Some(msg) = self.app.state_msg.as_deref() {
            Paragraph::new(msg)
                .block(Block::default().borders(Borders::ALL))
                .render(layout[1], frame.buffer_mut());
        }
        Text::from(vec![
            Line::from(vec![
                "m".bold().blue(),
                " Mount".into(),
                " | ".dark_gray(),
                "u".bold().blue(),
                " Unmount".into(),
                " | ".dark_gray(),
                "e".bold().blue(),
                " Eject".into(),
                " | ".dark_gray(),
                "r".bold().blue(),
                " Refresh".into(),
            ]),
            Line::from(vec![
                "<Enter>".bold().blue(),
                " Mount and exit printing mount point".into(),
                " | ".dark_gray(),
                "q".bold().blue(),
                " Quit".into(),
            ]),
        ])
        .alignment(Alignment::Center)
        .render(layout[2], frame.buffer_mut());

        if let AppState::ReadingPassphrase(_) = self.app.state {
            password_popup(frame, "Enter passphrase for unlocking device");
        }

        if let AppState::ReadingAgentPassword {
            name,
            password: _,
            respond_to: _,
        } = &self.app.state
        {
            password_popup(frame, &format!("Enter password for user {name}"))
        }
    }
}

fn password_popup(frame: &mut Frame, title: &str) {
    let popup_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(46),
            Constraint::Fill(1),
        ])
        .split(frame.size());
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(4),
            Constraint::Fill(2),
        ])
        .split(popup_layout[1]);
    Clear.render(popup_layout[1], frame.buffer_mut());
    Block::new()
        .title(format!(" {title} "))
        .title_alignment(Alignment::Center)
        .bold()
        .borders(Borders::ALL)
        .border_set(border::THICK)
        .render(popup_layout[1], frame.buffer_mut());
}
