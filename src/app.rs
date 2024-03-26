use std::{fmt::Display, time::Duration};

use color_eyre::{
    eyre::{bail, Context},
    Result,
};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style, Stylize},
    symbols::border,
    text::Line,
    widgets::{
        block::Title, Block, Borders, Cell, Clear, Paragraph, Row, StatefulWidget, Table,
        TableState, Widget,
    },
    Frame,
};
use tokio::sync::mpsc::{error::TryRecvError, Receiver, Sender};

use crate::tui;

pub struct App {
    devices: Vec<Device>,
    selected_device_index: usize,
    passphrase: Option<String>,
    reading_passphrase: bool,
    state_msg: Option<String>,
    exit: bool,
    exit_mount_point: Option<String>,
    print_on_exit: bool,
    sender: Sender<TuiMessage>,
    receiver: Receiver<UDisks2Message>,
}

pub struct Device {
    pub name: String,
    pub label: String,
    pub size: u64,
    pub state: DeviceState,
}

pub enum DeviceState {
    Locked,
    UnmountedUnlocked,
    Mounted,
    Unmounted,
}

pub enum TuiMessage {
    Mount(usize),
    Unmount(usize),
    UnlockAndMount(usize, String),
    Quit,
}

pub enum UDisks2Message {
    Mounted(usize, String),
    Unmounted(usize),
    Locked(usize),
    UnmountedAndLocked(usize),
    UnlockedAndMounted(usize, String),
    AlreadyMounted(usize, String),
    AlreadyUnmounted(usize),
    Devices(Vec<Device>),
    Err(String),
}

impl App {
    pub fn new(sender: Sender<TuiMessage>, receiver: Receiver<UDisks2Message>) -> Self {
        Self {
            devices: Vec::new(),
            selected_device_index: 0,
            passphrase: None,
            reading_passphrase: false,
            state_msg: None,
            exit: false,
            exit_mount_point: None,
            print_on_exit: false,
            sender,
            receiver,
        }
    }

    pub fn run(&mut self, terminal: &mut tui::Tui) -> Result<()> {
        while !self.exit {
            terminal.draw(|frame| self.render_frame(frame))?;
            self.receive_udisks_messages()?;
            self.handle_events().wrap_err("handling events failed")?;
        }
        self.sender.blocking_send(TuiMessage::Quit)?;
        terminal.draw(|frame| frame.render_widget(Paragraph::new("exiting..."), frame.size()))?;
        // handle remaining messages before exiting
        while let Some(msg) = self.receiver.blocking_recv() {
            match msg {
                UDisks2Message::Err(err_msg) => {
                    bail!(err_msg)
                }
                _ => self.handle_udisks_message(msg)?,
            }
        }
        Ok(())
    }

    pub fn print_exit_mount_point(&self) {
        if !self.print_on_exit {
            return;
        }

        if let Some(mount_point) = &self.exit_mount_point {
            println!("{}", mount_point);
        }
    }

    fn render_frame(&self, frame: &mut Frame) {
        frame.render_widget(self, frame.size())
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
        if self.reading_passphrase {
            if self.passphrase.is_none() {
                self.passphrase = Some("".to_string());
            }
            let passphrase = self.passphrase.as_mut().unwrap();
            match key_event.code {
                KeyCode::Char(c) => {
                    passphrase.push(c);
                }
                KeyCode::Esc => {
                    self.passphrase = None;
                    self.reading_passphrase = false;
                }
                KeyCode::Enter => {
                    self.reading_passphrase = false;
                    self.mount()?;
                }
                _ => {}
            }
            return Ok(());
        }
        match key_event.code {
            KeyCode::Char('q') | KeyCode::Esc => self.exit(),
            KeyCode::Char('j') | KeyCode::Down => self.next_device(),
            KeyCode::Char('k') | KeyCode::Up => self.prev_device(),
            KeyCode::Char('G') | KeyCode::End => self.last_device(),
            KeyCode::Char('g') | KeyCode::Home => self.first_device(),
            KeyCode::Char('m') => self.mount()?,
            KeyCode::Char('u') => self.unmount()?,
            KeyCode::Enter => {
                self.mount()?;
                self.print_on_exit = true;
                self.exit();
            }
            _ => {}
        }
        Ok(())
    }

    fn exit(&mut self) {
        self.exit = true;
    }

    fn next_device(&mut self) {
        if self.devices.is_empty() {
            return;
        }

        if self.devices.len() - 1 > self.selected_device_index {
            self.selected_device_index += 1;
        }
    }

    fn prev_device(&mut self) {
        if self.selected_device_index > 0 {
            self.selected_device_index -= 1;
        }
    }

    fn last_device(&mut self) {
        if self.devices.is_empty() {
            return;
        }

        self.selected_device_index = self.devices.len() - 1;
    }

    fn first_device(&mut self) {
        self.selected_device_index = 0;
    }

    fn receive_udisks_messages(&mut self) -> Result<()> {
        match self.receiver.try_recv() {
            Ok(msg) => self.handle_udisks_message(msg),
            Err(TryRecvError::Empty) => Ok(()),
            Err(err) => bail!("failed receiving message: {}", err),
        }
    }

