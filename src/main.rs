use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::get,
    Router,
};
use chrono::FixedOffset;
use config::Environment;
use futures::TryStreamExt;
use rspotify::{
    model::{Id, PlayableId, PlaylistId, TrackId},
    prelude::{BaseClient, OAuthClient},
    AuthCodeSpotify, Credentials,
};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio::sync::Mutex;

#[derive(serde::Deserialize)]
struct CallbackQueryParams {
    code: String,
}

#[derive(serde::Deserialize)]
struct Settings {
    playlist_id: String,
    cache_path: String,
    redirect_uri: String,
    client_id: String,
    client_secret: String,
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

async fn sync_task(state: &AppState) -> anyhow::Result<()> {
    let spotify_client = state.0.spotify_client.lock().await;

    let liked_songs = spotify_client
        .current_user_saved_tracks(None)
        .try_filter_map(|s| async move { Ok(s.track.id.map(PlayableId::Track).map(|s| s.uri())) })
        .try_collect::<HashSet<_>>()
        .await?;

    tracing::info!("song count: {}", liked_songs.len());

    let playlist_id = PlaylistId::from_id(&state.0.settings.playlist_id)?;

    let existing_playlist_items = spotify_client
        .playlist_items(playlist_id.clone(), None, None)
        .try_filter_map(|s| async move {
            Ok(s.track
                .and_then(|t| t.id().map(|t| t.clone_static()))
                .map(|s| s.uri()))
        })
        .try_collect::<HashSet<_>>()
        .await?;

    let items_to_add = liked_songs
        .difference(&existing_playlist_items)
        .collect::<Vec<_>>();

    let items_to_remove = existing_playlist_items
        .difference(&liked_songs)
        .collect::<Vec<_>>();

    let mut updated = false;

    // FIXME: https://developer.spotify.com/documentation/web-api/reference/reorder-or-replace-playlists-tracks
    // Use this, less requests etc
    if !items_to_remove.is_empty() {
        // FIXME: max is 100, else 400 bad request, paginate and use other API
        spotify_client
            .playlist_remove_all_occurrences_of_items(
                playlist_id.clone(),
                items_to_remove
                    .iter()
                    .map(|i| PlayableId::Track(TrackId::from_uri(i).unwrap())),
                None,
            )
            .await?;
        updated = true;
    }

    if !items_to_add.is_empty() {
        spotify_client
            .playlist_add_items(
                playlist_id.clone(),
                items_to_add
                    .iter()
                    .map(|i| PlayableId::Track(TrackId::from_uri(i).unwrap())),
                None,
            )
            .await?;
        updated = true;
    }

    if updated {
        let offset = FixedOffset::east_opt(8 * 3600).unwrap();
        let last_updated = chrono::Local::now()
            .with_timezone(&offset)
            .format("%d/%m/%Y %I:%M %P");
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
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
        scopes: rspotify::scopes!("user-library-read", "playlist-modify-public"),
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
    let sync_handle = tokio::spawn(async move {
        loop {
            if let Err(e) = sync_task(&sync_state).await {
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
    }

    Ok(())
}
