use app::{App, Device, DeviceState, TuiMessage, UDisks2Message};
use color_eyre::Result;
use tokio::sync::mpsc;

mod app;
mod errors;
mod tui;

#[tokio::main]
async fn main() -> Result<()> {
    errors::install_hooks()?;

    let (tui_sender, mut tui_receiver) = mpsc::channel::<TuiMessage>(100);
    let (udisks_sender, udisks_receiver) = mpsc::channel::<UDisks2Message>(100);

    let udisks_handle = tokio::spawn(async move {
        let devices = vec![
            Device {
                name: "/dev/sda1".to_string(),
                label: "a".to_string(),
                size: 10,
                state: DeviceState::Unmounted,
            },
            Device {
                name: "/nvme0n1/nvme0n1p1".to_string(),
                label: "b".to_string(),
                size: 20,
                state: DeviceState::UnmountedUnlocked,
            },
            Device {
                name: "/nvme0n1/nvme0n1p2".to_string(),
                label: "c".to_string(),
                size: 25,
                state: DeviceState::Locked,
            },
        ];

        udisks_sender.send(UDisks2Message::Devices(devices)).await?;

        while let Some(msg) = tui_receiver.recv().await {
            match msg {
                TuiMessage::Mount(device) => {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    // udisks_sender.send(UDisks2Message::Err("nope".to_string())).await?;
                    udisks_sender
                        .send(UDisks2Message::Mounted(device, "/example".to_string()))
                        .await?;
                }
                TuiMessage::Unmount(device) => {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    udisks_sender
                        .send(UDisks2Message::Unmounted(device))
                        .await?;
                }
                TuiMessage::UnlockAndMount(device, passphrase) => {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    udisks_sender
                        .send(UDisks2Message::UnlockedAndMounted(
                            device,
                            "/example".to_string(),
                        ))
                        .await?;
                }
                TuiMessage::Quit => break,
            }
        }

        Ok::<(), color_eyre::eyre::Error>(())
    });

    let mut app = App::new(tui_sender, udisks_receiver);
    let tui_handle = tokio::task::spawn_blocking(move || {
        let mut terminal = tui::init()?;
        app.run(&mut terminal)?;
        tui::restore()?;
        app.print_exit_mount_point();
        Ok::<(), color_eyre::eyre::Error>(())
    });

    tui_handle.await??;
    udisks_handle.await??;

    Ok(())
}
