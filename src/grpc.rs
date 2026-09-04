use anyhow::Result;
use tonic::transport::Channel;

pub const API_ADDR: &str = "http://127.0.0.1:50051";

pub mod proto {
    tonic::include_proto!("player");
}

pub use proto::{
    Empty, HealthResponse, LyricsRequest, LyricsResponse, PlaylistTracksRequest,
    PlaylistTracksResponse, PollOAuthResponse, RecommendRequest, ResolveRequest, ResolveResponse,
    SearchRequest, SetBrowserAuthRequest, SetBrowserAuthResponse, StartOAuthRequest,
    StartOAuthResponse, Track, UserPlaylistsResponse, player_api_client::PlayerApiClient,
};

pub async fn connect() -> Result<PlayerApiClient<Channel>> {
    Ok(PlayerApiClient::connect(API_ADDR.to_string()).await?)
}

pub async fn health() -> Result<HealthResponse> {
    let mut client = connect().await?;
    let resp = client.health(tonic::Request::new(Empty {})).await?;
    Ok(resp.into_inner())
}

pub async fn resolve(video_id: &str, quality: &str) -> Result<ResolveResponse> {
    let mut client = connect().await?;
    let resp = client
        .resolve_url(tonic::Request::new(ResolveRequest {
            video_id: video_id.to_string(),
            audio_quality: quality.to_string(),
        }))
        .await?;
    Ok(resp.into_inner())
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
