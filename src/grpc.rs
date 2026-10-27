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
    AlbumRequest, AlbumResponse, ArtistRequest, ArtistResponse, Empty, HealthResponse,
    HomeRequest, LikedSongsRequest, LyricsRequest, LyricsResponse, ParseAuthHeadersRequest,
    AddPlaylistItemsRequest, CreatePlaylistRequest, DeletePlaylistRequest, PlaylistEditResponse,
    PlaylistItemRef as PbPlaylistItemRef,
    RemovePlaylistItemsRequest, SetProxyRequest, SetProxyResponse,
    Shelf,
    PlaylistTracksRequest, PlaylistTracksResponse, PrefetchResolveRequest, RecommendRequest,
    ResolveRequest, ResolveResponse, SearchRequest, SearchSection, SessionState, SessionStatus,
    SetSessionRequest, SuggestRequest, Track, UserPlaylistsResponse,
    player_api_client::PlayerApiClient,
};

/// Deadline for one RPC. Resolving a stream URL runs yt-dlp extraction and is
/// allowed to take longer (see [`RESOLVE_TIMEOUT`]).
const CALL_TIMEOUT: Duration = Duration::from_secs(15);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(45);
/// A long playlist is fetched page by page in the sidecar; mixes are cut
/// off, real playlists are not, so this gets more room than a plain call.
const PLAYLIST_TIMEOUT: Duration = Duration::from_secs(45);
/// Music search goes through ytmusicapi and may page through results.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);
/// Autocomplete must feel instant; a slow answer is worse than none.
const SUGGEST_TIMEOUT: Duration = Duration::from_secs(5);

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

/// The sidecar has not announced its port yet: the app started a moment ago or
/// the process is being restarted. Callers that can wait retry this one.
#[derive(Debug)]
pub struct NotReady;

impl std::fmt::Display for NotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the sidecar endpoint is not configured yet")
    }
}

impl std::error::Error for NotReady {}

