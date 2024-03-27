use std::io::Cursor;

use color_eyre::Result;

use zbus::{proxy, Connection};
use zbus_xml::Node;
use zvariant::{ObjectPath, OwnedObjectPath};

pub struct Client {
    connection: Connection,
}

impl Client {
    pub async fn new() -> zbus::Result<Self> {
        let connection = zbus::Connection::system().await?;
        Ok(Client { connection })
    }

    pub async fn get_block_devices(&self) -> Result<Vec<BlockDevice>> {
        let manager_proxy = ManagerProxy::new(&self.connection).await?;
        let resp = manager_proxy.get_block_devices(Default::default()).await?;
        let mut devices = Vec::new();
        for op in resp {
            let block = op.as_str().to_string();
            let block_proxy = BlockProxy::builder(&self.connection)
                .path(block)?
                .build()
                .await?;

            devices.push(BlockDevice {
                path: op.into_inner(),
                proxy: block_proxy,
                cached_kind: None,
            });
        }

        Ok(devices)
    }

    pub async fn use_block_device<'a>(
        &'a self,
        block_device: &mut BlockDevice<'a>,
    ) -> Result<BlockDeviceUsage> {
        match block_device.kind(self).await? {
            BlockDeviceKind::Filesystem => {
                let proxy = FilesystemProxy::builder(&self.connection)
                    .path(&block_device.path)?
                    .build()
                    .await?;

                let encrypted_proxy = block_device.crypto_backing_device().await?.map(|cbd| {
                    EncryptedProxy::builder(&self.connection)
                        .path(cbd)
                        .map(|x| x.build())
                });

                if let Some(mount_point) = proxy.mount_points().await?.first() {
                    let mount_point = dbus_u8_array_to_str(mount_point)?.to_string();
                    let filesystem = MountedFilesystemBlockDevice { proxy, mount_point };
                    if let Some(encrypted_proxy) = encrypted_proxy {
                        Ok(BlockDeviceUsage::MountedUnlockedEncrypted(
                            MountedUnlockedEncryptedBlockDevice {
                                proxy: encrypted_proxy?.await?,
                                filesystem,
                            },
                        ))
                    } else {
                        Ok(BlockDeviceUsage::MountedFilesystem(filesystem))
                    }
                } else {
                    let filesystem = UnmountedFilesystemBlockDevice { proxy };
                    if let Some(encrypted_proxy) = encrypted_proxy {
                        Ok(BlockDeviceUsage::UnmountedUnlockedEncrypted(
                            UnmountedUnlockedEncryptedBlockDevice {
                                proxy: encrypted_proxy?.await?,
                                filesystem,
                            },
                        ))
                    } else {
                        Ok(BlockDeviceUsage::UnmountedFilesystem(filesystem))
                    }
                }
            }
            BlockDeviceKind::Encrypted => {
                let proxy = EncryptedProxy::builder(&self.connection)
                    .path(&block_device.path)?
                    .build()
                    .await?;
                if proxy.cleartext_device().await?.len() > 1 {
                    Ok(BlockDeviceUsage::Other)
                } else {
                    Ok(BlockDeviceUsage::LockedEncrypted(
                        LockedEncryptedBlockDevice { proxy },
                    ))
                }
            }
            BlockDeviceKind::Other => Ok(BlockDeviceUsage::Other),
        }
    }
}

#[derive(Debug)]
pub struct BlockDevice<'a> {
    path: ObjectPath<'a>,
    proxy: BlockProxy<'a>,
    cached_kind: Option<BlockDeviceKind>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum BlockDeviceKind {
    Filesystem,
    Encrypted,
    Other,
}

