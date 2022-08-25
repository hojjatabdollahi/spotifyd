#[cfg(feature = "dbus_mpris")]
use crate::dbus_mpris::DbusServer;
use crate::process::{spawn_program_on_event, Child};
#[cfg(feature = "rest_api")]
use crate::rest_api::RestServer;
use futures::{self, Future, Stream, StreamExt};
use librespot_connect::spirc::Spirc;
use librespot_core::session::SessionError;
use librespot_core::{
    cache::Cache,
    config::{ConnectConfig, DeviceType, SessionConfig},
    session::Session,
};
use librespot_discovery::Credentials;
use librespot_discovery::Discovery;
use librespot_playback::config::{AudioFormat, VolumeCtrl};
use librespot_playback::{
    audio_backend::Sink,
    config::PlayerConfig,
    mixer::Mixer,
    player::{Player, PlayerEvent},
};
use log::{error, info};
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
#[cfg(any(feature = "dbus_mpris", feature = "rest_api"))]
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_stream::wrappers::UnboundedReceiverStream;

pub struct LibreSpotConnection {
    connection: Pin<Box<dyn Future<Output = Result<(Session, Credentials), SessionError>>>>,
    spirc_task: Option<Pin<Box<dyn Future<Output = ()>>>>,
    spirc: Option<Arc<Spirc>>,
    discovery_stream: Discovery,
}

impl LibreSpotConnection {
    pub fn new(
        connection: Pin<Box<dyn Future<Output = Result<(Session, Credentials), SessionError>>>>,
        discovery_stream: Discovery,
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
    pub backend: fn(Option<String>, AudioFormat) -> Box<dyn Sink>,
    pub audio_device: Option<String>,
}

pub struct SpotifydState {
    // TODO: this ain't a stream anymore, rename
    pub ctrl_c_stream: Pin<Box<dyn Future<Output = Result<(), io::Error>>>>,
    pub shutting_down: bool,
    pub cache: Option<Cache>,
    pub device_name: String,
    pub player_event_channel: Option<Pin<Box<dyn Stream<Item = PlayerEvent>>>>,
    pub player_event_program: Option<String>,
    pub dbus_mpris_server: Option<Pin<Box<dyn Future<Output = ()>>>>,
    pub rest_server: Option<Pin<Box<dyn Future<Output = ()>>>>,
}

#[cfg(feature = "dbus_mpris")]
#[allow(clippy::unnecessary_wraps)]
fn new_dbus_server(
    session: Session,
    spirc: Arc<Spirc>,
    device_name: String,
    event_rx: UnboundedReceiver<PlayerEvent>,
) -> Option<Pin<Box<dyn Future<Output = ()>>>> {
    Some(Box::pin(DbusServer::new(
        session,
        spirc,
        device_name,
        event_rx,
    )))
}

#[cfg(feature = "rest_api")]
#[allow(clippy::unnecessary_wraps)]
fn new_rest_server(
    session: Session,
    spirc: Arc<Spirc>,
    device_name: String,
    event_rx: UnboundedReceiver<PlayerEvent>,
    event_tx_something: UnboundedSender<String>,
) -> Option<Pin<Box<dyn Future<Output = ()>>>> {
    Some(Box::pin(RestServer::new(
        session,
        spirc,
        device_name,
        event_rx,
        event_tx_something,
    )))
}

pub(crate) struct MainLoopState {
    pub(crate) librespot_connection: LibreSpotConnection,
    pub(crate) audio_setup: AudioSetup,
    pub(crate) spotifyd_state: SpotifydState,
    pub(crate) player_config: PlayerConfig,
    pub(crate) session_config: SessionConfig,
    pub(crate) autoplay: bool,
    pub(crate) volume_ctrl: VolumeCtrl,
    pub(crate) initial_volume: Option<u16>,
    pub(crate) running_event_program: Option<Child>,
    pub(crate) shell: String,
    pub(crate) device_type: DeviceType,
    // Command line option should still be available without dbus_mpris feature
    #[allow(dead_code)]
    pub(crate) use_mpris: bool,
    #[cfg(feature = "dbus_mpris")]
    pub(crate) mpris_event_tx: Option<UnboundedSender<PlayerEvent>>,
    #[allow(dead_code)]
    pub(crate) use_rest: bool,
    #[cfg(feature = "rest_api")]
    pub(crate) rest_event_tx: Option<UnboundedSender<PlayerEvent>>,
    pub(crate) rest_rx_got_something: Option<UnboundedReceiver<String>>,
}

impl Future for MainLoopState {
    type Output = ();