    fn handle_udisks_message(&mut self, msg: UDisks2Message) -> Result<()> {
        match msg {
            UDisks2Message::Devices(devices) => {
                self.devices = devices;
                Ok(())
            }
            UDisks2Message::Mounted(idx, mount_point) => {
                let device = &mut self.devices[idx];
                device.state = DeviceState::Mounted;
                self.state_msg = Some(format!("Mounted {} at {}", device.name, mount_point));
                self.exit_mount_point = Some(mount_point);
                Ok(())
            }
            UDisks2Message::Unmounted(idx) => {
                let device = &mut self.devices[idx];
                device.state = DeviceState::Unmounted;
                self.state_msg = Some(format!("Unmounted {}", device.name));
                Ok(())
            }
            UDisks2Message::Locked(idx) => {
                let device = &mut self.devices[idx];
                device.state = DeviceState::Locked;
                self.state_msg = Some(format!("Locked {}", device.name));
                Ok(())
            }
            UDisks2Message::UnmountedAndLocked(idx) => {
                let device = &mut self.devices[idx];
                device.state = DeviceState::Locked;
                self.state_msg = Some(format!("Unmounted and locked {}", device.name));
                Ok(())
            }
            UDisks2Message::UnlockedAndMounted(idx, mount_point) => {
                let device = &mut self.devices[idx];
                device.state = DeviceState::Mounted;
                self.state_msg = Some(format!(
                    "Unlocked and mounted {} at {}",
                    device.name, mount_point
                ));
                self.exit_mount_point = Some(mount_point);
                Ok(())
            }
            UDisks2Message::AlreadyMounted(idx, mount_point) => {
                let device = &self.devices[idx];
                self.state_msg = Some(format!(
                    "Already mounted {} at {}",
                    device.name, mount_point
                ));
                self.exit_mount_point = Some(mount_point);
                Ok(())
            }
            UDisks2Message::AlreadyUnmounted(idx) => {
                let device = &self.devices[idx];
                self.state_msg = Some(format!("Already unmounted {}", device.name));
                Ok(())
            }
            UDisks2Message::Err(error_msg) => {
                self.state_msg = Some(format!("Error: {}", error_msg));
                Ok(())
            }
        }
    }

    fn mount(&mut self) -> Result<()> {
        if self.devices.is_empty() {
            return Ok(());
        }

        let device = &self.devices[self.selected_device_index];
        let msg = if let Some(passphrase) = self.passphrase.take() {
            TuiMessage::UnlockAndMount(self.selected_device_index, passphrase)
        } else {
            match device.state {
                DeviceState::Locked => {
                    self.reading_passphrase = true;
                    return Ok(());
                }
                _ => TuiMessage::Mount(self.selected_device_index),
            }
        };

        self.sender.blocking_send(msg)?;

        self.state_msg = Some(format!("Mounting {}...", device.name));

        Ok(())
    }

    fn unmount(&mut self) -> Result<()> {
        if self.devices.is_empty() {
            return Ok(());
        }

        self.sender
            .blocking_send(TuiMessage::Unmount(self.selected_device_index))?;

        self.state_msg = Some(format!(
            "Unmounting {}...",
            &self.devices[self.selected_device_index].name
        ));
        Ok(())
    }
}

impl Widget for &App {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Fill(1),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(area);

        let header = Row::new(
            ["Name", "Label", "Size", "Status"]
                .into_iter()
                .map(Cell::from),
        )
        .blue();
        let mut devices_rows: Vec<Row> = self
            .devices
            .iter()
            .map(|d| {
                Row::new([
                    Cell::new(d.name.as_str()),
                    Cell::new(d.label.as_str()),
                    Cell::new(d.size.to_string()),
                    Cell::new(d.state.to_string()),
                ])
            })
            .collect();
        let mut rows = vec![Row::new([Cell::default(); 0])];
        rows.append(&mut devices_rows);
        let widths = [
            Constraint::Fill(3),
            Constraint::Fill(3),
            Constraint::Fill(1),
            Constraint::Fill(2),
        ];
        let mut state = TableState::new().with_selected(self.selected_device_index + 1);
        StatefulWidget::render(
            Table::new(rows, widths)
                .header(header)
                .highlight_style(Style::new().blue().add_modifier(Modifier::REVERSED)),
            layout[0],
            buf,
            &mut state,
        );

        if let Some(msg) = self.state_msg.as_deref() {
            Paragraph::new(msg)
                .block(Block::default().borders(Borders::ALL))
                .render(layout[1], buf);
        }
        Block::new()
            .title(
                Title::from(Line::from(vec![
                    "m".bold().blue(),
                    " Mount".into(),
                    " | ".dark_gray(),
                    "u".bold().blue(),
                    " Unmount".into(),
                    " | ".dark_gray(),
                    "<Enter>".bold().blue(),
                    " Mount and exit printing mount point".into(),
                    " | ".dark_gray(),
                    "q".bold().blue(),
                    " Quit".into(),
                ]))
                .alignment(Alignment::Center),
            )
            .render(layout[2], buf);

        if self.reading_passphrase {
            let popup_layout = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Fill(1),
                    Constraint::Length(46),
                    Constraint::Fill(1),
                ])
                .split(area);
            let popup_layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Fill(1),
                    Constraint::Length(4),
                    Constraint::Fill(2),
                ])
                .split(popup_layout[1]);
            Clear.render(popup_layout[1], buf);
            Block::new()
                .title(" Enter passphrase for unlocking device ")
                .title_alignment(Alignment::Center)
                .bold()
                .borders(Borders::ALL)
                .border_set(border::THICK)
                .render(popup_layout[1], buf);
        }
    }
}

impl Display for DeviceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            DeviceState::Locked => "Locked",
            DeviceState::UnmountedUnlocked => "Unlocked, unmounted",
            DeviceState::Mounted => "Mounted",
            DeviceState::Unmounted => "Unmounted",
        };
        write!(f, "{}", s)
    }
}
