use std::{
    borrow::Cow,
    ffi::{CStr, CString},
    fmt::Display,
};

use color_eyre::Result;
use humansize::{format_size, DECIMAL};
use secrecy::{zeroize::Zeroize, ExposeSecret, SecretString};

use crate::{
    app::{GuiDevice, GuiDeviceInfo},
    udisks2::{
        BlockDevice, BlockDeviceKind, BlockProxy, Client, DriveProxy, EncryptedProxy,
        FilesystemProxy,
    },
};

#[derive(Debug, Clone)]
pub struct Device {
    client: Client,
    block_device: BlockDevice,
}

#[derive(Debug)]
pub enum DeviceState {
    Locked,
    UnmountedUnlocked,
    Mounted,
    Unmounted,
}

pub enum DeviceMessage {
    Mounted(usize, String),
    Unmounted(usize),
    Locked(usize),
    UnmountedAndLocked(usize, GuiDeviceInfo),
    UnlockedAndMounted(usize, String, GuiDeviceInfo),
    AlreadyMounted(usize, String),
    AlreadyUnmounted(usize),
    AlreadyLocked(usize),
    Devices(Vec<GuiDevice>, Vec<Device>),
    PassphraseRequired(usize),
    Ejected(usize),
}

impl Device {
    pub async fn new(client: &Client, block_device: BlockDevice) -> Result<Self> {
        let client = client.clone();
        Ok(Self {
            client,
            block_device,
        })
    }

    pub async fn mount(
        &self,
        idx: usize,
        passphrase: Option<SecretString>,
    ) -> Result<DeviceMessage> {
        let object_path = if let BlockDeviceKind::Encrypted = self.block_device.kind {
            let proxy = EncryptedProxy::builder(self.client.conn())
                .path(&self.block_device.path)?
                .build()
                .await?;
            let cleartext_device = proxy.cleartext_device().await?;
            if cleartext_device.len() > 1 {
                Cow::Owned(cleartext_device)
            } else {
                let mut passphrase = match passphrase {
                    Some(p) => p,
                    None => return Ok(DeviceMessage::PassphraseRequired(idx)),
                };
                let cleartext_device = proxy
                    .unlock(passphrase.expose_secret(), Default::default())
                    .await?;
                passphrase.zeroize();
                let proxy = FilesystemProxy::builder(self.client.conn())
                    .path(&cleartext_device)?
                    .build()
                    .await?;
                let mount_point = proxy.mount(Default::default()).await?;

                let proxy = BlockProxy::builder(self.client.conn())
                    .path(cleartext_device)?
                    .build()
                    .await?;
                let name = Self::get_name(&proxy).await?;
                let label = Self::get_label(&proxy).await?;
                let size = Self::get_size(&proxy).await?;
                return Ok(DeviceMessage::UnlockedAndMounted(
                    idx,
                    mount_point.clone(),
                    GuiDeviceInfo {
                        name,
                        label,
                        size,
                        mount_point,
                    },
                ));
            }
        } else {
            Cow::Borrowed(&self.block_device.path)
        };

        let proxy = FilesystemProxy::builder(self.client.conn())
            .path(object_path.as_ref())?
            .build()
            .await?;
        if let Some(mount_point) = proxy.mount_points().await?.first() {
            let mount_point = CStr::from_bytes_with_nul(mount_point)?
                .to_string_lossy()
                .to_string();
            Ok(DeviceMessage::AlreadyMounted(idx, mount_point))
        } else {
            let mount_point = proxy.mount(Default::default()).await?;
            Ok(DeviceMessage::Mounted(idx, mount_point))
        }
    }

