use std::{
    borrow::Cow,
    collections::VecDeque,
    ffi::CStr,
    future::Future,
    sync::{mpsc, Arc},
};

use color_eyre::{eyre::Context, Result};
use secrecy::SecretString;
use tokio::{runtime::Runtime, sync::oneshot, task::JoinHandle};

use crate::{
    device::{Device, DeviceMessage, DeviceState},
    udisks2::{BlockDevice, BlockDeviceKind, BlockProxy, Client, EncryptedProxy, FilesystemProxy},
    AgentMessage,
};

pub struct App {
    pub client: Client,
    pub devices: Arc<[Device]>,
    pub gui_devices: Box<[GuiDevice]>,
    pub selected_device_index: usize,
    pub state: AppState,
    pub pending_state: VecDeque<AppState>,
    pub state_msg: Option<String>,
    pub exit: bool,
    pub exit_after_passphrase: bool,
    pub exit_mount_point: Option<String>,
    pub print_on_exit: bool,
    pub runtime: Runtime,
    pub tasks: VecDeque<JoinHandle<Result<DeviceMessage>>>,
    pub agent_receiver: mpsc::Receiver<AgentMessage>,
}

#[derive(Debug)]
pub struct GuiDevice {
    pub info: GuiDeviceInfo,
    pub state: DeviceState,
}

#[derive(Debug)]
pub struct GuiDeviceInfo {
    pub name: String,
    pub label: String,
    pub size: String,
    pub mount_point: String,
}

pub enum AppState {
    DisksList,
    ReadingPassphrase(String),
    ReadingAgentPassword {
        name: String,
        password: String,
        respond_to: Option<oneshot::Sender<SecretString>>,
    },
}

impl App {
    pub fn new(agent_receiver: mpsc::Receiver<AgentMessage>) -> Result<Self> {
        let runtime = Runtime::new()?;
        let client = runtime.block_on(Client::new())?;
        let mut app = Self {
            client,
            gui_devices: Box::new([]),
            devices: Arc::new([]),
            selected_device_index: 0,
            state: AppState::DisksList,
            pending_state: VecDeque::new(),
            state_msg: None,
            exit: false,
            exit_after_passphrase: false,
            exit_mount_point: None,
            print_on_exit: false,
            runtime,
            tasks: VecDeque::new(),
            agent_receiver,
        };
        app.get_or_refresh_devices();
        Ok(app)
    }

