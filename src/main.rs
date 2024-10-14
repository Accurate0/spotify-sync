use anyhow::Context;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::get,
    Router,
};
use chrono::FixedOffset;
use config::Environment;
use futures::{StreamExt, TryStreamExt};
use rspotify::{
    model::{Id, PlayableId, PlaylistId, TrackId},
    prelude::{BaseClient, OAuthClient},
    AuthCodeSpotify, Credentials,
};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tracing::Level;
use tracing_subscriber::{filter::Targets, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(serde::Deserialize)]
struct CallbackQueryParams {
    code: String,
}

#[derive(serde::Deserialize)]
struct Settings {
    #[serde(rename = "playlist_id")]
    liked_songs_playlist_id: String,
    discover_weekly_archive_playlist_id: String,
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

const CRON_EXPR: &str = "0 0 20 ? * SUN *";

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
    songs_to_add: &HashSet<String>,
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

    let items_to_add = songs_to_add
        .difference(&existing_playlist_items)
        .map(|i| PlayableId::Track(TrackId::from_uri(i).unwrap()))
        .collect::<Vec<_>>();

    let items_to_remove = existing_playlist_items
        .difference(songs_to_add)
        .map(|i| PlayableId::Track(TrackId::from_uri(i).unwrap()))
        .collect::<Vec<_>>();

    tracing::info!(
        "change summary: items_to_add -> {}, items_to_remove -> {}",
        items_to_add.len(),
        items_to_remove.len()
    );

    let mut updated = false;

    // FIXME: https://developer.spotify.com/documentation/web-api/reference/reorder-or-replace-playlists-tracks
    // Use this, less requests etc
    if !items_to_remove.is_empty() && action.contains(&PlaylistAction::Remove) {
        // FIXME: max is 100, else 400 bad request, paginate and use other API
        spotify_client
            .playlist_remove_all_occurrences_of_items(playlist_id.clone(), items_to_remove, None)
            .await?;
        updated = true;
    }

    if !items_to_add.is_empty() && action.contains(&PlaylistAction::Add) {
        spotify_client
            .playlist_add_items(playlist_id.clone(), items_to_add, None)
            .await?;
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

async fn discover_weekly_archive(state: &AppState) -> anyhow::Result<()> {
    let spotify_client = state.0.spotify_client.lock().await;
    let mut user_playlists = spotify_client.current_user_playlists();

    let mut discover_weekly_playlist = None;
    while let Some(playlist) = user_playlists.next().await {
        let playlist = playlist?;
        if playlist.name == "Discover Weekly"
            && playlist.owner.id.to_string() == "spotify:user:spotify"
        {
            discover_weekly_playlist = Some(playlist);
            break;
        }
    }

    if discover_weekly_playlist.is_none() {
        tracing::warn!("no discovery weekly playlist found..");
        return Ok(());
    }

    let discover_weekly_playlist_id = discover_weekly_playlist.map(|p| p.id).unwrap();
    let discover_weekly_playlist_items = spotify_client
        .playlist_items(discover_weekly_playlist_id, None, None)
        .try_filter_map(|s| async move {
            Ok(s.track
                .and_then(|t| t.id().map(|t| t.clone_static()))
                .map(|s| s.uri()))
        })
        .try_collect::<HashSet<_>>()
        .await?;

    let mut actions = HashSet::with_capacity(1);
    actions.insert(PlaylistAction::Add);

    diff_and_update_playlist(
        &spotify_client,
        &state.0.settings.discover_weekly_archive_playlist_id,
        &discover_weekly_playlist_items,
        &actions,
    )
    .await?;

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
    .await?;

    let mut archive_actions = HashSet::with_capacity(2);
    archive_actions.insert(PlaylistAction::Add);

    diff_and_update_playlist(
        &spotify_client,
        &state.0.settings.liked_songs_archive_playlist_id,
        &liked_songs,
        &archive_actions,
    )
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

    let cron_expr = CRON_EXPR.parse::<cron::Schedule>()?;
    let discover_weekly_state = state.clone();
    let offset = FixedOffset::east_opt(8 * 3600).context("must have correct offset")?;
    let discover_weekly_handle = tokio::spawn(async move {
        loop {
            let utc = chrono::Utc::now().naive_utc();
            let time_now = chrono::DateTime::<FixedOffset>::from_naive_utc_and_offset(utc, offset);
            let next = cron_expr.after(&time_now).next().unwrap();

            let time_until = if time_now >= next {
                Duration::ZERO
            } else {
                (next - time_now).to_std().unwrap()
            };

            tracing::info!("next archive in {}s at {}", time_until.as_secs(), next);
            tokio::time::sleep(time_until).await;

            if let Err(e) = discover_weekly_archive(&discover_weekly_state).await {
                tracing::error!("error in sync: {e}")
            }
        }
    });

    let sync_state = state.clone();
    let sync_handle = tokio::spawn(async move {
        loop {
            if let Err(e) = sync_and_archive_liked_songs(&sync_state).await {
                tracing::error!("error in sync: {e}")
            }

            tokio::time::sleep(Duration::from_secs(900)).await
        }
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/callback", get(callback))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8000").await?;
    tokio::select! {
        _ = axum::serve(listener, app) => {
            tracing::error!("axum exited")
        }
        _ = sync_handle => {
            tracing::error!("sync handle exited")
        }
        _ = discover_weekly_handle => {
            tracing::error!("sync handle exited")
        }
    }

    Ok(())
}
