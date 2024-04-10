use std::io::Cursor;

use color_eyre::Result;

use zbus::{proxy, Connection};
use zbus_xml::Node;
use zvariant::{ObjectPath, OwnedObjectPath};

#[derive(Debug, Clone)]
pub struct Client {
    connection: Connection,
}

impl Client {
    pub async fn new() -> zbus::Result<Self> {
        let connection = zbus::Connection::system().await?;
        Ok(Client { connection })
    }

    pub fn conn(&self) -> &Connection {
        &self.connection
    }

    pub async fn get_block_devices(&self) -> Result<Vec<BlockDevice>> {
        let manager_proxy = ManagerProxy::new(&self.connection).await?;
        let resp = manager_proxy.get_block_devices(Default::default()).await?;
        let mut devices = Vec::new();
        for path in resp {
            let kind = match self.block_device_kind(&path).await? {
                Some(kind) => kind,
                None => continue,
            };
            devices.push(BlockDevice { path, kind });
        }

        Ok(devices)
    }

    async fn block_device_kind(
        &self,
        object_path: &ObjectPath<'_>,
    ) -> Result<Option<BlockDeviceKind>> {
        let proxy = BlockProxy::builder(&self.connection)
            .path(object_path)?
            .build()
            .await?;
        if proxy.hint_ignore().await? {
            return Ok(None);
        }
        if proxy.crypto_backing_device().await?.len() > 1 {
            return Ok(None);
        }

        let xml_descriptor = proxy.inner().introspect().await?;
        let r = Cursor::new(xml_descriptor);
        let node = Node::from_reader(r)?;
        let interfaces = node.interfaces();

        for interface in interfaces {
            match interface.name().as_str() {
                "org.freedesktop.UDisks2.Filesystem" => {
                    return Ok(Some(BlockDeviceKind::Filesystem));
                }
                "org.freedesktop.UDisks2.Encrypted" => {
                    return Ok(Some(BlockDeviceKind::Encrypted));
                }
                _ => {}
            }
        }
        Ok(None)
    }
}

#[derive(Debug, Clone)]
pub struct BlockDevice {
    pub path: OwnedObjectPath,
    pub kind: BlockDeviceKind,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum BlockDeviceKind {
    Filesystem,
    Encrypted,
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
    fn drive(&self) -> zbus::Result<OwnedObjectPath>;

    #[zbus(property)]
    fn device(&self) -> zbus::Result<Vec<u8>>;

    #[zbus(property)]
    fn id_label(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn size(&self) -> zbus::Result<u64>;

    #[zbus(property)]
    fn crypto_backing_device(&self) -> zbus::Result<OwnedObjectPath>;
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

#[proxy(
    interface = "org.freedesktop.UDisks2.Drive",
    default_service = "org.freedesktop.UDisks2"
)]
trait Drive {
    fn eject(
        &self,
        options: std::collections::HashMap<&str, &zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<()>;
}