#[derive(Debug, Clone)]
pub enum BlockDeviceUsage<'a> {
    MountedFilesystem(MountedFilesystemBlockDevice<'a>),
    UnmountedFilesystem(UnmountedFilesystemBlockDevice<'a>),
    MountedUnlockedEncrypted(MountedUnlockedEncryptedBlockDevice<'a>),
    UnmountedUnlockedEncrypted(UnmountedUnlockedEncryptedBlockDevice<'a>),
    LockedEncrypted(LockedEncryptedBlockDevice<'a>),
    Other,
}

impl<'a> BlockDevice<'a> {
    pub async fn name(&self) -> Result<String> {
        let p = self.proxy.device().await?;
        Ok(dbus_u8_array_to_str(&p)?.to_string())
    }

    pub async fn label(&self) -> Result<String> {
        Ok(self.proxy.id_label().await?)
    }

    pub async fn size(&self) -> Result<u64> {
        Ok(self.proxy.size().await?)
    }

    pub async fn kind(&mut self, client: &Client) -> Result<BlockDeviceKind> {
        if let Some(kind) = self.cached_kind {
            return Ok(kind);
        }

        let xml_descriptor = self.proxy.inner().introspect().await?;
        let r = Cursor::new(xml_descriptor);
        let node = Node::from_reader(r)?;
        let interfaces = node.interfaces();

        let mut kind = BlockDeviceKind::Other;
        for interface in interfaces {
            match interface.name().as_str() {
                "org.freedesktop.UDisks2.Filesystem" => {
                    kind = BlockDeviceKind::Filesystem;
                    break;
                }
                "org.freedesktop.UDisks2.Encrypted" => {
                    let encrypted_proxy = EncryptedProxy::builder(&client.connection)
                        .path(&self.path)?
                        .build()
                        .await?;
                    if encrypted_proxy.cleartext_device().await?.len() > 1 {
                        kind = BlockDeviceKind::Other;
                    } else {
                        kind = BlockDeviceKind::Encrypted;
                    }
                    break;
                }
                _ => {}
            }
        }
        self.cached_kind = Some(kind);
        Ok(kind)
    }

    pub async fn hidden(&self) -> zbus::Result<bool> {
        self.proxy.hint_ignore().await
    }

    async fn crypto_backing_device(&self) -> zbus::Result<Option<OwnedObjectPath>> {
        let cbd = self.proxy.crypto_backing_device().await?;
        if cbd.len() > 1 {
            return Ok(Some(cbd));
        }
        Ok(None)
    }
}

#[derive(Debug, Clone)]
pub struct UnmountedFilesystemBlockDevice<'a> {
    proxy: FilesystemProxy<'a>,
}

#[derive(Debug, Clone)]
pub struct MountedFilesystemBlockDevice<'a> {
    proxy: FilesystemProxy<'a>,
    mount_point: String,
}

#[derive(Debug, Clone)]
pub struct UnmountedUnlockedEncryptedBlockDevice<'a> {
    proxy: EncryptedProxy<'a>,
    filesystem: UnmountedFilesystemBlockDevice<'a>,
}

#[derive(Debug, Clone)]
pub struct MountedUnlockedEncryptedBlockDevice<'a> {
    proxy: EncryptedProxy<'a>,
    filesystem: MountedFilesystemBlockDevice<'a>,
}

impl<'a> UnmountedFilesystemBlockDevice<'a> {
    pub async fn mount(self) -> Result<MountedFilesystemBlockDevice<'a>> {
        let mount_point = self.proxy.mount(Default::default()).await?;
        Ok(MountedFilesystemBlockDevice {
            proxy: self.proxy,
            mount_point,
        })
    }
}

impl<'a> MountedFilesystemBlockDevice<'a> {
    pub fn mount_point(&self) -> &str {
        &self.mount_point
    }

    pub async fn unmount(self) -> Result<UnmountedFilesystemBlockDevice<'a>> {
        self.proxy.unmount(Default::default()).await?;
        Ok(UnmountedFilesystemBlockDevice { proxy: self.proxy })
    }
}

