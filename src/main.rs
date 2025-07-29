use std::{collections::HashMap, sync::mpsc, thread};

use app::App;
use color_eyre::Result;

mod app;
mod device;
mod errors;
mod tui;
mod udisks2;

mod mypolkit;
use glib::{subclass::types::ObjectSubclassExt, variant::ToVariant};
use mypolkit::MyPolkit;
use polkit_agent_rs::{gio, polkit::UnixProcess, traits::ListenerExt, RegisterFlags};
use secrecy::SecretString;
use tokio::sync::oneshot;

const OBJECT_PATH: &str = "/org/udiskstui/PolicyKit1/AuthenticationAgent";

#[derive(Debug)]
pub enum AgentMessage {
    ChooseUser {
        users: Vec<String>,
        respond_to: oneshot::Sender<Option<(String, usize)>>,
    },
    RequestPassword {
        name: String,
        respond_to: oneshot::Sender<SecretString>,
    },
    // Error(String)
}

fn main() -> Result<()> {
    errors::install_hooks()?;

    let main_loop = glib::MainLoop::new(None, false);

    let subject = UnixProcess::new_for_owner(
        nix::unistd::getpid().as_raw(),
        0,
        nix::unistd::getuid().as_raw().try_into()?,
    );

    let my_polkit = MyPolkit::default();
    let mut options = HashMap::new();
    options.insert("fallback", true.to_variant());
    let options = options.to_variant();
    let _handle = my_polkit.register_with_options(
        RegisterFlags::NONE,
        &subject,
        OBJECT_PATH,
        Some(&options),
        gio::Cancellable::NONE,
    )?;

    let (glib_cancel_send, glib_cancel_receive) = oneshot::channel::<()>();
    let (tui_sender, tui_receiver) = mpsc::channel();

    let my_polkit_imp = mypolkit::imp::MyPolkit::from_obj(&my_polkit);
    my_polkit_imp.sender.set(tui_sender).unwrap();

    let main_loop_2 = main_loop.clone();
    glib::MainContext::default().spawn_local(async move {
        glib_cancel_receive.await.unwrap();
        main_loop_2.quit();
    });

    thread::spawn(move || start_tui(tui_receiver, glib_cancel_send).unwrap());

    main_loop.run();

    Ok(())
}

fn start_tui(
    receiver: mpsc::Receiver<AgentMessage>,
    glib_cancel_send: oneshot::Sender<()>,
) -> Result<()> {
    let mut app = App::new(receiver, glib_cancel_send)?;
    let mut terminal = tui::init()?;
    let result = app.run(&mut terminal);
    tui::restore()?;
    result?;
    app.print_exit_mount_point();

    Ok(())
}