async fn connect() -> Result<Client> {
    let channel = CHANNEL
        .get()
        .cloned()
        .ok_or_else(|| anyhow::Error::new(NotReady))?;
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

pub async fn resolve(video_id: &str, quality: &str, use_account: bool) -> Result<ResolveResponse> {
    resolve_impl(video_id, quality, use_account, false).await
}

pub async fn resolve_forced(
    video_id: &str,
    quality: &str,
    use_account: bool,
) -> Result<ResolveResponse> {
    resolve_impl(video_id, quality, use_account, true).await
}

async fn resolve_impl(
    video_id: &str,
    quality: &str,
    use_account: bool,
    force: bool,
) -> Result<ResolveResponse> {
    let mut client = connect().await?;
    let resp = client
        .resolve_url(with_timeout(
            ResolveRequest {
                video_id: video_id.to_string(),
                audio_quality: quality.to_string(),
                force,
                use_account,
            },
            RESOLVE_TIMEOUT,
        ))
        .await?;
    Ok(resp.into_inner())
}

pub async fn prefetch_resolve(
    ids: &[String],
    quality: &str,
    use_account: bool,
) -> Result<i32> {
    let mut client = connect().await?;
    let resp = client
        .prefetch_resolve(request(PrefetchResolveRequest {
            ids: ids.to_vec(),
            audio_quality: quality.to_string(),
            use_account,
        }))
        .await?;
    Ok(resp.into_inner().resolved)
}

/// YouTube Music search. `filter` is "", "songs", "videos", "albums",
/// "artists" or "playlists"; the sidecar answers with titled sections.
pub async fn search(query: &str, filter: &str, limit: i32) -> Result<Vec<SearchSection>> {
    let mut client = connect().await?;
    let resp = client
        .search(with_timeout(
            SearchRequest {
                query: query.to_string(),
                limit,
                filter: filter.to_string(),
            },
            SEARCH_TIMEOUT,
        ))
        .await?
        .into_inner();
    if !resp.error.is_empty() {
        return Err(anyhow::anyhow!(resp.error));
    }
    Ok(resp.sections)
}

/// Home shelves (personal when signed in, public explore otherwise).
pub async fn get_home(limit: i32) -> Result<Vec<Shelf>> {
    let mut client = connect().await?;
    let resp = client
        .get_home(with_timeout(HomeRequest { limit }, SEARCH_TIMEOUT))
        .await?
        .into_inner();
    if !resp.error.is_empty() {
        return Err(anyhow::anyhow!(resp.error));
    }
    Ok(resp.shelves)
}

/// True for "the sidecar is not there yet" failures, which are worth retrying:
/// no port announced yet, a refused connection, a restarting process. An error
/// the sidecar itself reported is not retryable.
pub fn is_retryable_failure(err: &anyhow::Error) -> bool {
    if err.downcast_ref::<NotReady>().is_some() {
        return true;
    }
    if err.downcast_ref::<tonic::transport::Error>().is_some() {
        return true;
    }
    matches!(
        err.downcast_ref::<tonic::Status>().map(|s| s.code()),
        Some(tonic::Code::Unavailable | tonic::Code::Unknown)
    )
}

/// Album page (title, header line, tracks).
pub async fn get_album(browse_id: &str) -> Result<AlbumResponse> {
    let mut client = connect().await?;
    let resp = client
        .get_album(with_timeout(
            AlbumRequest {
                browse_id: browse_id.to_string(),
            },
            SEARCH_TIMEOUT,
        ))
        .await?
        .into_inner();
    if !resp.error.is_empty() {
        return Err(anyhow::anyhow!(resp.error));
    }
    Ok(resp)
}

/// Artist page (popular songs + albums).
pub async fn get_artist(browse_id: &str) -> Result<ArtistResponse> {
    let mut client = connect().await?;
    let resp = client
        .get_artist(with_timeout(
            ArtistRequest {
                browse_id: browse_id.to_string(),
            },
            SEARCH_TIMEOUT,
        ))
        .await?
        .into_inner();
    if !resp.error.is_empty() {
        return Err(anyhow::anyhow!(resp.error));
    }
    Ok(resp)
}

/// Query suggestions for the search box.
pub async fn suggest(query: &str) -> Result<Vec<String>> {
    let mut client = connect().await?;
    let resp = client
        .suggest(with_timeout(
            SuggestRequest {
                query: query.to_string(),
            },
            SUGGEST_TIMEOUT,
        ))
        .await?
        .into_inner();
    Ok(resp.suggestions)
}

pub async fn get_user_playlists() -> Result<UserPlaylistsResponse> {
    let mut client = connect().await?;
    let resp = client
        .get_user_playlists(request(Empty {}))
        .await?;
    Ok(resp.into_inner())
}

/// One page of a playlist. `limit == 0` means "the default page".
pub async fn get_playlist_tracks(
    playlist_id: &str,
    offset: i32,
    limit: i32,
) -> Result<PlaylistTracksResponse> {
    let mut client = connect().await?;
    let resp = client
        .get_playlist_tracks(with_timeout(
            PlaylistTracksRequest {
                id: playlist_id.to_string(),
                offset,
                limit,
            },
            PLAYLIST_TIMEOUT,
        ))
        .await?;
    Ok(resp.into_inner())
}

/// One row to remove from an account playlist: YouTube needs its own per-item
/// id, because the same video may appear twice.
pub struct PlaylistItemRef {
    pub video_id: String,
    pub set_video_id: String,
}

/// Creates a playlist on the account (private) and returns its id.
pub async fn create_account_playlist(title: &str) -> Result<PlaylistEditResponse> {
    let mut client = connect().await?;
    let resp = client
        .create_account_playlist(with_timeout(
            CreatePlaylistRequest {
                title: title.to_string(),
            },
            CALL_TIMEOUT,
        ))
        .await?;
    Ok(resp.into_inner())
}

/// Drops a saved playlist out of the account's library (it stays on YouTube).
pub async fn remove_from_library(playlist_id: &str) -> Result<PlaylistEditResponse> {
    let mut client = connect().await?;
    let resp = client
        .remove_from_library(with_timeout(
            DeletePlaylistRequest {
                playlist_id: playlist_id.to_string(),
            },
            CALL_TIMEOUT,
        ))
        .await?;
    Ok(resp.into_inner())
}

/// Deletes a playlist on the account. There is no undo.
pub async fn delete_account_playlist(playlist_id: &str) -> Result<PlaylistEditResponse> {
    let mut client = connect().await?;
    let resp = client
        .delete_account_playlist(with_timeout(
            DeletePlaylistRequest {
                playlist_id: playlist_id.to_string(),
            },
            CALL_TIMEOUT,
        ))
        .await?;
    Ok(resp.into_inner())
}

/// Appends tracks to an account playlist (this writes to YouTube).
pub async fn add_playlist_items(
    playlist_id: &str,
    video_ids: &[String],
) -> Result<PlaylistEditResponse> {
    let mut client = connect().await?;
    let resp = client
        .add_playlist_items(with_timeout(
            AddPlaylistItemsRequest {
                playlist_id: playlist_id.to_string(),
                video_ids: video_ids.to_vec(),
            },
            CALL_TIMEOUT,
        ))
        .await?;
    Ok(resp.into_inner())
}

/// Removes tracks from an account playlist (this writes to YouTube).
pub async fn remove_playlist_items(
    playlist_id: &str,
    items: &[PlaylistItemRef],
) -> Result<PlaylistEditResponse> {
    let mut client = connect().await?;
    let resp = client
        .remove_playlist_items(with_timeout(
            RemovePlaylistItemsRequest {
                playlist_id: playlist_id.to_string(),
                items: items
                    .iter()
                    .map(|i| PbPlaylistItemRef {
                        video_id: i.video_id.clone(),
                        set_video_id: i.set_video_id.clone(),
                    })
                    .collect(),
            },
            CALL_TIMEOUT,
        ))
        .await?;
    Ok(resp.into_inner())
}

/// Tells the sidecar which proxy to use ("" or "none" = no proxy).
pub async fn set_proxy(url: &str) -> Result<SetProxyResponse> {
    let mut client = connect().await?;
    let resp = client
        .set_proxy(with_timeout(
            SetProxyRequest {
                url: url.to_string(),
            },
            CALL_TIMEOUT,
        ))
        .await?;
    Ok(resp.into_inner())
}

pub async fn get_liked_songs(offset: i32, limit: i32) -> Result<PlaylistTracksResponse> {
    let mut client = connect().await?;
    let resp = client
        .get_liked_songs(with_timeout(LikedSongsRequest { offset, limit }, PLAYLIST_TIMEOUT))
        .await?;
    Ok(resp.into_inner())
}

pub async fn set_session(cookie: &str, user_agent: &str, authuser: &str) -> Result<SessionStatus> {
    let mut client = connect().await?;
    let resp = client
        .set_session(request(SetSessionRequest {
            cookie: cookie.to_string(),
            user_agent: user_agent.to_string(),
            x_goog_authuser: authuser.to_string(),
        }))
        .await?;
    Ok(resp.into_inner())
}

/// Normalises pasted browser headers into session fields (sidecar-side parser).
pub async fn parse_auth_headers(text: &str) -> Result<SetSessionRequest> {
    let mut client = connect().await?;
    let resp = client
        .parse_auth_headers(request(ParseAuthHeadersRequest {
            text: text.to_string(),
        }))
        .await?;
    Ok(resp.into_inner())
}

pub async fn validate_session() -> Result<SessionStatus> {
    let mut client = connect().await?;
    let resp = client.validate_session(request(Empty {})).await?;
    Ok(resp.into_inner())
}

pub async fn clear_session() -> Result<SessionStatus> {
    let mut client = connect().await?;
    let resp = client.clear_session(request(Empty {})).await?;
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
