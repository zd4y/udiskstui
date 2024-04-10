use std::{
    borrow::Cow,
    ffi::{CStr, CString},
};

use color_eyre::Result;
use humansize::{format_size, DECIMAL};

use crate::{
    app::{GuiDeviceInfo, Message},
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

impl Device {
    pub async fn new(client: &Client, block_device: BlockDevice) -> Result<Self> {
        let client = client.clone();
        Ok(Self {
            client,
            block_device,
        })
    }

    pub async fn mount(&self, idx: usize, passphrase: Option<String>) -> Result<Message> {
        let object_path = if let BlockDeviceKind::Encrypted = self.block_device.kind {
            let proxy = EncryptedProxy::builder(self.client.conn())
                .path(&self.block_device.path)?
                .build()
                .await?;
            let cleartext_device = proxy.cleartext_device().await?;
            if cleartext_device.len() > 1 {
                Cow::Owned(cleartext_device)
            } else {
                let passphrase = match passphrase {
                    Some(p) => p,
                    None => return Ok(Message::PassphraseRequired(idx)),
                };
                let cleartext_device = proxy.unlock(&passphrase, Default::default()).await?;
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
                return Ok(Message::UnlockedAndMounted(
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
            Ok(Message::AlreadyMounted(idx, mount_point))
        } else {
            let mount_point = proxy.mount(Default::default()).await?;
            Ok(Message::Mounted(idx, mount_point))
        }
    }

    pub async fn unmount(&self, idx: usize) -> Result<Message> {
        match self.block_device.kind {
            BlockDeviceKind::Filesystem => {
                let proxy = FilesystemProxy::builder(self.client.conn())
                    .path(&self.block_device.path)?
                    .build()
                    .await?;
                if proxy.mount_points().await?.is_empty() {
                    Ok(Message::AlreadyUnmounted(idx))
                } else {
                    proxy.unmount(Default::default()).await?;
                    Ok(Message::Unmounted(idx))
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
                        return Ok(Message::Locked(idx));
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
                    Ok(Message::UnmountedAndLocked(idx, info))
                } else {
                    Ok(Message::AlreadyLocked(idx))
                }
            }
        }
    }

    pub async fn eject(&self, idx: usize) -> Result<Message> {
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
        Ok(Message::Ejected(idx))
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
