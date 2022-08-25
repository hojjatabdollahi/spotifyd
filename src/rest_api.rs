use axum::response::{IntoResponse, Response};
use chrono::{prelude::*, Duration};
use futures::task::{Context, Poll};
use futures::{self, Future};
use librespot_connect::spirc::Spirc;
use librespot_core::{
    keymaster::{get_token, Token as LibrespotToken},
    mercury::MercuryError,
    session::Session,
};

use librespot_playback::player::PlayerEvent;
use log::{error, info, trace};
use reqwest::StatusCode;
use rspotify::ClientError;
use rspotify::{
    model::{
        offset::Offset, AlbumId, ArtistId, EpisodeId, IdError, PlaylistId, RepeatState, SearchType,
        ShowId, TrackId, Type,
    },
    prelude::*,
    AuthCodeSpotify, Token as RspotifyToken,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{env, pin::Pin, sync::Arc};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use axum::{self, routing, Json};

pub struct RestServer {
    session: Session,
    spirc: Arc<Spirc>,
    // api_token: RspotifyToken,
    spotify_client: Arc<AuthCodeSpotify>,
    #[allow(clippy::type_complexity)]
    token_request: Option<Pin<Box<dyn Future<Output = Result<LibrespotToken, MercuryError>>>>>,
    rest_future: Option<Pin<Box<dyn Future<Output = ()>>>>,
    device_name: String,
    event_rx: UnboundedReceiver<PlayerEvent>,
    event_tx: Option<UnboundedSender<PlayerEvent>>,
    event_tx_something: Arc<UnboundedSender<String>>,
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
        event_tx_something: UnboundedSender<String>,
    ) -> RestServer {
        RestServer {
            session,
            spirc,
            spotify_client: Default::default(),
            token_request: None,
            rest_future: None,
            device_name,
            event_rx,
            event_tx: None,
            event_tx_something: Arc::new(event_tx_something),
        }
    }
}

impl Future for RestServer {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        info!("INSIDE THE POLL FOR REST");
        if self.event_tx.is_some() {
            if let Poll::Ready(Some(msg)) = self.event_rx.poll_recv(cx) {
                self.event_tx.as_ref().unwrap().send(msg).unwrap();
            }
        }
        let needs_token = match *self.spotify_client.get_token().lock().unwrap() {
            Some(ref token) => token.is_expired(),
            None => true,
        };
        info!("Needs token? {needs_token:?}");

        if needs_token {
            info!("Trying to renew token");
            if let Some(mut fut) = self.token_request.take() {
                if let Poll::Ready(token) = fut.as_mut().poll(cx) {
                    let token = match token {
                        Ok(token) => {
                            info!("Got a new token {token:?}");
                            token
                        }
                        Err(_) => {
                            error!("failed to request a token for the web API");
                            // shutdown DBus-Server
                            return Poll::Ready(());
                        }
                    };

                    let expires_in = Duration::seconds(token.expires_in as i64);
                    info!("expires in {expires_in:?} seconds");
                    let api_token = RspotifyToken {
                        access_token: token.access_token,
                        expires_in,
                        expires_at: Some(Utc::now() + expires_in),
                        ..RspotifyToken::default()
                    };

                    if self.rest_future.is_none() {
                        info!("Rest future is none");
                        self.spotify_client = Arc::new(AuthCodeSpotify::from_token(api_token));

                        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                        self.event_tx = Some(tx);
                        self.rest_future = Some(Box::pin(create_rest_server(
                            Arc::clone(&self.spotify_client),
                            Arc::clone(&self.event_tx_something),
                            self.spirc.clone(),
                            self.device_name.clone(),
                            rx,
                        )));
                    } else {
                        info!("rest future is not none");
                        *self.spotify_client.get_token().lock().unwrap() = Some(api_token);
                    }
                } else {
                    info!("token request is not ready yet");
                    self.token_request = Some(fut);
                }
            } else {
                info!("create a new future to use to get tokens");
                self.token_request = Some(Box::pin({
                    let sess = self.session.clone();
                    // This is more meant as a fast hotfix than anything else!
                    let client_id =
                        env::var("SPOTIFYD_CLIENT_ID").unwrap_or_else(|_| CLIENT_ID.to_string());
                    async move { get_token(&sess, &client_id, SCOPE).await }
                }));
            }
        }

        // Not polling the future here in some cases is fine, since we will poll it
        // immediately after the token request has completed.
        // If we would poll the future in any case, we would risk using invalid tokens for API requests.
        if self.token_request.is_none() {
            if let Some(ref mut fut) = self.rest_future {
                return fut.as_mut().poll(cx);
            }
        }

        info!("pending rest");
        Poll::Pending
    }
}

