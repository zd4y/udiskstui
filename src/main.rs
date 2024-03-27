use std::future::Future;

use app::{App, Device, DeviceState, TuiMessage, UDisks2Message};
use color_eyre::Result;
use humansize::DECIMAL;
use tokio::sync::mpsc::{self, error::SendError, Receiver, Sender};
use udisks2::{BlockDevice, BlockDeviceUsage, Client};

mod app;
mod errors;
mod tui;
mod udisks2;

#[tokio::main]
async fn main() -> Result<()> {
    errors::install_hooks()?;

    let (tui_sender, tui_receiver) = mpsc::channel::<TuiMessage>(100);
    let (udisks_sender, udisks_receiver) = mpsc::channel::<UDisks2Message>(100);

    let udisks_handle = tokio::spawn(async move { udisks_task(udisks_sender, tui_receiver).await });

    let mut app = App::new(tui_sender, udisks_receiver);
    let tui_handle = tokio::task::spawn_blocking(move || {
        let mut terminal = tui::init()?;
        let r = app.run(&mut terminal);
        tui::restore()?;
        r?;
        app.print_exit_mount_point();
        Ok::<(), color_eyre::eyre::Error>(())
    });

    let r = udisks_handle.await?;
    tui_handle.await??;
    r?;

    Ok(())
}

async fn udisks_task(
    sender: Sender<UDisks2Message>,
    mut receiver: Receiver<TuiMessage>,
) -> Result<()> {
    let client = Client::new().await?;
    let (devices, mut block_devices_usage) = get_devices(&client).await?;
    sender.send(UDisks2Message::Devices(devices)).await?;

    while let Some(msg) = receiver.recv().await {
        match msg {
            TuiMessage::Mount(idx) => {
                let (bd, bdu) = block_devices_usage[idx].take().unwrap();
                let bdu = checked(&sender, bdu.clone(), mount(&client, idx, bdu, None)).await?;
                block_devices_usage[idx] = Some((bd, bdu));
            }
            TuiMessage::Unmount(idx) => {
                let (bd, bdu) = block_devices_usage[idx].take().unwrap();
                let bdu = checked(&sender, bdu.clone(), unmount(idx, bdu)).await?;
                block_devices_usage[idx] = Some((bd, bdu));
            }
            TuiMessage::UnlockAndMount(idx, passphrase) => {
                let (bd, bdu) = block_devices_usage[idx].take().unwrap();
                let bdu = checked(
                    &sender,
                    bdu.clone(),
                    mount(&client, idx, bdu, Some(passphrase)),
                )
                .await?;
                block_devices_usage[idx] = Some((bd, bdu));
            }
            TuiMessage::Refresh => {
                let n = get_devices(&client).await?;
                block_devices_usage = n.1;
                sender.send(UDisks2Message::Devices(n.0)).await?;
            }
            TuiMessage::Quit => break,
        }
    }

    Ok(())
}

async fn get_devices(
    client: &Client,
) -> Result<(Vec<Device>, Vec<Option<(BlockDevice, BlockDeviceUsage)>>)> {
    let block_devices = client.get_block_devices().await?;
    let mut block_devices_usage = Vec::with_capacity(block_devices.len());

    for mut device in block_devices {
        if device.hidden().await? {
            continue;
        }
        let bdu = client.use_block_device(&mut device).await?;
        if let BlockDeviceUsage::Other = bdu {
            continue;
        }
        block_devices_usage.push(Some((device, bdu)));
    }

    let mut devices = Vec::with_capacity(block_devices_usage.len());

    for e in &block_devices_usage {
        let (device, device_usage) = e.as_ref().unwrap();
        let state = match device_usage {
            BlockDeviceUsage::MountedFilesystem(_) => DeviceState::Mounted,
            BlockDeviceUsage::UnmountedFilesystem(_) => DeviceState::Unmounted,
            BlockDeviceUsage::MountedUnlockedEncrypted(_) => DeviceState::Mounted,
            BlockDeviceUsage::UnmountedUnlockedEncrypted(_) => DeviceState::UnmountedUnlocked,
            BlockDeviceUsage::LockedEncrypted(_) => DeviceState::Locked,
            BlockDeviceUsage::Other => unreachable!(),
        };
        devices.push(Device {
            name: device.name().await?,
            label: device.label().await?,
            size: humansize::format_size(device.size().await?, DECIMAL),
            state,
        });
    }

    Ok((devices, block_devices_usage))
}

