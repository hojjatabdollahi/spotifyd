use axum::response::{IntoResponse, Response};
use chrono::prelude::*;
use futures::task::{Context, Poll};
use futures::{self, Future};
use librespot_connect::spirc::Spirc;
use librespot_core::{
    keymaster::{get_token, Token as LibrespotToken},
    mercury::MercuryError,
    session::Session,
};
use librespot_playback::player::PlayerEvent;
use log::info;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use reqwest::StatusCode;
use rspotify::spotify::senum::RepeatState;
use rspotify::spotify::{
    client::Spotify, model::offset::for_position, oauth2::TokenInfo as RspotifyToken,
    util::datetime_to_timestamp,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::env;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use axum::{self, routing, Json};

pub struct RestServer {
    session: Session,
    spirc: Arc<Spirc>,
    api_token: RspotifyToken,
    #[allow(clippy::type_complexity)]
    token_request: Option<Pin<Box<dyn Future<Output = Result<LibrespotToken, MercuryError>>>>>,
    rest_future: Option<Pin<Box<dyn Future<Output = ()>>>>,
    device_name: String,
    event_rx: UnboundedReceiver<PlayerEvent>,
    event_tx: Option<UnboundedSender<PlayerEvent>>,
}

const CLIENT_ID: &str = "2c1ea588dfbc4a989e2426f8385297c3";
const SCOPE: &str = "user-read-playback-state,user-read-private,\
                     user-read-email,playlist-read-private,user-library-read,user-library-modify,\
                     user-top-read,playlist-read-collaborative,playlist-modify-public,\
                     playlist-modify-private,user-follow-read,user-follow-modify,\
                     user-read-currently-playing,user-modify-playback-state,\
                     user-read-recently-played";

impl RestServer {
    pub fn new(
        session: Session,
        spirc: Arc<Spirc>,
        device_name: String,
        event_rx: UnboundedReceiver<PlayerEvent>,
    ) -> RestServer {
        RestServer {
            session,
            spirc,
            api_token: RspotifyToken::default(),
            token_request: None,
            rest_future: None,
            device_name,
            event_rx,
            event_tx: None,
        }
    }

    fn is_token_expired(&self) -> bool {
        let now: DateTime<Utc> = Utc::now();
        match self.api_token.expires_at {
            Some(expires_at) => now.timestamp() > expires_at - 100,
            None => true,
        }
    }
}

impl Future for RestServer {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.event_tx.is_some() {
            if let Poll::Ready(Some(msg)) = self.event_rx.poll_recv(cx) {
                self.event_tx.as_ref().unwrap().send(msg).unwrap();
            }
        }
        let mut got_new_token = false;
        if self.is_token_expired() {
            if let Some(ref mut fut) = self.token_request {
                if let Poll::Ready(Ok(token)) = fut.as_mut().poll(cx) {
                    self.api_token = RspotifyToken::default()
                        .access_token(&token.access_token)
                        .expires_in(token.expires_in)
                        .expires_at(datetime_to_timestamp(token.expires_in));
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                    self.event_tx = Some(tx);
                    self.rest_future = Some(Box::pin(create_rest_server(
                        self.api_token.clone(),
                        self.spirc.clone(),
                        self.device_name.clone(),
                        rx,
                    )));
                    // TODO: for reasons I don't _entirely_ understand, the token request completing
                    // convinces callers that they don't need to re-check the status of this future
                    // until we start playing. This causes DBUS to not respond until that point in
                    // time. So, fire a "wake" here, which tells callers to keep checking.
                    cx.waker().clone().wake();
                    got_new_token = true;
                }
            } else {
                self.token_request = Some(Box::pin({
                    let sess = self.session.clone();
                    // This is more meant as a fast hotfix than anything else!
                    let client_id =
                        env::var("SPOTIFYD_CLIENT_ID").unwrap_or_else(|_| CLIENT_ID.to_string());
                    async move { get_token(&sess, &client_id, SCOPE).await }
                }));
            }
        } else if let Some(ref mut fut) = self.rest_future {
            return fut.as_mut().poll(cx);
        }

        if got_new_token {
            self.token_request = None;
        }

        Poll::Pending
    }
}

async fn shutdown() -> Json<Value> {
    Json(json!({"res": "done"}))
}

#[derive(Debug)]
enum SError {
    #[allow(dead_code)]
    NotFound,
    #[allow(dead_code)]
    StatusError,
    #[allow(dead_code)]
    InvalidUsername,
}
enum AppError {
    Playback(SError),
}

