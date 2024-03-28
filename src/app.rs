use std::{collections::VecDeque, fmt::Display, future::Future, sync::Arc, time::Duration};

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
use tokio::{
    runtime::Runtime,
    sync::{
        mpsc::{self, error::TryRecvError, Receiver, Sender},
        RwLock,
    },
    task::JoinHandle,
};

use crate::{
    device::{Device, DeviceState},
    tui,
    udisks2::{BlockDevice, BlockDeviceKind, BlockProxy, Client, EncryptedProxy},
};

pub struct App {
    client: Client,
    devices: Arc<RwLock<Vec<Device>>>,
    gui_devices: Vec<GuiDevice>,
    selected_device_index: usize,
    passphrase: Option<String>,
    reading_passphrase: bool,
    state_msg: Option<String>,
    exit: bool,
    exit_mount_point: Option<String>,
    print_on_exit: bool,
    sender: Sender<Message>,
    receiver: Receiver<Message>,
    runtime: Runtime,
    tasks: VecDeque<JoinHandle<Result<()>>>,
}

#[derive(Debug)]
pub struct GuiDevice {
    name: String,
    label: String,
    size: String,
    state: DeviceState,
}

pub enum Message {
    Mounted(usize, String),
    Unmounted(usize),
    Locked(usize),
    UnmountedAndLocked(usize, String, String, String),
    UnlockedAndMounted(usize, String, String, String, String),
    AlreadyMounted(usize, String),
    AlreadyUnmounted(usize),
    Devices(Vec<GuiDevice>),
}

impl App {
    pub fn new() -> Result<Self> {
        let (sender, receiver) = mpsc::channel(10);
        let runtime = Runtime::new()?;
        let client = runtime.block_on(Client::new())?;
        let mut app = Self {
            client,
            gui_devices: Vec::new(),
            devices: Arc::new(RwLock::new(Vec::new())),
            selected_device_index: 0,
            passphrase: None,
            reading_passphrase: false,
            state_msg: None,
            exit: false,
            exit_mount_point: None,
            print_on_exit: false,
            sender,
            receiver,
            runtime,
            tasks: VecDeque::new(),
        };
        app.get_or_refresh_devices();
        Ok(app)
    }

