use anyhow::Result;
use std::sync::OnceLock;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

pub const API_ADDR: &str = "http://127.0.0.1:50051";

pub mod proto {
    tonic::include_proto!("player");
}

pub use proto::{
    ClearAuthResponse, Empty, HealthResponse, LyricsRequest, LyricsResponse, PlaylistTracksRequest,
    PlaylistTracksResponse, PollOAuthResponse, PrefetchResolveRequest, RecommendRequest,
    ResolveRequest, ResolveResponse, SearchRequest, SetBrowserAuthRequest, SetBrowserAuthResponse,
    StartOAuthRequest, StartOAuthResponse, Track, UserPlaylistsResponse,
    player_api_client::PlayerApiClient,
};

static API_TOKEN: OnceLock<String> = OnceLock::new();

pub fn set_api_token(token: String) {
    let _ = API_TOKEN.set(token);
}

pub async fn connect() -> Result<
    PlayerApiClient<
        tonic::service::interceptor::InterceptedService<Channel, impl tonic::service::Interceptor>,
    >,
> {
    let channel = Channel::from_static(API_ADDR).connect_lazy();
    let token = API_TOKEN.get().cloned().unwrap_or_default();
    let client = PlayerApiClient::with_interceptor(channel, move |mut req: tonic::Request<()>| {
        if !token.is_empty()
            && let Ok(value) = MetadataValue::try_from(token.clone())
        {
            req.metadata_mut().insert("x-fasttube-token", value);
        }
        Ok(req)
    });
    Ok(client)
}

pub async fn health() -> Result<HealthResponse> {
    let mut client = connect().await?;
    let resp = client.health(tonic::Request::new(Empty {})).await?;
    Ok(resp.into_inner())
}

pub async fn resolve(video_id: &str, quality: &str) -> Result<ResolveResponse> {
    resolve_impl(video_id, quality, false).await
}

pub async fn resolve_forced(video_id: &str, quality: &str) -> Result<ResolveResponse> {
    resolve_impl(video_id, quality, true).await
}

async fn resolve_impl(video_id: &str, quality: &str, force: bool) -> Result<ResolveResponse> {
    let mut client = connect().await?;
    let resp = client
        .resolve_url(tonic::Request::new(ResolveRequest {
            video_id: video_id.to_string(),
            audio_quality: quality.to_string(),
            force,
        }))
        .await?;
    Ok(resp.into_inner())
}

pub async fn prefetch_resolve(ids: &[String], quality: &str) -> Result<i32> {
    let mut client = connect().await?;
    let resp = client
        .prefetch_resolve(tonic::Request::new(PrefetchResolveRequest {
            ids: ids.to_vec(),
            audio_quality: quality.to_string(),
        }))
        .await?;
    Ok(resp.into_inner().resolved)
}

pub async fn search(query: &str, limit: i32, music: bool) -> Result<Vec<Track>> {
    let mut client = connect().await?;
    let resp = client
        .search(tonic::Request::new(SearchRequest {
            query: query.to_string(),
            limit,
            music,
        }))
        .await?;
    Ok(resp.into_inner().tracks)
}

pub async fn get_user_playlists() -> Result<UserPlaylistsResponse> {
    let mut client = connect().await?;
    let resp = client
        .get_user_playlists(tonic::Request::new(Empty {}))
        .await?;
    Ok(resp.into_inner())
}

pub async fn get_playlist_tracks(playlist_id: &str) -> Result<PlaylistTracksResponse> {
    let mut client = connect().await?;
    let resp = client
        .get_playlist_tracks(tonic::Request::new(PlaylistTracksRequest {
            id: playlist_id.to_string(),
        }))
        .await?;
    Ok(resp.into_inner())
}

pub async fn get_liked_songs() -> Result<PlaylistTracksResponse> {
    let mut client = connect().await?;
    let resp = client
        .get_liked_songs(tonic::Request::new(Empty {}))
        .await?;
    Ok(resp.into_inner())
}

pub async fn start_oauth(client_id: &str, client_secret: &str) -> Result<StartOAuthResponse> {
    let mut client = connect().await?;
    let resp = client
        .start_o_auth(tonic::Request::new(StartOAuthRequest {
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
        }))
        .await?;
    Ok(resp.into_inner())
}

pub async fn poll_oauth() -> Result<PollOAuthResponse> {
    let mut client = connect().await?;
    let resp = client.poll_o_auth(tonic::Request::new(Empty {})).await?;
    Ok(resp.into_inner())
}

pub async fn set_browser_auth(auth_text: &str) -> Result<SetBrowserAuthResponse> {
    let mut client = connect().await?;
    let resp = client
        .set_browser_auth(tonic::Request::new(SetBrowserAuthRequest {
            auth_text: auth_text.to_string(),
        }))
        .await?;
    Ok(resp.into_inner())
}

pub async fn clear_auth() -> Result<ClearAuthResponse> {
    let mut client = connect().await?;
    let resp = client.clear_auth(tonic::Request::new(Empty {})).await?;
    Ok(resp.into_inner())
}

pub async fn get_lyrics(artist: &str, title: &str, duration_secs: f64) -> Result<LyricsResponse> {
    let mut client = connect().await?;
    let resp = client
        .get_lyrics(tonic::Request::new(LyricsRequest {
            artist: artist.to_string(),
            title: title.to_string(),
            duration_secs,
        }))
        .await?;
    Ok(resp.into_inner())
}

pub async fn recommend(video_id: &str, limit: i32) -> Result<Vec<Track>> {
    let mut client = connect().await?;
    let resp = client
        .recommend(tonic::Request::new(RecommendRequest {
            video_id: video_id.to_string(),
            limit,
        }))
        .await?;
    Ok(resp.into_inner().tracks)
}