impl<'a> UnmountedUnlockedEncryptedBlockDevice<'a> {
    pub async fn mount(self) -> Result<MountedUnlockedEncryptedBlockDevice<'a>> {
        let filesystem = self.filesystem.mount().await?;
        Ok(MountedUnlockedEncryptedBlockDevice {
            proxy: self.proxy,
            filesystem,
        })
    }

    pub async fn lock(self) -> Result<LockedEncryptedBlockDevice<'a>> {
        self.proxy.lock(Default::default()).await?;
        Ok(LockedEncryptedBlockDevice { proxy: self.proxy })
    }
}

impl<'a> MountedUnlockedEncryptedBlockDevice<'a> {
    pub fn mount_point(&self) -> &str {
        &self.filesystem.mount_point
    }

    pub async fn unmount(self) -> Result<UnmountedUnlockedEncryptedBlockDevice<'a>> {
        let filesystem = self.filesystem.unmount().await?;
        Ok(UnmountedUnlockedEncryptedBlockDevice {
            proxy: self.proxy,
            filesystem,
        })
    }
}

#[derive(Debug, Clone)]
pub struct LockedEncryptedBlockDevice<'a> {
    proxy: EncryptedProxy<'a>,
}

impl<'a> LockedEncryptedBlockDevice<'a> {
    pub async fn unlock(
        self,
        client: &Client,
        passphrase: &str,
    ) -> Result<UnmountedUnlockedEncryptedBlockDevice<'a>> {
        let path = self.proxy.unlock(passphrase, Default::default()).await?;
        let filesystem_proxy = FilesystemProxy::builder(&client.connection)
            .path(path)?
            .build()
            .await?;
        let filesystem = UnmountedFilesystemBlockDevice {
            proxy: filesystem_proxy,
        };
        Ok(UnmountedUnlockedEncryptedBlockDevice {
            proxy: self.proxy,
            filesystem,
        })
    }
}

#[proxy(
    default_service = "org.freedesktop.UDisks2",
    default_path = "/org/freedesktop/UDisks2/Manager",
    interface = "org.freedesktop.UDisks2.Manager"
)]
trait Manager {
    fn get_block_devices(
        &self,
        options: std::collections::HashMap<&str, zvariant::Value<'_>>,
    ) -> zbus::Result<Vec<OwnedObjectPath>>;
}

#[proxy(
    default_service = "org.freedesktop.UDisks2",
    interface = "org.freedesktop.UDisks2.Block"
)]
trait Block {
    #[zbus(property)]
    fn hint_ignore(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn drive(&self) -> zbus::Result<ObjectPath>;

    #[zbus(property)]
    fn device(&self) -> zbus::Result<Vec<u8>>;

    #[zbus(property)]
    fn id_label(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn size(&self) -> zbus::Result<u64>;

    #[zbus(property)]
    fn crypto_backing_device(&self) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
}

#[proxy(
    default_service = "org.freedesktop.UDisks2",
    interface = "org.freedesktop.UDisks2.Filesystem"
)]
trait Filesystem {
    fn mount(
        &self,
        options: std::collections::HashMap<&str, zvariant::Value<'_>>,
    ) -> zbus::Result<String>;

    fn unmount(
        &self,
        options: std::collections::HashMap<&str, zvariant::Value<'_>>,
    ) -> zbus::Result<()>;

    #[zbus(property)]
    fn mount_points(&self) -> zbus::Result<Vec<Vec<u8>>>;
}

#[proxy(
    default_service = "org.freedesktop.UDisks2",
    interface = "org.freedesktop.UDisks2.Encrypted"
)]
trait Encrypted {
    fn lock(
        &self,
        options: std::collections::HashMap<&str, zvariant::Value<'_>>,
    ) -> zbus::Result<()>;

    fn unlock(
        &self,
        passphrase: &str,
        options: std::collections::HashMap<&str, zvariant::Value<'_>>,
    ) -> zbus::Result<OwnedObjectPath>;

    #[zbus(property)]
    fn cleartext_device(&self) -> zbus::Result<OwnedObjectPath>;
}

fn dbus_u8_array_to_str(s: &[u8]) -> Result<&str, std::str::Utf8Error> {
    std::str::from_utf8(&s[..s.len() - 1])
}