    pub fn run(&mut self, terminal: &mut tui::Tui) -> Result<()> {
        while !self.exit {
            terminal.draw(|frame| self.render_frame(frame))?;
            self.receive_udisks_messages()?;
            self.check_finished_tasks()?;
            self.handle_events().wrap_err("handling events failed")?;
        }
        terminal.draw(|frame| {
            frame.render_widget(
                Paragraph::new(self.state_msg.as_deref().unwrap_or("exiting...")),
                frame.size(),
            )
        })?;

        while let Some(task) = self.tasks.pop_front() {
            match self.runtime.block_on(task)? {
                Ok(()) => {}
                Err(err) => {
                    self.state_msg = Some(format!("Error: {err}"));
                    self.exit = false;
                    return self.run(terminal);
                }
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
            KeyCode::Char('r') => self.refresh()?,
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
        if self.gui_devices.is_empty() {
            return;
        }

        if self.gui_devices.len() - 1 > self.selected_device_index {
            self.selected_device_index += 1;
        }
    }

    fn prev_device(&mut self) {
        if self.selected_device_index > 0 {
            self.selected_device_index -= 1;
        }
    }

    fn last_device(&mut self) {
        if self.gui_devices.is_empty() {
            return;
        }

        self.selected_device_index = self.gui_devices.len() - 1;
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

    fn handle_udisks_message(&mut self, msg: Message) -> Result<()> {
        match msg {
            Message::Devices(devices) => {
                self.gui_devices = devices;
                self.selected_device_index = 0;
                self.state_msg = None;
                self.exit_mount_point = None;
                self.print_on_exit = false;
                Ok(())
            }
            Message::Mounted(idx, mount_point) => {
                let device = &mut self.gui_devices[idx];
                device.state = DeviceState::Mounted;
                self.state_msg = Some(format!("Mounted {} at {}", device.name, mount_point));
                self.exit_mount_point = Some(mount_point);
                Ok(())
            }
            Message::Unmounted(idx) => {
                let device = &mut self.gui_devices[idx];
                device.state = DeviceState::Unmounted;
                self.state_msg = Some(format!("Unmounted {}", device.name));
                Ok(())
            }
            Message::Locked(idx) => {
                let device = &mut self.gui_devices[idx];
                device.state = DeviceState::Locked;
                self.state_msg = Some(format!("Locked {}", device.name));
                Ok(())
            }
            Message::UnmountedAndLocked(idx, name, label, size) => {
                let device = &mut self.gui_devices[idx];
                device.name = name;
                device.label = label;
                device.size = size;
                device.state = DeviceState::Locked;
                self.state_msg = Some(format!("Unmounted and locked {}", device.name));
                Ok(())
            }
            Message::UnlockedAndMounted(idx, mount_point, name, label, size) => {
                let device = &mut self.gui_devices[idx];
                device.name = name;
                device.label = label;
                device.size = size;
                device.state = DeviceState::Mounted;
                self.state_msg = Some(format!(
                    "Unlocked and mounted {} at {}",
                    device.name, mount_point
                ));
                self.exit_mount_point = Some(mount_point);
                Ok(())
            }
            Message::AlreadyMounted(idx, mount_point) => {
                let device = &self.gui_devices[idx];
                self.state_msg = Some(format!(
                    "Already mounted {} at {}",
                    device.name, mount_point
                ));
                self.exit_mount_point = Some(mount_point);
                Ok(())
            }
            Message::AlreadyUnmounted(idx) => {
                let device = &self.gui_devices[idx];
                self.state_msg = Some(format!("Already unmounted {}", device.name));
                Ok(())
            }
        }
    }

    fn mount(&mut self) -> Result<()> {
        let idx = self.selected_device_index;
        let devices_lock = self.devices.clone();
        let sender = self.sender.clone();
        let passphrase = self.passphrase.take();
        self.spawn(async move {
            let mut devices = devices_lock.write().await;
            if let Some(device) = devices.get_mut(idx) {
                let msg = device.mount(idx, passphrase).await?;
                sender.send(msg).await?;
            }
            Ok(())
        });

        self.state_msg = Some(format!("Mounting {}...", self.gui_devices[idx].name));

        Ok(())
    }

    fn unmount(&mut self) -> Result<()> {
        let idx = self.selected_device_index;
        let devices_lock = self.devices.clone();
        let sender = self.sender.clone();
        self.spawn(async move {
            let mut devices = devices_lock.write().await;
            if let Some(device) = devices.get_mut(idx) {
                let msg = device.unmount(idx).await?;
                sender.send(msg).await?;
            }
            Ok(())
        });

        self.state_msg = Some(format!(
            "Unmounting {}...",
            &self.gui_devices[self.selected_device_index].name
        ));
        Ok(())
    }

    fn refresh(&mut self) -> Result<()> {
        self.selected_device_index = 0;
        self.passphrase = None;
        self.reading_passphrase = false;
        self.state_msg = None;
        self.exit = false;
        self.exit_mount_point = None;
        self.print_on_exit = false;
        self.get_or_refresh_devices();
        Ok(())
    }

    fn get_or_refresh_devices(&mut self) {
        let client = self.client.clone();
        let devices_lock = Arc::clone(&self.devices);
        let sender = self.sender.clone();
        self.spawn(async move {
            let block_devices = client.get_block_devices().await?;
            let mut new_devices = Vec::with_capacity(block_devices.len());
            let mut gui_devices = Vec::with_capacity(block_devices.len());

            for block_device in block_devices {
                gui_devices.push(GuiDevice::new(&client, &block_device).await?);
                let device = Device::new(&client, block_device).await?;
                new_devices.push(device);
            }

            let mut devices = devices_lock.write().await;
            devices.clear();
            *devices = new_devices;
            sender.send(Message::Devices(gui_devices)).await?;

            Ok(())
        });
    }

    fn spawn<F>(&mut self, task: F)
    where
        F: Future<Output = Result<()>> + Send + 'static,
    {
        self.tasks.push_back(self.runtime.spawn(task));
    }

    fn check_finished_tasks(&mut self) -> Result<()> {
        let l = self.tasks.len();
        let mut i = 0;
        while let Some(task) = self.tasks.pop_front() {
            if task.is_finished() {
                match self.runtime.block_on(task)? {
                    Ok(()) => {}
                    Err(err) => {
                        self.state_msg = Some(format!("Error: {err}"));
                        self.exit = false;
                    }
                }
            } else {
                self.tasks.push_back(task)
            }
            i += 1;
            if i >= l {
                break;
            }
        }
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
            .gui_devices
            .iter()
            .map(|d| {
                Row::new([
                    Cell::new(d.name.as_str()),
                    Cell::new(d.label.as_str()),
                    Cell::new(d.size.as_str()),
                    Cell::new(d.state.to_string()),
                ])
            })
            .collect();
        let mut rows = vec![Row::new([Cell::default(); 0])];
        rows.append(&mut devices_rows);
        let widths = [
            Constraint::Fill(3),
            Constraint::Fill(3),
            Constraint::Max(10),
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
                    "r".bold().blue(),
                    " Refresh".into(),
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

impl GuiDevice {
    async fn new(client: &Client, block_device: &BlockDevice) -> Result<Self> {
        let path = match block_device.kind {
            BlockDeviceKind::Filesystem => block_device.path.clone(),
            BlockDeviceKind::Encrypted => {
                let encrypted_proxy = EncryptedProxy::builder(client.conn())
                    .path(&block_device.path)?
                    .build()
                    .await?;
                let cleartext_device = encrypted_proxy.cleartext_device().await?;
                if cleartext_device.len() > 1 {
                    cleartext_device
                } else {
                    block_device.path.clone()
                }
            }
        };
        let proxy = BlockProxy::builder(client.conn())
            .path(path)?
            .build()
            .await?;
        let name = Device::get_name(&proxy).await?;
        let label = Device::get_label(&proxy).await?;
        let size = Device::get_size(&proxy).await?;
        let state = Device::get_state(client, block_device).await?;
        Ok(Self {
            name,
            label,
            size,
            state,
        })
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