    pub async fn unmount(&self, idx: usize) -> Result<DeviceMessage> {
        match self.block_device.kind {
            BlockDeviceKind::Filesystem => {
                let proxy = FilesystemProxy::builder(self.client.conn())
                    .path(&self.block_device.path)?
                    .build()
                    .await?;
                if proxy.mount_points().await?.is_empty() {
                    Ok(DeviceMessage::AlreadyUnmounted(idx))
                } else {
                    proxy.unmount(Default::default()).await?;
                    Ok(DeviceMessage::Unmounted(idx))
                }
            }
            BlockDeviceKind::Encrypted => {
                let proxy = EncryptedProxy::builder(self.client.conn())
                    .path(&self.block_device.path)?
                    .build()
                    .await?;
                let cleartext_device = proxy.cleartext_device().await?;
                if cleartext_device.len() > 1 {
                    let filesystem_proxy = FilesystemProxy::builder(self.client.conn())
                        .path(cleartext_device)?
                        .build()
                        .await?;
                    if filesystem_proxy.mount_points().await?.is_empty() {
                        proxy.lock(Default::default()).await?;
                        return Ok(DeviceMessage::Locked(idx));
                    }
                    filesystem_proxy.unmount(Default::default()).await?;
                    proxy.lock(Default::default()).await?;

                    let proxy = BlockProxy::builder(self.client.conn())
                        .path(&self.block_device.path)?
                        .build()
                        .await?;
                    let name = Self::get_name(&proxy).await?;
                    let label = Self::get_label(&proxy).await?;
                    let size = Self::get_size(&proxy).await?;
                    let info = GuiDeviceInfo {
                        name,
                        label,
                        size,
                        mount_point: String::new(),
                    };
                    Ok(DeviceMessage::UnmountedAndLocked(idx, info))
                } else {
                    Ok(DeviceMessage::AlreadyLocked(idx))
                }
            }
        }
    }

    pub async fn eject(&self, idx: usize) -> Result<DeviceMessage> {
        let proxy = BlockProxy::builder(self.client.conn())
            .path(&self.block_device.path)?
            .build()
            .await?;
        let drive = proxy.drive().await?;
        let proxy = DriveProxy::builder(self.client.conn())
            .path(drive)?
            .build()
            .await?;
        proxy.eject(Default::default()).await?;
        Ok(DeviceMessage::Ejected(idx))
    }

    pub async fn get_name(proxy: &BlockProxy<'_>) -> Result<String> {
        let p = proxy.device().await?;
        Ok(CString::from_vec_with_nul(p)?.to_string_lossy().to_string())
    }

    pub async fn get_label(proxy: &BlockProxy<'_>) -> Result<String> {
        Ok(proxy.id_label().await?)
    }

    pub async fn get_size(proxy: &BlockProxy<'_>) -> Result<String> {
        let size = proxy.size().await?;
        Ok(format_size(size, DECIMAL))
    }

    pub async fn get_state(client: &Client, block_device: &BlockDevice) -> Result<DeviceState> {
        match block_device.kind {
            BlockDeviceKind::Filesystem => {
                let proxy = FilesystemProxy::builder(client.conn())
                    .path(&block_device.path)?
                    .build()
                    .await?;
                if proxy.mount_points().await?.is_empty() {
                    Ok(DeviceState::Unmounted)
                } else {
                    Ok(DeviceState::Mounted)
                }
            }
            BlockDeviceKind::Encrypted => {
                let proxy = EncryptedProxy::builder(client.conn())
                    .path(&block_device.path)?
                    .build()
                    .await?;
                let cleartext_device = proxy.cleartext_device().await?;
                if cleartext_device.len() > 1 {
                    let proxy = FilesystemProxy::builder(client.conn())
                        .path(cleartext_device)?
                        .build()
                        .await?;
                    if proxy.mount_points().await?.is_empty() {
                        Ok(DeviceState::UnmountedUnlocked)
                    } else {
                        Ok(DeviceState::Mounted)
                    }
                } else {
                    Ok(DeviceState::Locked)
                }
            }
        }
    }
}

impl Display for DeviceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            DeviceState::Locked => "Locked",
            DeviceState::UnmountedUnlocked => "Unlocked",
            DeviceState::Mounted => "Mounted",
            DeviceState::Unmounted => "Unmounted",
        };
        write!(f, "{}", s)
    }
}