    fn poll(mut self: Pin<&mut MainLoopState>, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            info!("1");
            if let Poll::Ready(Some(creds)) = self
                .as_mut()
                .librespot_connection
                .discovery_stream
                .poll_next_unpin(cx)
            {
                if let Some(ref mut spirc) = self.librespot_connection.spirc {
                    spirc.shutdown();
                }
                let session_config = self.session_config.clone();
                let cache = self.spotifyd_state.cache.clone();
                // TODO: a bunch of this init logic can probably be unrolled using async / await
                self.librespot_connection.connection =
                    Box::pin(Session::connect(session_config, creds, cache, false));
            }

            info!("2");
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

            info!("3");
            if self.running_event_program.is_none() {
                if let Some(ref mut player_event_channel) = self.spotifyd_state.player_event_channel
                {
                    if let Poll::Ready(Some(event)) = player_event_channel.poll_next_unpin(cx) {
                        #[cfg(feature = "dbus_mpris")]
                        if let Some(ref tx) = self.mpris_event_tx {
                            tx.send(event.clone()).unwrap();
                        }
                        #[cfg(feature = "rest_api")]
                        if let Some(ref tx) = self.rest_event_tx {
                            tx.send(event.clone()).unwrap();
                        }
                        if let Some(ref cmd) = self.spotifyd_state.player_event_program {
                            match spawn_program_on_event(&self.shell, cmd, event) {
                                Ok(child) => self.running_event_program = Some(child),
                                Err(e) => error!("{}", e),
                            }
                        }
                    }
                }
            }

            info!("4");
            if let Some(ref mut fut) = self.spotifyd_state.dbus_mpris_server {
                let _ = fut.as_mut().poll(cx);
            }

            info!("5");
            info!("before rest server");
            if let Some(ref mut fut) = self.spotifyd_state.rest_server {
                info!("before rest poll ");
                let _ = fut.as_mut().poll(cx);
            }

            info!("rx_something");
            if let Some(ref mut f) = self.rest_rx_got_something {
                info!("rx_something is something");
                while let Poll::Ready(a) = f.poll_recv(cx) {
                    info!("rx_something returned: {a:?}");
                }
                // match f.poll_recv(cx) {
                //     Poll::Ready(a) => {
                //         info!("rx_something returned: {a:?}");
                //     }
                //     Poll::Pending => {
                //         info!("rx_something is pending");
                //     }
                // }
            }
            // info!("rx_something");
            // if let Some(ref mut f) = self.rest_rx_got_something {
            //     info!("rx_something is something");
            //     match f.poll_recv(cx) {
            //         Poll::Ready(a) => {
            //             info!("rx_something returned: {a:?}");
            //         }
            //         Poll::Pending => {
            //             info!("rx_something is pending");
            //         }
            //     }
            // }

            info!("after rest server");

            info!("6");
            if let Poll::Ready(Ok(session)) = self.librespot_connection.connection.as_mut().poll(cx)
            {
                info!("if 1");
                let mixer = (self.audio_setup.mixer)();
                let audio_filter = mixer.get_soft_volume();
                self.librespot_connection.connection = Box::pin(futures::future::pending());
                let backend = self.audio_setup.backend;
                let audio_device = self.audio_setup.audio_device.clone();
                let (player, event_channel) = Player::new(
                    self.player_config.clone(),
                    session.0.clone(),
                    audio_filter,
                    // TODO: dunno how to work with AudioFormat yet, maybe dig further if this
                    // doesn't work for all configurations
                    move || (backend)(audio_device, AudioFormat::default()),
                );

                self.spotifyd_state.player_event_channel =
                    Some(Box::pin(UnboundedReceiverStream::new(event_channel)));

                let (spirc, spirc_task) = Spirc::new(
                    ConnectConfig {
                        autoplay: self.autoplay,
                        name: self.spotifyd_state.device_name.clone(),
                        device_type: self.device_type,
                        initial_volume: self.initial_volume,
                        has_volume_ctrl: true,
                    },
                    session.0.clone(),
                    player,
                    mixer,
                );
                self.librespot_connection.spirc_task = Some(Box::pin(spirc_task));
                let shared_spirc = Arc::new(spirc);
                self.librespot_connection.spirc = Some(shared_spirc.clone());

                #[cfg(feature = "dbus_mpris")]
                if self.use_mpris {
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                    self.mpris_event_tx = Some(tx);
                    self.spotifyd_state.dbus_mpris_server = new_dbus_server(
                        session.clone(),
                        shared_spirc.clone(),
                        self.spotifyd_state.device_name.clone(),
                        rx,
                    );
                }

                #[cfg(feature = "rest_api")]
                if self.use_rest {
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                    let (tx_rest_something, rx_rest_something) =
                        tokio::sync::mpsc::unbounded_channel();
                    self.rest_event_tx = Some(tx);
                    self.rest_rx_got_something = Some(rx_rest_something);
                    self.spotifyd_state.rest_server = new_rest_server(
                        session.0.clone(),
                        shared_spirc.clone(),
                        self.spotifyd_state.device_name.clone(),
                        rx,
                        tx_rest_something,
                    );
                }
            } else if self
                .spotifyd_state
                .ctrl_c_stream
                .as_mut()
                .poll(cx)
                .is_ready()
            {
                info!("if 2");
                if !self.spotifyd_state.shutting_down {
                    if let Some(ref spirc) = self.librespot_connection.spirc {
                        spirc.shutdown();
                        self.spotifyd_state.shutting_down = true;
                    }
                    return Poll::Ready(());
                }
            } else if let Some(Poll::Ready(_)) = self
                .librespot_connection
                .spirc_task
                .as_mut()
                .map(|ref mut st| st.as_mut().poll(cx))
            {
                info!("if 3");
                return Poll::Ready(());
            } else {
                info!("if 4");
                info!("return pending for some reason");
                return Poll::Pending;
            }
        }
    }
}