async fn checked<'a, F>(
    sender: &'a Sender<UDisks2Message>,
    prev_bdu: BlockDeviceUsage<'a>,
    f: F,
) -> Result<BlockDeviceUsage<'a>>
where
    F: Future<Output = Result<(BlockDeviceUsage<'a>, UDisks2Message)>>,
{
    match f.await {
        Ok((bdu, msg)) => {
            sender.send(msg).await?;
            Ok(bdu)
        }
        Err(err) if err.is::<SendError<UDisks2Message>>() => Err(err),
        Err(err) => {
            sender.send(UDisks2Message::Err(err.to_string())).await?;
            Ok(prev_bdu)
        }
    }
}

async fn mount<'a>(
    client: &'a Client,
    idx: usize,
    bdu: BlockDeviceUsage<'a>,
    passphrase: Option<String>,
) -> Result<(BlockDeviceUsage<'a>, UDisks2Message)> {
    match bdu {
        BlockDeviceUsage::MountedFilesystem(fs) => {
            let m = fs.mount_point().to_string();
            let bdu = BlockDeviceUsage::MountedFilesystem(fs);
            Ok((bdu, UDisks2Message::AlreadyMounted(idx, m)))
        }
        BlockDeviceUsage::UnmountedFilesystem(fs) => {
            let fs = fs.mount().await?;
            let m = fs.mount_point().to_string();
            let bdu = BlockDeviceUsage::MountedFilesystem(fs);
            Ok((bdu, UDisks2Message::Mounted(idx, m)))
        }
        BlockDeviceUsage::MountedUnlockedEncrypted(fs) => {
            let m = fs.mount_point().to_string();
            let bdu = BlockDeviceUsage::MountedUnlockedEncrypted(fs);
            Ok((bdu, UDisks2Message::AlreadyMounted(idx, m)))
        }
        BlockDeviceUsage::UnmountedUnlockedEncrypted(fs) => {
            let fs = fs.mount().await?;
            let m = fs.mount_point().to_string();
            let bdu = BlockDeviceUsage::MountedUnlockedEncrypted(fs);
            Ok((bdu, UDisks2Message::Mounted(idx, m)))
        }
        BlockDeviceUsage::LockedEncrypted(fs) => {
            let fs = fs.unlock(client, passphrase.as_ref().unwrap()).await?;
            let fs = fs.mount().await?;
            let m = fs.mount_point().to_string();
            let bdu = BlockDeviceUsage::MountedUnlockedEncrypted(fs);
            Ok((bdu, UDisks2Message::UnlockedAndMounted(idx, m)))
        }
        BlockDeviceUsage::Other => unreachable!(),
    }
}

async fn unmount(
    idx: usize,
    bdu: BlockDeviceUsage<'_>,
) -> Result<(BlockDeviceUsage, UDisks2Message)> {
    match bdu {
        BlockDeviceUsage::MountedFilesystem(fs) => {
            let fs = fs.unmount().await?;
            let bdu = BlockDeviceUsage::UnmountedFilesystem(fs);
            Ok((bdu, UDisks2Message::Unmounted(idx)))
        }
        BlockDeviceUsage::UnmountedFilesystem(fs) => {
            let bdu = BlockDeviceUsage::UnmountedFilesystem(fs);
            Ok((bdu, UDisks2Message::AlreadyUnmounted(idx)))
        }
        BlockDeviceUsage::MountedUnlockedEncrypted(fs) => {
            let fs = fs.unmount().await?;
            let fs = fs.lock().await?;
            let bdu = BlockDeviceUsage::LockedEncrypted(fs);
            Ok((bdu, UDisks2Message::UnmountedAndLocked(idx)))
        }
        BlockDeviceUsage::UnmountedUnlockedEncrypted(fs) => {
            let fs = fs.lock().await?;
            let bdu = BlockDeviceUsage::LockedEncrypted(fs);
            Ok((bdu, UDisks2Message::Locked(idx)))
        }
        BlockDeviceUsage::LockedEncrypted(fs) => {
            let bdu = BlockDeviceUsage::LockedEncrypted(fs);
            Ok((bdu, UDisks2Message::AlreadyUnmounted(idx)))
        }
        BlockDeviceUsage::Other => unreachable!(),
    }
}
