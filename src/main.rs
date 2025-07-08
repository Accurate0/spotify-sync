use axum::{
    extract::{MatchedPath, Query, State},
    http::StatusCode,
    routing::get,
    Router,
};
use chrono::FixedOffset;
use config::Environment;
use futures::TryStreamExt;
use http::Request;
use itertools::Itertools;
use rspotify::{
    model::{Id, PlayableId, PlaylistId, TrackId},
    prelude::{BaseClient, OAuthClient},
    AuthCodeSpotify, Credentials,
};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tower_http::{
    trace::{DefaultOnRequest, DefaultOnResponse, TraceLayer},
    LatencyUnit,
};
use tracing::{Instrument, Level};
use tracing_subscriber::{filter::Targets, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(serde::Deserialize)]
struct CallbackQueryParams {
    code: String,
}

#[derive(serde::Deserialize)]
struct Settings {
    #[serde(rename = "playlist_id")]
    liked_songs_playlist_id: String,
    liked_songs_archive_playlist_id: String,
    cache_path: String,
    redirect_uri: String,
    client_id: String,
    client_secret: String,
}

#[derive(PartialEq, Eq, Hash)]
enum PlaylistAction {
    Add,
    Remove,
}

struct AppStateInner {
    settings: Settings,
    oauth: rspotify::OAuth,
    spotify_client: Mutex<AuthCodeSpotify>,
}

#[derive(Clone)]
struct AppState(Arc<AppStateInner>);

async fn health() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn callback(State(state): State<AppState>, query: Query<CallbackQueryParams>) {
    let settings = &state.0.settings;
    let config = rspotify::Config {
        token_cached: true,
        cache_path: settings.cache_path.to_owned().into(),
        ..Default::default()
    };

    let oauth = state.0.oauth.clone();

    let creds = Credentials::new(&settings.client_id, &settings.client_secret);
    let spotify_client = AuthCodeSpotify::with_config(creds, oauth, config);
    let result = spotify_client.request_token(&query.code).await;
    match result {
        Ok(()) => {
            let mut existing_client = state.0.spotify_client.lock().await;
            let _ = std::mem::replace(&mut *existing_client, spotify_client);
        }
        Err(e) => tracing::error!("error fetching token: {e}"),
    }
}

async fn diff_and_update_playlist(
    spotify_client: &AuthCodeSpotify,
    playlist_to_update: &String,
    liked_songs: &HashSet<String>,
    action: &HashSet<PlaylistAction>,
) -> anyhow::Result<()> {
    let playlist_id = PlaylistId::from_id(playlist_to_update)?;

    let existing_playlist_items = spotify_client
        .playlist_items(playlist_id.clone(), None, None)
        .try_filter_map(|s| async move {
            Ok(s.track
                .and_then(|t| t.id().map(|t| t.clone_static()))
                .map(|s| s.uri()))
        })
        .try_collect::<HashSet<_>>()
        .await?;

    tracing::info!("{}", existing_playlist_items.len());
    tracing::info!("{}", liked_songs.len());
    let mut difference = liked_songs
        .difference(&existing_playlist_items)
        .map(|i| PlayableId::Track(TrackId::from_uri(i).unwrap()))
        .collect::<HashSet<_>>();

    let difference2 = existing_playlist_items
        .difference(liked_songs)
        .map(|i| PlayableId::Track(TrackId::from_uri(i).unwrap()))
        .collect::<HashSet<_>>();

    difference.extend(difference2);

    tracing::info!("{difference:?}");
    let mut items_to_add = Vec::new();
    let mut items_to_remove = Vec::new();

    for different_item in difference {
        if liked_songs.contains(&different_item.uri()) {
            items_to_remove.push(different_item);
        } else if existing_playlist_items.contains(&different_item.uri()) {
            items_to_add.push(different_item);
        }
    }

    tracing::info!(
        "change summary: items_to_add -> {}, items_to_remove -> {}",
        items_to_add.len(),
        items_to_remove.len()
    );

    let mut updated = false;

    if !items_to_remove.is_empty() && action.contains(&PlaylistAction::Remove) {
        let chunks = items_to_remove.into_iter().chunks(100);
        let remove_requests = chunks
            .into_iter()
            .map(|c| c.collect_vec())
            .map(|c| {
                let playlist_id = playlist_id.clone();

                Box::pin(async move {
                    let r = spotify_client
                        .playlist_remove_all_occurrences_of_items(playlist_id.clone(), c, None)
                        .await?;

                    tracing::info!("{r:?}");

                    Ok::<(), anyhow::Error>(())
                })
            })
            .collect_vec();

        let results = futures::future::join_all(remove_requests).await;

        let errors = results
            .into_iter()
            .filter(|r| r.is_err())
            .map(|r| r.err().unwrap())
            .collect_vec();

        tracing::error!("errors: {errors:?}");
        updated = true;
    }

    if !items_to_add.is_empty() && action.contains(&PlaylistAction::Add) {
        let chunks = items_to_add.into_iter().chunks(100);
        let add_requests = chunks
            .into_iter()
            .map(|c| c.collect_vec())
            .map(|c| {
                let playlist_id = playlist_id.clone();

                Box::pin(async move {
                    let r = spotify_client
                        .playlist_add_items(playlist_id.clone(), c, None)
                        .await?;

                    tracing::info!("{r:?}");

                    Ok::<(), anyhow::Error>(())
                })
            })
            .collect_vec();

        let results = futures::future::join_all(add_requests).await;
        let errors = results
            .into_iter()
            .filter(|r| r.is_err())
            .map(|r| r.err().unwrap())
            .collect_vec();

        tracing::error!("errors: {errors:?}");
        updated = true;
    }

    if updated {
        let offset = FixedOffset::east_opt(8 * 3600).unwrap();
        let last_updated = chrono::Local::now()
            .with_timezone(&offset)
            .format("%d/%m/%Y %I:%M %P");

        tracing::info!("update done at {}", last_updated);
        spotify_client
            .playlist_change_detail(
                playlist_id,
                None,
                None,
                Some(&format!("Last updated: {}", last_updated)),
                None,
            )
            .await?;
    }

    Ok(())
}

