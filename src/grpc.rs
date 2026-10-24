use anyhow::Result;
use std::sync::OnceLock;
use std::time::Duration;
use tonic::metadata::MetadataValue;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

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

/// Deadline for one RPC. Resolving a stream URL runs yt-dlp extraction and is
/// allowed to take longer (see [`RESOLVE_TIMEOUT`]).
const CALL_TIMEOUT: Duration = Duration::from_secs(15);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(45);

/// One channel for the whole process: the sidecar reports its port once at
/// startup and every call reuses this connection instead of dialling again.
static CHANNEL: OnceLock<Channel> = OnceLock::new();
static API_TOKEN: OnceLock<String> = OnceLock::new();

#[derive(Clone)]
pub struct TokenInterceptor {
    token: String,
}

impl tonic::service::Interceptor for TokenInterceptor {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> std::result::Result<tonic::Request<()>, tonic::Status> {
        if let Ok(value) = MetadataValue::try_from(self.token.as_str()) {
            request.metadata_mut().insert("x-fasttube-token", value);
        }
        Ok(request)
    }
}

type Client = PlayerApiClient<InterceptedService<Channel, TokenInterceptor>>;

/// Binds the client to the port the sidecar printed on startup. Called by the
/// supervisor once; a restart on the same port keeps this channel.
pub fn set_endpoint(port: u16, token: String) -> Result<()> {
    let endpoint = Channel::from_shared(format!("http://127.0.0.1:{port}"))?
        .connect_timeout(Duration::from_secs(5))
        .tcp_nodelay(true);
    let _ = CHANNEL.set(endpoint.connect_lazy());
    let _ = API_TOKEN.set(token);
    Ok(())
}

async fn connect() -> Result<Client> {
    let channel = CHANNEL
        .get()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("sidecar endpoint is not configured yet"))?;
    let token = API_TOKEN.get().cloned().unwrap_or_default();
    Ok(PlayerApiClient::with_interceptor(
        channel,
        TokenInterceptor { token },
    ))
}

fn request<T>(message: T) -> tonic::Request<T> {
    with_timeout(message, CALL_TIMEOUT)
}

fn with_timeout<T>(message: T, timeout: Duration) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.set_timeout(timeout);
    request
}

pub async fn health() -> Result<HealthResponse> {
    let mut client = connect().await?;
    let resp = client.health(request(Empty {})).await?;
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
        .resolve_url(with_timeout(
            ResolveRequest {
                video_id: video_id.to_string(),
                audio_quality: quality.to_string(),
                force,
            },
            RESOLVE_TIMEOUT,
        ))
        .await?;
    Ok(resp.into_inner())
}

pub async fn prefetch_resolve(ids: &[String], quality: &str) -> Result<i32> {
    let mut client = connect().await?;
    let resp = client
        .prefetch_resolve(request(PrefetchResolveRequest {
            ids: ids.to_vec(),
            audio_quality: quality.to_string(),
        }))
        .await?;
    Ok(resp.into_inner().resolved)
}

pub async fn search(query: &str, limit: i32, music: bool) -> Result<Vec<Track>> {
    let mut client = connect().await?;
    let resp = client
        .search(request(SearchRequest {
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
        .get_user_playlists(request(Empty {}))
        .await?;
    Ok(resp.into_inner())
}

pub async fn get_playlist_tracks(playlist_id: &str) -> Result<PlaylistTracksResponse> {
    let mut client = connect().await?;
    let resp = client
        .get_playlist_tracks(request(PlaylistTracksRequest {
            id: playlist_id.to_string(),
        }))
        .await?;
    Ok(resp.into_inner())
}

pub async fn get_liked_songs() -> Result<PlaylistTracksResponse> {
    let mut client = connect().await?;
    let resp = client
        .get_liked_songs(request(Empty {}))
        .await?;
    Ok(resp.into_inner())
}

pub async fn start_oauth(client_id: &str, client_secret: &str) -> Result<StartOAuthResponse> {
    let mut client = connect().await?;
    let resp = client
        .start_o_auth(request(StartOAuthRequest {
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
        }))
        .await?;
    Ok(resp.into_inner())
}

pub async fn poll_oauth() -> Result<PollOAuthResponse> {
    let mut client = connect().await?;
    let resp = client.poll_o_auth(request(Empty {})).await?;
    Ok(resp.into_inner())
}

pub async fn set_browser_auth(auth_text: &str) -> Result<SetBrowserAuthResponse> {
    let mut client = connect().await?;
    let resp = client
        .set_browser_auth(request(SetBrowserAuthRequest {
            auth_text: auth_text.to_string(),
        }))
        .await?;
    Ok(resp.into_inner())
}

pub async fn clear_auth() -> Result<ClearAuthResponse> {
    let mut client = connect().await?;
    let resp = client.clear_auth(request(Empty {})).await?;
    Ok(resp.into_inner())
}

pub async fn get_lyrics(artist: &str, title: &str, duration_secs: f64) -> Result<LyricsResponse> {
    let mut client = connect().await?;
    let resp = client
        .get_lyrics(request(LyricsRequest {
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
        .recommend(request(RecommendRequest {
            video_id: video_id.to_string(),
            limit,
        }))
        .await?;
    Ok(resp.into_inner().tracks)
}
