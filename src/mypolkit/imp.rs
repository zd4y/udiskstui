use color_eyre::eyre::bail;
use glib::error::ErrorDomain;
use glib::object::Cast;
use glib::subclass::prelude::*;
use polkit_agent_rs::gio;
use polkit_agent_rs::gio::prelude::CancellableExt;
use polkit_agent_rs::polkit;
use polkit_agent_rs::polkit::UnixUser;
use polkit_agent_rs::subclass::ListenerImpl;
use polkit_agent_rs::Session as AgentSession;
use secrecy::ExposeSecret;
use tokio::sync::oneshot;

use crate::AgentMessage;

#[derive(Default)]
pub struct MyPolkit {
    pub sender: OnceCell<mpsc::Sender<AgentMessage>>,
}
use std::cell::OnceCell;
use std::sync::mpsc;

#[derive(Debug, Clone, Copy)]
struct SessionError;

impl ErrorDomain for SessionError {
    fn domain() -> glib::Quark {
        glib::Quark::from_str("session_error")
    }
    fn code(self) -> i32 {
        -1
    }
    fn from(code: i32) -> Option<Self>
    where
        Self: Sized,
    {
        if code == -1 {
            return Some(Self);
        }
        None
    }
}

fn start_session(
    session: &AgentSession,
    name: String,
    cancellable: gio::Cancellable,
    task: gio::Task<String>,
    sender: mpsc::Sender<AgentMessage>,
) {
    let sub_loop = glib::MainLoop::new(None, true);

    let sub_loop_2 = sub_loop.clone();
    session.connect_completed(move |session, success| {
        let task = task.clone();
        if !success {
            unsafe {
                task.return_result(Err(glib::Error::new(SessionError, "unsuccessfull")));
            }
        } else {
            unsafe {
                task.return_result(Ok("success".to_string()));
            }
        }
        session.cancel();
        sub_loop_2.quit();
    });
    session.connect_show_info(|_session, _info| unimplemented!());
    session.connect_show_error(|_session, _error| unimplemented!());
    session.connect_request(move |session, request, _echo_on| {
        if !request.starts_with("Password:") {
            return;
        }

        let (respond_to, receiver) = oneshot::channel();
        if let Err(err) = sender.send(AgentMessage::RequestPassword {
            name: name.clone(),
            respond_to,
        }) {
            session.cancel();
            panic!("failed to send agent message: {err}");
        }
        let Ok(password) = receiver.blocking_recv() else {
            session.cancel();
            cancellable.cancel();
            return;
        };
        session.response(password.expose_secret());
    });
    session.initiate();
    sub_loop.run();
}

impl ListenerImpl for MyPolkit {
    type Message = String;
    fn initiate_authentication(
        &self,
        _action_id: &str,
        _message: &str,
        _icon_name: &str,
        _details: &polkit::Details,
        cookie: &str,
        identities: Vec<polkit::Identity>,
        cancellable: gio::Cancellable,
        task: gio::Task<Self::Message>,
    ) {
        let users: Vec<UnixUser> = identities
            .into_iter()
            .flat_map(|idenifier| idenifier.dynamic_cast())
            .collect();

        let (name, index) = match self.choose_user(&users) {
            Ok(Some(val)) => val,
            Ok(None) => {
                cancellable.cancel();
                return;
            }
            Err(err) => {
                cancellable.cancel();
                panic!("failed to choose user: {err}")
            }
        };

        let session = AgentSession::new(&users[index], cookie);

        start_session(
            &session,
            name,
            cancellable,
            task,
            self.sender.get().unwrap().clone(),
        );
    }
    fn initiate_authentication_finish(
        &self,
        gio_result: Result<gio::Task<Self::Message>, glib::Error>,
    ) -> bool {
        match gio_result {
            Ok(_) => true,
            Err(_err) => {
                unimplemented!()
            }
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for MyPolkit {
    const NAME: &'static str = "MyPolkit";
    type Type = super::MyPolkit;
    type ParentType = super::Listener;
}

impl ObjectImpl for MyPolkit {}

impl MyPolkit {
    fn choose_user(&self, users: &[UnixUser]) -> color_eyre::Result<Option<(String, usize)>> {
        let names: Vec<String> = users
            .iter()
            .map(|user| user.name().unwrap().to_string())
            .collect();

        let (sender, receiver) = oneshot::channel();
        if let Err(err) = self.sender.get().unwrap().send(AgentMessage::ChooseUser {
            users: names,
            respond_to: sender,
        }) {
            bail!("failed to send agent message: {err}");
        }

        match receiver.blocking_recv() {
            Ok(res) => Ok(res),
            Err(err) => {
                bail!("failed to receive answer: {err}")
            }
        }
    }
}