async fn sync_and_archive_liked_songs(state: &AppState) -> anyhow::Result<()> {
    tracing::info!("starting sync");
    let spotify_client = state.0.spotify_client.lock().await;

    let liked_songs = spotify_client
        .current_user_saved_tracks(None)
        .try_filter_map(|s| async move { Ok(s.track.id.map(PlayableId::Track).map(|s| s.uri())) })
        .try_collect::<HashSet<_>>()
        .await?;

    tracing::info!("liked song count: {}", liked_songs.len());

    let mut sync_actions = HashSet::with_capacity(2);
    sync_actions.insert(PlaylistAction::Add);
    sync_actions.insert(PlaylistAction::Remove);

    diff_and_update_playlist(
        &spotify_client,
        &state.0.settings.liked_songs_playlist_id,
        &liked_songs,
        &sync_actions,
    )
    .instrument(tracing::span!(Level::INFO, "sync"))
    .await?;

    let mut archive_actions = HashSet::with_capacity(2);
    archive_actions.insert(PlaylistAction::Add);

    diff_and_update_playlist(
        &spotify_client,
        &state.0.settings.liked_songs_archive_playlist_id,
        &liked_songs,
        &archive_actions,
    )
    .instrument(tracing::span!(Level::INFO, "archive"))
    .await?;

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            Targets::default()
                .with_target("rspotify_http::reqwest", Level::WARN)
                .with_default(Level::INFO),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let settings = config::Config::builder()
        .add_source(Environment::with_prefix("SPOTIFYSYNC"))
        .build()?
        .try_deserialize::<Settings>()?;

    let config = rspotify::Config {
        token_cached: true,
        cache_path: settings.cache_path.to_owned().into(),
        ..Default::default()
    };

    let oauth = rspotify::OAuth {
        scopes: rspotify::scopes!(
            "user-library-read",
            "playlist-read-private",
            "playlist-modify-public",
            "playlist-modify-private"
        ),
        redirect_uri: settings.redirect_uri.to_owned(),
        ..Default::default()
    };

    let creds = Credentials::new(&settings.client_id, &settings.client_secret);

    let token = rspotify::Token::from_cache(&settings.cache_path);
    let spotify_client = match token {
        Ok(token) => AuthCodeSpotify::from_token_with_config(token, creds, oauth.clone(), config),
        Err(_) => AuthCodeSpotify::with_config(creds, oauth.clone(), config),
    };

    let result = spotify_client.refresh_token().await;
    if let Err(e) = result {
        tracing::error!("error refreshing token: {e}")
    }

    let request_url = spotify_client.get_authorize_url(false)?;
    tracing::info!("url: {request_url}");

    let state = AppState(
        AppStateInner {
            settings,
            oauth,
            spotify_client: Mutex::new(spotify_client),
        }
        .into(),
    );

    let sync_state = state.clone();
    let sync_handle = tokio::spawn(
        async move {
            loop {
                if let Err(e) = sync_and_archive_liked_songs(&sync_state).await {
                    tracing::error!("error in sync: {e}")
                }

                tokio::time::sleep(Duration::from_secs(3600)).await
            }
        }
        .instrument(tracing::span!(Level::INFO, "liked-songs")),
    );

    let trace_layer = TraceLayer::new_for_http()
        .make_span_with(|request: &Request<_>| {
            let matched_path = request
                .extensions()
                .get::<MatchedPath>()
                .map(MatchedPath::as_str);

            tracing::info_span!("request", uri = matched_path)
        })
        .on_request(DefaultOnRequest::new().level(Level::INFO))
        .on_response(
            DefaultOnResponse::new()
                .level(Level::INFO)
                .latency_unit(LatencyUnit::Millis),
        );

    let app = Router::new()
        .route("/callback", get(callback))
        .with_state(state)
        .layer(trace_layer)
        .route("/health", get(health));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8000").await?;
    tokio::select! {
        _ = axum::serve(listener, app) => {
            tracing::error!("axum exited")
        }
        _ = sync_handle => {
            tracing::error!("sync handle exited")
        }
        // _ = discover_weekly_handle => {
        //     tracing::error!("sync handle exited")
        // }
    }

    Ok(())
}