async fn generate_response(error: bool) -> Result<Json<Value>, AppError> {
    if error {
        Err(AppError::Playback(SError::StatusError))
    } else {
        Ok(Json(json!({"res": "done"})))
    }
}

#[derive(Debug)]
enum SError {
    #[allow(dead_code)]
    NotFound,
    #[allow(dead_code)]
    StatusError,
    #[allow(dead_code)]
    AuthError,
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
            AppError::Playback(SError::AuthError) => (StatusCode::UNAUTHORIZED, "Unauthorized"),
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

async fn playback(
    sp_client: Arc<AuthCodeSpotify>,
    tx_copy: Arc<UnboundedSender<String>>,
) -> Result<Json<Value>, AppError> {
    match tx_copy.send("Getting Playback".to_string()) {
        Ok(_) => {
            info!("Getting Playback SENT");
        }
        Err(_) => {
            info!("Getting Playback ERROR");
        }
    }
    let val = sp_client
        .current_playback(None, None::<Vec<_>>)
        .map_err(|e| get_error(e))?;
    match val {
        Some(val) => Ok(Json(json!(val))),
        None => Ok(Json(json!({}))),
    }
}

async fn shuffle(
    state: bool,
    device_name: String,
    sp_client: Arc<AuthCodeSpotify>,
    tx_copy: Arc<UnboundedSender<String>>,
) -> Result<Json<Value>, AppError> {
    match tx_copy.send("Shuffling".to_string()) {
        Ok(_) => {
            info!("shuffle is sent");
        }
        Err(_) => {
            info!("shuffle ERROR");
        }
    }
    let device_id = get_device_id(device_name.clone(), sp_client.clone());
    trace!("{device_id:?}");
    sp_client
        .shuffle(state, device_id.as_deref())
        .map_err(|e| get_error(e))?;
    Ok(Json(json!({"res": "done"})))
}

async fn get_category_playlist(
    category_id: String,
    sp_client: Arc<AuthCodeSpotify>,
    tx_copy: Arc<UnboundedSender<String>>,
) -> Result<Json<Value>, AppError> {
    match tx_copy.send("Getting category playlist".to_string()) {
        Ok(_) => {
            info!("Getting category playlist is sent");
        }
        Err(_) => {
            info!("Getting category playlist ERROR");
        }
    }
    let res = sp_client
        .category_playlists_manual(&category_id, None, Some(30), None)
        .map_err(|e| get_error(e))?;
    Ok(Json(json!(res)))
}

async fn repeat(
    payload_state: String,
    device_name: String,
    sp_client: Arc<AuthCodeSpotify>,
    tx_copy: Arc<UnboundedSender<String>>,
) -> Result<Json<Value>, AppError> {
    match tx_copy.send("Repeating".to_string()) {
        Ok(_) => {
            info!("Repeat is sent");
        }
        Err(_) => {
            info!("Repeat ERROR");
        }
    }
    let state = match payload_state.as_str() {
        "track" => RepeatState::Track,
        "context" => RepeatState::Context,
        "off" => RepeatState::Off,
        _ => RepeatState::Off,
    };
    let device_id = get_device_id(device_name, sp_client.clone());
    sp_client
        .repeat(&state, device_id.as_deref())
        .map_err(|e| get_error(e))?;
    Ok(Json(json!({"res": "done"})))
}

fn get_error(e: ClientError) -> SError {
    error!("{e:?}");
    match e {
        rspotify::ClientError::Http(_) => SError::AuthError,
        _ => SError::StatusError,
    }
}

async fn search(
    query: String,
    sp_client: Arc<AuthCodeSpotify>,
    tx_copy: Arc<UnboundedSender<String>>,
) -> Result<Json<Value>, AppError> {
    match tx_copy.send("SEARCH IS HAPPENING".to_string()) {
        Ok(_) => {
            info!("SEARCH WAS SENT");
        }
        Err(_) => {
            info!("SEARCH ERROR");
        }
    }
    let tracks = sp_client
        .search(&query, &SearchType::Track, None, None, Some(6), None)
        .map_err(|e| get_error(e))?;
    let albums = sp_client
        .search(&query, &SearchType::Album, None, None, Some(6), None)
        .map_err(|e| get_error(e))?;
    let artists = sp_client
        .search(&query, &SearchType::Artist, None, None, Some(6), None)
        .map_err(|e| get_error(e))?;
    let playlists = sp_client
        .search(&query, &SearchType::Playlist, None, None, Some(6), None)
        .map_err(|e| get_error(e))?;
    Ok(Json(
        json!({"tracks":tracks, "albums":albums, "artists":artists, "playlists":playlists}),
    ))
}

async fn open_ur(
    sp_client: Arc<AuthCodeSpotify>,
    uri: String,
    device_name: String,
    tx_copy: Arc<UnboundedSender<String>>,
) -> Result<Json<Value>, AppError> {
    match tx_copy.send("Openning URI".to_string()) {
        Ok(_) => {
            info!("Openning URI SENT");
        }
        Err(_) => {
            info!("Openning URI ERROR");
        }
    }
    struct AnyContextId(Box<dyn PlayContextId>);

    impl Id for AnyContextId {
        fn id(&self) -> &str {
            self.0.id()
        }

        fn _type(&self) -> Type {
            self.0._type()
        }

        fn _type_static() -> Type
        where
            Self: Sized,
        {
            unreachable!("never called");
        }

        unsafe fn from_id_unchecked(_id: &str) -> Self
        where
            Self: Sized,
        {
            unreachable!("never called");
        }
    }
    impl PlayContextId for AnyContextId {}

    enum Uri {
        Playable(Box<dyn PlayableId>),
        Context(AnyContextId),
    }

    impl Uri {
        fn from_id(id_type: Type, id: &str) -> Result<Uri, IdError> {
            use Uri::*;
            let uri = match id_type {
                Type::Track => Playable(Box::new(TrackId::from_id(id)?)),
                Type::Episode => Playable(Box::new(EpisodeId::from_id(id)?)),
                Type::Artist => Context(AnyContextId(Box::new(ArtistId::from_id(id)?))),
                Type::Album => Context(AnyContextId(Box::new(AlbumId::from_id(id)?))),
                Type::Playlist => Context(AnyContextId(Box::new(PlaylistId::from_id(id)?))),
                Type::Show => Context(AnyContextId(Box::new(ShowId::from_id(id)?))),
                Type::User | Type::Collection => return Err(IdError::InvalidType),
            };
            Ok(uri)
        }
    }

    let mut chars = uri
        .strip_prefix("spotify")
        .ok_or_else(|| AppError::Playback(SError::NotFound))?
        .chars();

    let sep = match chars.next() {
        Some(ch) if ch == '/' || ch == ':' => ch,
        _ => return Err(AppError::Playback(SError::NotFound)),
    };
    let rest = chars.as_str();

    let (id_type, id) = rest
        .rsplit_once(sep)
        .and_then(|(id_type, id)| Some((id_type.parse::<Type>().ok()?, id)))
        .ok_or_else(|| AppError::Playback(SError::NotFound))?;

    let uri = Uri::from_id(id_type, id).map_err(|_| AppError::Playback(SError::NotFound))?;

    let device_id = get_device_id(device_name, sp_client.clone());
    trace!("device id: {device_id:?}");
    let res;
    match uri {
        Uri::Playable(id) => {
            res = sp_client.start_uris_playback(
                Some(id.as_ref()),
                device_id.as_deref(),
                Some(Offset::for_position(0)),
                None,
            );
            trace!("{res:?}");
        }

        Uri::Context(id) => {
            match id_type {
                Type::Artist => {
                    res = sp_client.start_context_playback(&id, device_id.as_deref(), None, None);
                }
                _ => {
                    res = sp_client.start_context_playback(
                        &id,
                        device_id.as_deref(),
                        Some(Offset::for_position(0)),
                        None,
                    );
                }
            }
            trace!("{res:?}");
        }
    }
    if res.is_err() {
        generate_response(true).await
    } else {
        generate_response(false).await
    }
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

fn get_device_id(mv_device_name: String, sp_client: Arc<AuthCodeSpotify>) -> Option<String> {
    sp_client.device().ok().and_then(|devices| {
        devices
            .into_iter()
            .find_map(|d| if d.name == mv_device_name { d.id } else { None })
    })
}

async fn create_rest_server(
    // api_token: RspotifyToken,
    spotify_api_client: Arc<AuthCodeSpotify>,
    tx_something: Arc<UnboundedSender<String>>,
    spirc: Arc<Spirc>,
    device_name: String,
    mut event_rx: UnboundedReceiver<PlayerEvent>,
) {
    trace!("creating rest server");
    let app = axum::Router::new()
        .route(
            "/shutdown",
            routing::get({
                let local_spirc = Arc::clone(&spirc);
                move || {
                    local_spirc.shutdown();
                    generate_response(false)
                }
            }),
        )
        .route(
            "/play",
            routing::get({
                let local_spirc = Arc::clone(&spirc);
                move || {
                    local_spirc.play();
                    generate_response(false)
                }
            }),
        )
        .route(
            "/pause",
            routing::get({
                let local_spirc = Arc::clone(&spirc);
                move || {
                    local_spirc.pause();
                    generate_response(false)
                }
            }),
        )
        .route(
            "/play_pause",
            routing::get({
                let local_spirc = Arc::clone(&spirc);
                move || {
                    local_spirc.play_pause();
                    generate_response(false)
                }
            }),
        )
        .route(
            "/next",
            routing::get({
                let local_spirc = Arc::clone(&spirc);
                move || {
                    local_spirc.next();
                    generate_response(false)
                }
            }),
        )
        .route(
            "/previous",
            routing::get({
                let local_spirc = Arc::clone(&spirc);
                move || {
                    local_spirc.prev();
                    generate_response(false)
                }
            }),
        )
        .route(
            "/seek",
            routing::post({
                // let mv_device_name = device_name.clone();
                // let mv_api_token = api_token.clone();
                let sp_client = Arc::clone(&spotify_api_client);
                let tx_copy: Arc<UnboundedSender<String>> = Arc::clone(&tx_something);
                move |Json(payload): Json<SeekItem>| {
                    // let sp = create_spotify_api(&mv_api_token);
                    tx_copy.send("Seeking".to_string());
                    info!("pos: {}", payload.pos);
                    if let Ok(Some(playing)) = sp_client.current_playback(None, None::<Vec<_>>) {
                        info!("current pos: {:?}", playing.progress);
                        let res = sp_client.seek_track(payload.pos, playing.device.id.as_deref());
                        info!("{res:?}");
                    }
                    generate_response(false)
                }
            }),
        )
        .route(
            "/get_category_playlist",
            routing::post({
                // let mv_device_name = device_name.clone();
                let sp_client = Arc::clone(&spotify_api_client);
                let tx_copy: Arc<UnboundedSender<String>> = Arc::clone(&tx_something);
                move |Json(payload): Json<StringItem>| {
                    get_category_playlist(payload.val, sp_client, tx_copy)
                }
            }),
        )
        .route(
            "/repeat",
            routing::post({
                let sp_client = Arc::clone(&spotify_api_client);
                let mv_device_name = device_name.clone();
                let tx_copy: Arc<UnboundedSender<String>> = Arc::clone(&tx_something);
                move |Json(payload): Json<StringItem>| {
                    repeat(payload.val, mv_device_name, sp_client, tx_copy)
                }
            }),
        )
        .route(
            "/shuffle",
            routing::post({
                let mv_device_name = device_name.clone();
                let sp_client = Arc::clone(&spotify_api_client);
                let tx_copy: Arc<UnboundedSender<String>> = Arc::clone(&tx_something);
                move |Json(payload): Json<BoolItem>| {
                    shuffle(payload.val, mv_device_name, sp_client, tx_copy)
                }
            }),
        )
        .route(
            "/search",
            routing::post({
                let tx_copy: Arc<UnboundedSender<String>> = Arc::clone(&tx_something);
                let sp_client = Arc::clone(&spotify_api_client);
                move |Json(payload): Json<SearchItem>| search(payload.keyword, sp_client, tx_copy)
            }),
        )
        .route(
            "/player_status",
            routing::get({
                let sp_client = Arc::clone(&spotify_api_client);
                let tx_copy: Arc<UnboundedSender<String>> = Arc::clone(&tx_something);
                move || playback(sp_client, tx_copy)
            }),
        )
        .route(
            "/transfer_playback",
            routing::get({
                let mv_device_name = device_name.clone();
                let sp_client = Arc::clone(&spotify_api_client);
                let tx_copy: Arc<UnboundedSender<String>> = Arc::clone(&tx_something);
                move || match get_device_id(mv_device_name, sp_client.clone()) {
                    Some(device_id) => {
                        tx_copy.send("Transfering playback".to_string());
                        let _ = sp_client.transfer_playback(&device_id, Some(false));
                        generate_response(false)
                    }
                    None => generate_response(true),
                }
            }),
        )
        .route(
            "/volume",
            routing::post({
                let mv_device_name = device_name.clone();
                let sp_client = Arc::clone(&spotify_api_client);
                let tx_copy: Arc<UnboundedSender<String>> = Arc::clone(&tx_something);
                move |Json(payload): Json<VolumeItem>| {
                    let device_id = get_device_id(mv_device_name, sp_client.clone());
                    tx_copy.send("Changing volume".to_string());
                    match sp_client.volume(payload.vol, device_id.as_deref()) {
                        Ok(_) => generate_response(false),
                        Err(_) => generate_response(true),
                    }
                }
            }),
        )
        .route(
            "/open_uri",
            routing::post({
                let mv_device_name = device_name.clone();
                let sp_client = Arc::clone(&spotify_api_client);
                let tx_copy: Arc<UnboundedSender<String>> = Arc::clone(&tx_something);
                move |Json(payload): Json<UriItem>| {
                    open_ur(sp_client, payload.uri, mv_device_name, tx_copy)
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
