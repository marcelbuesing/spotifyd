use futures::{self, Future, Poll, Stream, select};
use futures_util::future::TryFutureExt;
use librespot::{
    connect::{
        discovery::DiscoveryStream,
        spirc::{Spirc, SpircTask},
    },
    core::{
        cache::Cache,
        config::{ConnectConfig, DeviceType, SessionConfig},
        session::Session,
    },
    playback::{
        audio_backend::Sink,
        config::PlayerConfig,
        mixer::Mixer,
        player::{Player, PlayerEvent},
    },
};
use log::error;
use tokio::runtime::Handle;
// use tokio_core::reactor::Handle;
// use tokio::io::IoStream;

use std::{io, rc::Rc, pin::Pin, task::Context};

#[cfg(feature = "dbus_mpris")]
use crate::dbus_mpris::DbusServer;
use crate::process::{spawn_program_on_event, Child};

pub struct LibreSpotConnection {
    connection: Box<dyn Future<Output = Result<Session, io::Error>>>,
    spirc_task: Option<SpircTask>,
    spirc: Option<Rc<Spirc>>,
    discovery_stream: DiscoveryStream,
}

impl LibreSpotConnection {
    pub fn new(
        connection: Box<dyn Future<Output = Result<Session, io::Error>>>,
        discovery_stream: DiscoveryStream,
    ) -> LibreSpotConnection {
        LibreSpotConnection {
            connection,
            spirc_task: None,
            spirc: None,
            discovery_stream,
        }
    }
}

pub struct AudioSetup {
    pub mixer: Box<dyn FnMut() -> Box<dyn Mixer>>,
    pub backend: fn(Option<String>) -> Box<dyn Sink>,
    pub audio_device: Option<String>,
}

pub struct SpotifydState {
    pub shutting_down: bool,
    pub cache: Option<Cache>,
    pub device_name: String,
    pub player_event_channel: Option<futures::channel::mpsc::UnboundedReceiver<PlayerEvent>>,
    pub player_event_program: Option<String>,
    pub dbus_mpris_server: Option<Box<dyn Future<Output = Result<(), ()>>>>,
}

#[cfg(feature = "dbus_mpris")]
fn new_dbus_server(
    session: Session,
    handle: Handle,
    spirc: Rc<Spirc>,
    device_name: String,
) -> Option<Box<dyn Future<Output = Result<(), ()>>>> {
    Some(Box::new(DbusServer::new(
        session,
        handle,
        spirc,
        device_name,
    )))
}

#[cfg(not(feature = "dbus_mpris"))]
fn new_dbus_server(
    _: Session,
    _: Handle,
    _: Rc<Spirc>,
    _: String,
) -> Option<Box<dyn Future<Output = Result<(), ()>>>> {
    None
}

pub(crate) struct MainLoopState {
    pub(crate) librespot_connection: LibreSpotConnection,
    pub(crate) audio_setup: AudioSetup,
    pub(crate) spotifyd_state: SpotifydState,
    pub(crate) player_config: PlayerConfig,
    pub(crate) session_config: SessionConfig,
    pub(crate) handle: Handle,
    pub(crate) linear_volume: bool,
    pub(crate) running_event_program: Option<Child>,
    pub(crate) shell: String,
}

impl MainLoopState {
    async fn run(&mut self) {
        loop {


            if let Some(mut child) = self.running_event_program.take() {
                match child.try_wait() {
                    // Still running...
                    Ok(None) => self.running_event_program = Some(child),
                    // Exited with error...
                    Err(e) => error!("{}", e),
                    // Exited without error...
                    Ok(Some(_)) => (),
                }
            }
            if self.running_event_program.is_none() {
                if let Some(ref mut player_event_channel) = self.spotifyd_state.player_event_channel
                {
                    if let Some(event) = player_event_channel.next().await.unwrap() {
                        if let Some(ref cmd) = self.spotifyd_state.player_event_program {
                            match spawn_program_on_event(&self.shell, cmd, event) {
                                Ok(child) => self.running_event_program = Some(child),
                                Err(e) => error!("{}", e),
                            }
                        }
                    }
                }
            }

            if let Some(ref mut fut) = self.spotifyd_state.dbus_mpris_server {
                let _ = fut.poll();
            }

            select! {
                Some(creds) = self.librespot_connection.discovery_stream.next() => {
                    if let Some(ref mut spirc) = self.librespot_connection.spirc {
                        spirc.shutdown();
                    }
                    let session_config = self.session_config.clone();
                    let cache = self.spotifyd_state.cache.clone();
                    let handle = self.handle.clone();
                    self.librespot_connection.connection =
                        Session::connect(session_config, creds, cache, handle).compat();
                },
                session = self.librespot_connection.connection.next() => {
                    let mixer = (self.audio_setup.mixer)();
                    let audio_filter = mixer.get_audio_filter();
                    self.librespot_connection.connection = Box::new(futures::future::ok(()).compat());
                    let backend = self.audio_setup.backend;
                    let audio_device = self.audio_setup.audio_device.clone();
                    let (player, event_channel) = Player::new(
                        self.player_config.clone(),
                        session.clone(),
                        audio_filter,
                        move || (backend)(audio_device),
                    );

                    self.spotifyd_state.player_event_channel = Some(event_channel);

                    let (spirc, spirc_task) = Spirc::new(
                        ConnectConfig {
                            name: self.spotifyd_state.device_name.clone(),
                            device_type: DeviceType::default(),
                            volume: mixer.volume(),
                            linear_volume: self.linear_volume,
                        },
                        session.clone(),
                        player,
                        mixer,
                    );
                    self.librespot_connection.spirc_task = Some(spirc_task);
                    let shared_spirc = Rc::new(spirc);
                    self.librespot_connection.spirc = Some(shared_spirc.clone());

                    self.spotifyd_state.dbus_mpris_server = new_dbus_server(
                        session,
                        self.handle.clone(),
                        shared_spirc,
                        self.spotifyd_state.device_name.clone(),
                    );
            }
            // _ = self.spotifyd_state.ctrl_c_stream.next().unwrap() {
            //     if !self.spotifyd_state.shutting_down {
            //         if let Some(ref spirc) = self.librespot_connection.spirc {
            //             spirc.shutdown();
            //             self.spotifyd_state.shutting_down = true;
            //         } else {
            //             return Ok(Async::Ready(()));
            //         }
            //     }
            // } 
            // _ = self
            //     .librespot_connection
            //     .spirc_task
            //     .as_mut()
            //     .map(|ref mut st| st.poll().unwrap())
            //     .next()
            // {
            //     return Ok(Async::Ready(()));
            // } else {
            //     return Ok(Async::NotReady);
            // }        }
            }
        }
    }
}