    pub fn tick(&mut self) -> Result<()> {
        self.check_finished_tasks()?;
        self.handle_agent_messages()
            .wrap_err("failed handling agent messages")?;

        if let AppState::DisksList = self.state {
            if let Some(state) = self.pending_state.pop_front() {
                self.state = state;
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

    fn handle_agent_messages(&mut self) -> Result<()> {
        let Ok(msg) = self.agent_receiver.try_recv() else {
            return Ok(());
        };

        match msg {
            AgentMessage::ChooseUser { users, respond_to } => {
                // FIXME: this should ask the user...
                respond_to.send(Some((users[0].clone(), 0))).unwrap();
            }
            AgentMessage::RequestPassword { name, respond_to } => {
                self.add_next_state(AppState::ReadingAgentPassword {
                    name,
                    password: "".to_string(),
                    respond_to: Some(respond_to),
                });
            }
        }

        Ok(())
    }

    pub fn exit(&mut self) {
        self.exit = true;
    }

    pub fn next_device(&mut self) {
        if self.gui_devices.is_empty() {
            return;
        }

        if self.gui_devices.len() - 1 > self.selected_device_index {
            self.selected_device_index += 1;
        }
    }

    pub fn prev_device(&mut self) {
        if self.selected_device_index > 0 {
            self.selected_device_index -= 1;
        }
    }

    pub fn last_device(&mut self) {
        if self.gui_devices.is_empty() {
            return;
        }

        self.selected_device_index = self.gui_devices.len() - 1;
    }

    pub fn first_device(&mut self) {
        self.selected_device_index = 0;
    }

    pub fn handle_message(&mut self, msg: DeviceMessage) -> Result<()> {
        match msg {
            DeviceMessage::Devices(gui_devices, devices) => {
                self.gui_devices = gui_devices.into();
                self.devices = devices.into();
                self.selected_device_index = 0;
                self.exit_mount_point = None;
                self.print_on_exit = false;
                Ok(())
            }
            DeviceMessage::Mounted(idx, mount_point) => {
                let device = &mut self.gui_devices[idx];
                device.state = DeviceState::Mounted;
                device.info.mount_point = mount_point.clone();
                self.state_msg = Some(format!("Mounted {} at {}", device.info.name, mount_point));
                self.exit_mount_point = Some(mount_point);
                Ok(())
            }
            DeviceMessage::Unmounted(idx) => {
                let device = &mut self.gui_devices[idx];
                device.state = DeviceState::Unmounted;
                device.info.mount_point = String::new();
                self.state_msg = Some(format!("Unmounted {}", device.info.name));
                Ok(())
            }
            DeviceMessage::Locked(idx) => {
                let device = &mut self.gui_devices[idx];
                device.state = DeviceState::Locked;
                device.info.mount_point = String::new();
                self.state_msg = Some(format!("Locked {}", device.info.name));
                Ok(())
            }
            DeviceMessage::UnmountedAndLocked(idx, device_info) => {
                let device = &mut self.gui_devices[idx];
                device.info = device_info;
                device.state = DeviceState::Locked;
                self.state_msg = Some(format!("Unmounted and locked {}", device.info.name));
                Ok(())
            }
            DeviceMessage::UnlockedAndMounted(idx, mount_point, device_info) => {
                let device = &mut self.gui_devices[idx];
                device.info = device_info;
                device.state = DeviceState::Mounted;
                self.state_msg = Some(format!(
                    "Unlocked and mounted {} at {}",
                    device.info.name, mount_point
                ));
                self.exit_mount_point = Some(mount_point);
                Ok(())
            }
            DeviceMessage::AlreadyMounted(idx, mount_point) => {
                let device = &mut self.gui_devices[idx];
                device.state = DeviceState::Mounted;
                device.info.mount_point = mount_point.clone();
                self.state_msg = Some(format!(
                    "Already mounted {} at {}",
                    device.info.name, mount_point
                ));
                self.exit_mount_point = Some(mount_point);
                Ok(())
            }
            DeviceMessage::AlreadyUnmounted(idx) => {
                let device = &mut self.gui_devices[idx];
                device.state = DeviceState::Unmounted;
                device.info.mount_point = String::new();
                self.state_msg = Some(format!("Already unmounted {}", device.info.name));
                Ok(())
            }
            DeviceMessage::AlreadyLocked(idx) => {
                let device = &mut self.gui_devices[idx];
                device.state = DeviceState::Locked;
                device.info.mount_point = String::new();
                self.state_msg = Some(format!("Already unmounted and locked {}", device.info.name));
                Ok(())
            }
            DeviceMessage::PassphraseRequired(idx) => {
                self.add_next_state(AppState::ReadingPassphrase("".to_string()));
                self.selected_device_index = idx;
                if self.exit {
                    self.exit_after_passphrase = true;
                }
                self.exit = false;
                Ok(())
            }
            DeviceMessage::Ejected(idx) => {
                self.refresh()?;
                self.state_msg = Some(format!("Ejected {}", self.gui_devices[idx].info.name));
                Ok(())
            }
        }
    }

    pub fn mount(&mut self, passphrase: Option<SecretString>) -> Result<()> {
        if self.devices.is_empty() {
            return Ok(());
        }

        let idx = self.selected_device_index;
        let devices = Arc::clone(&self.devices);
        self.spawn(async move {
            let device = &devices[idx];
            let msg = device.mount(idx, passphrase).await?;
            Ok(msg)
        });

        self.state_msg = Some(format!("Mounting {}...", self.gui_devices[idx].info.name));
        Ok(())
    }

    pub fn unmount(&mut self) -> Result<()> {
        if self.devices.is_empty() {
            return Ok(());
        }

        let idx = self.selected_device_index;
        let devices = Arc::clone(&self.devices);
        self.spawn(async move {
            let device = &devices[idx];
            let msg = device.unmount(idx).await?;
            Ok(msg)
        });

        self.state_msg = Some(format!(
            "Unmounting {}...",
            &self.gui_devices[idx].info.name
        ));
        Ok(())
    }

    pub fn eject(&mut self) -> Result<()> {
        if self.devices.is_empty() {
            return Ok(());
        }

        let idx = self.selected_device_index;
        let devices = Arc::clone(&self.devices);
        self.spawn(async move {
            let device = &devices[idx];
            let msg = device.eject(idx).await?;
            Ok(msg)
        });

        self.state_msg = Some(format!("Ejecting {}...", &self.gui_devices[idx].info.name));
        Ok(())
    }

    pub fn refresh(&mut self) -> Result<()> {
        self.selected_device_index = 0;
        self.state = AppState::DisksList;
        self.pending_state = VecDeque::new();
        self.state_msg = None;
        self.exit = false;
        self.exit_after_passphrase = false;
        self.exit_mount_point = None;
        self.print_on_exit = false;
        self.get_or_refresh_devices();
        Ok(())
    }

    pub fn add_next_state(&mut self, state: AppState) {
        match self.state {
            AppState::DisksList => {}
            _ => {
                self.pending_state.push_back(state);
                return;
            }
        }
        match self.pending_state.pop_front() {
            Some(pending) => {
                self.state = pending;
                self.pending_state.push_back(state);
            }
            None => {
                self.state = state;
            }
        }
    }

    pub fn get_or_refresh_devices(&mut self) {
        let client = self.client.clone();
        self.spawn(async move {
            let block_devices = client.get_block_devices().await?;
            let mut devices = Vec::with_capacity(block_devices.len());
            let mut gui_devices = Vec::with_capacity(block_devices.len());

            for block_device in block_devices {
                gui_devices.push(GuiDevice::new(&client, &block_device).await?);
                devices.push(Device::new(&client, block_device).await?);
            }

            Ok(DeviceMessage::Devices(gui_devices, devices))
        });
    }

    pub fn spawn<F>(&mut self, task: F)
    where
        F: Future<Output = Result<DeviceMessage>> + Send + 'static,
    {
        self.tasks.push_back(self.runtime.spawn(task));
    }

    fn check_finished_tasks(&mut self) -> Result<()> {
        for _ in 0..self.tasks.len() {
            if let Some(task) = self.tasks.pop_front() {
                if task.is_finished() {
                    match self.runtime.block_on(task)? {
                        Ok(msg) => self.handle_message(msg)?,
                        Err(err) => {
                            self.state_msg = Some(format!("Error: {err}"));
                            self.exit = false;
                        }
                    }
                } else {
                    self.tasks.push_back(task)
                }
            } else {
                break;
            }
        }
        Ok(())
    }
}

impl GuiDevice {
    async fn new(client: &Client, block_device: &BlockDevice) -> Result<Self> {
        let (path, mount_point) = match block_device.kind {
            BlockDeviceKind::Filesystem => {
                let filesystem_proxy = FilesystemProxy::builder(client.conn())
                    .path(&block_device.path)?
                    .build()
                    .await?;
                let mount_point = match filesystem_proxy.mount_points().await?.first() {
                    Some(mount_point) => CStr::from_bytes_with_nul(mount_point)?
                        .to_string_lossy()
                        .to_string(),
                    None => String::new(),
                };
                (Cow::Borrowed(&block_device.path), mount_point)
            }
            BlockDeviceKind::Encrypted => {
                let encrypted_proxy = EncryptedProxy::builder(client.conn())
                    .path(&block_device.path)?
                    .build()
                    .await?;
                let cleartext_device = encrypted_proxy.cleartext_device().await?;
                if cleartext_device.len() > 1 {
                    let filesystem_proxy = FilesystemProxy::builder(client.conn())
                        .path(&cleartext_device)?
                        .build()
                        .await?;
                    let mount_point = match filesystem_proxy.mount_points().await?.first() {
                        Some(mount_point) => CStr::from_bytes_with_nul(mount_point)?
                            .to_string_lossy()
                            .to_string(),
                        None => String::new(),
                    };
                    (Cow::Owned(cleartext_device), mount_point)
                } else {
                    (Cow::Borrowed(&block_device.path), String::new())
                }
            }
        };
        let proxy = BlockProxy::builder(client.conn())
            .path(path.as_ref())?
            .build()
            .await?;
        let name = Device::get_name(&proxy).await?;
        let label = Device::get_label(&proxy).await?;
        let size = Device::get_size(&proxy).await?;
        let state = Device::get_state(client, block_device).await?;
        Ok(Self {
            info: GuiDeviceInfo {
                name,
                label,
                size,
                mount_point,
            },
            state,
        })
    }
}