impl From<SError> for AppError {
    fn from(inner: SError) -> Self {
        AppError::Playback(inner)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            AppError::Playback(SError::NotFound) => (StatusCode::NOT_FOUND, "User not found"),
            AppError::Playback(SError::StatusError) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "Failed to get the data")
            }
            AppError::Playback(SError::InvalidUsername) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "Invalid username")
            }
        };

        let body = Json(json!({
            "error": error_message,
        }));

        (status, body).into_response()
    }
}

async fn playback(api_token: RspotifyToken) -> Result<Json<Value>, AppError> {
    let mv_api_token = api_token.clone();
    let sp = create_spotify_api(&mv_api_token);
    let val = sp.current_playback(None).map_err(|_| SError::StatusError)?;
    match val {
        Some(val) => Ok(Json(json!(val))),
        None => Err(AppError::Playback(SError::StatusError)),
    }
}

async fn shuffle(
    state: bool,
    device_name: String,
    api_token: RspotifyToken,
) -> Result<Json<Value>, AppError> {
    let mv_api_token = api_token.clone();
    let sp = create_spotify_api(&mv_api_token);
    let device_id = get_device_id(device_name.clone(), api_token.clone());
    info!("{device_id:?}");
    sp.shuffle(state, device_id)
        .map_err(|_| SError::StatusError)?;
    Ok(Json(json!({"res": "done"})))
}

async fn get_category_playlist(
    category_id: String,
    device_name: String,
    api_token: RspotifyToken,
) -> Result<Json<Value>, AppError> {
    let mv_api_token = api_token.clone();
    let sp = create_spotify_api(&mv_api_token);
    let device_id = get_device_id(device_name.clone(), api_token.clone());
    info!("{device_id:?}");

    let res = sp
        .get_category_playlist(category_id, None, Some(30), Some(0))
        .map_err(|e| {
            info!("{e:?}");
            SError::StatusError
        })?;
    Ok(Json(json!(res)))
}
async fn repeat(
    payload_state: String,
    device_name: String,
    api_token: RspotifyToken,
) -> Result<Json<Value>, AppError> {
    let mv_api_token = api_token.clone();
    let sp = create_spotify_api(&mv_api_token);
    let device_id = get_device_id(device_name.clone(), api_token.clone());
    info!("{device_id:?}");
    let state = match payload_state.as_str() {
        "track" => RepeatState::Track,
        "context" => RepeatState::Context,
        "off" => RepeatState::Off,
        _ => RepeatState::Off,
    };
    sp.repeat(state, device_id)
        .map_err(|_| SError::StatusError)?;
    Ok(Json(json!({"res": "done"})))
}

async fn search(query: String, mv_api_token: RspotifyToken) -> Json<Value> {
    let sp = create_spotify_api(&mv_api_token);
    let res = sp.search_track(&query, 6, 0, None).unwrap();
    let res2 = sp.search_album(&query, 5, 0, None).unwrap();
    let res3 = sp.search_artist(&query, 5, 0, None).unwrap();
    let res4 = sp.search_playlist(&query, 5, 0, None).unwrap();
    Json(json!([res, res2, res3, res4]))
}

#[derive(Deserialize)]
struct UriItem {
    uri: String,
}

#[derive(Deserialize)]
struct SearchItem {
    keyword: String,
}

#[derive(Deserialize)]
struct VolumeItem {
    vol: u8,
}

#[derive(Deserialize)]
struct SeekItem {
    pos: u32,
}

#[derive(Deserialize)]
struct StringItem {
    val: String,
}

#[derive(Deserialize)]
struct BoolItem {
    val: bool,
}

fn get_device_id(mv_device_name: String, mv_api_token: RspotifyToken) -> Option<String> {
    let device_name = utf8_percent_encode(&mv_device_name, NON_ALPHANUMERIC).to_string();
    let sp = create_spotify_api(&mv_api_token);
    match sp.device() {
        Ok(device_payload) => {
            match device_payload
                .devices
                .into_iter()
                .find(|d| d.name == device_name)
            {
                Some(device) => Some(device.id),
                None => None,
            }
        }
        Err(_) => None,
    }
}

async fn create_rest_server(
    api_token: RspotifyToken,
    spirc: Arc<Spirc>,
    device_name: String,
    mut event_rx: UnboundedReceiver<PlayerEvent>,
) {
    info!("Hello");
    let app = axum::Router::new()
        .route("/", routing::get(shutdown))
        .route(
            "/shutdown",
            routing::get({
                let local_spirc = Arc::clone(&spirc);
                move || {
                    local_spirc.shutdown();
                    shutdown()
                }
            }),
        )
        .route(
            "/play_pause",
            routing::get({
                let local_spirc = Arc::clone(&spirc);
                move || {
                    local_spirc.play_pause();
                    shutdown()
                }
            }),
        )
        .route(
            "/next",
            routing::get({
                let local_spirc = Arc::clone(&spirc);
                move || {
                    local_spirc.next();
                    shutdown()
                }
            }),
        )
        .route(
            "/previous",
            routing::get({
                let local_spirc = Arc::clone(&spirc);
                move || {
                    local_spirc.prev();
                    shutdown()
                }
            }),
        )
        .route(
            "/seek",
            routing::post({
                let mv_device_name = device_name.clone();
                let mv_api_token = api_token.clone();
                let device_id = get_device_id(mv_device_name.clone(), api_token.clone());
                move |Json(payload): Json<SeekItem>| {
                    let sp = create_spotify_api(&mv_api_token);
                    info!("pos: {}", payload.pos);
                    if let Ok(Some(playing)) = sp.current_user_playing_track() {
                        info!("current pos: {}", playing.progress_ms.unwrap_or(0));
                        let res = sp.seek_track(payload.pos, device_id);
                        info!("{res:?}");
                    }
                    shutdown()
                }
            }),
        )
        .route(
            "/get_category_playlist",
            routing::post({
                let mv_device_name = device_name.clone();
                let mv_api_token = api_token.clone();
                move |Json(payload): Json<StringItem>| {
                    get_category_playlist(payload.val, mv_device_name, mv_api_token)
                }
            }),
        )
        .route(
            "/repeat",
            routing::post({
                let mv_device_name = device_name.clone();
                let mv_api_token = api_token.clone();
                move |Json(payload): Json<StringItem>| {
                    repeat(payload.val, mv_device_name, mv_api_token)
                }
            }),
        )
        .route(
            "/shuffle",
            routing::post({
                let mv_device_name = device_name.clone();
                let mv_api_token = api_token.clone();
                move |Json(payload): Json<BoolItem>| {
                    shuffle(payload.val, mv_device_name, mv_api_token)
                }
            }),
        )
        .route(
            "/search",
            routing::post({
                let mv_api_token = api_token.clone();
                move |Json(payload): Json<SearchItem>| search(payload.keyword, mv_api_token)
            }),
        )
        .route(
            "/player_status",
            routing::get({
                let mv_api_token = api_token.clone();
                move || playback(mv_api_token)
            }),
        )
        .route(
            "/transfer_playback",
            routing::get({
                let mv_device_name = device_name.clone();
                let mv_api_token = api_token.clone();
                move || {
                    let sp = create_spotify_api(&mv_api_token);
                    let device_id = get_device_id(mv_device_name, mv_api_token);
                    let _ = sp.transfer_playback(&device_id.unwrap(), false);

                    shutdown()
                }
            }),
        )
        .route(
            "/volume",
            routing::post({
                let mv_device_name = device_name.clone();
                let mv_api_token = api_token.clone();
                move |Json(payload): Json<VolumeItem>| {
                    let sp = create_spotify_api(&mv_api_token);
                    let device_id = get_device_id(mv_device_name, mv_api_token);
                    sp.volume(payload.vol, device_id).unwrap();
                    shutdown()
                }
            }),
        )
        .route(
            "/open_uri",
            routing::post({
                let mv_device_name = device_name.clone();
                let mv_api_token = api_token.clone();
                move |Json(payload): Json<UriItem>| {
                    let sp = create_spotify_api(&mv_api_token);
                    let device_id = get_device_id(mv_device_name, mv_api_token);
                    if payload.uri.contains("spotify:track") {
                        let _ = sp.start_playback(
                            device_id,
                            None,
                            Some(vec![payload.uri]),
                            for_position(0),
                            None,
                        );
                    } else {
                        let _ = sp.start_playback(
                            device_id,
                            Some(payload.uri),
                            None,
                            for_position(0),
                            None,
                        );
                    }
                    shutdown()
                }
            }),
        );
    let w = axum::Server::bind(&"0.0.0.0:3000".parse().unwrap()).serve(app.into_make_service());
    tokio::spawn(async move { w.await });

    loop {
        let _ = event_rx
            .recv()
            .await
            .expect("Changed track channel was unexpectedly closed");
    }
}

fn create_spotify_api(token: &RspotifyToken) -> Spotify {
    Spotify::default().access_token(&token.access_token).build()
}
