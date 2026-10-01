//! Music metadata API boundary.
//!
//! v1 ships a single anonymous backend (rustypipe). The trait exists so an
//! authenticated backend (browser cookies / OAuth) can be added in v2
//! without touching the UI or app logic.

pub mod models;
pub mod rustypipe;

use anyhow::Result;
use async_trait::async_trait;

use models::{
    AlbumDetail, AlbumSummary, ArtistDetail, ArtistSummary, PlaylistDetail, PlaylistSummary,
    SearchKind, SearchResults, Track,
};

#[derive(Debug, Clone, Copy)]
pub enum StreamFormat {
    Best,
    Mp4Aac,
}

#[async_trait]
pub trait MusicApi: Send + Sync {
    async fn search(&self, query: &str, kind: SearchKind) -> Result<SearchResults>;
    async fn album(&self, id: &str) -> Result<AlbumDetail>;
    async fn artist(&self, id: &str) -> Result<ArtistDetail>;
    async fn playlist(&self, id: &str) -> Result<PlaylistDetail>;
    /// Radio ("up next" autoplay) continuation seeded by a track.
    async fn radio(&self, video_id: &str) -> Result<Vec<Track>>;
    /// Resolve a directly playable audio stream URL for `video_id`.
    ///
    /// Doing this in-process replaces mpv's yt-dlp hook, which in turn needs
    /// an external JS runtime (deno/node) for YouTube's challenges.
    /// The returned URL is short-lived - resolve it right before playback.
    async fn stream_url(&self, video_id: &str, format: StreamFormat) -> Result<String>;
    /// Plain-text lyrics from YT Music (fallback source; LRCLIB is primary).
    async fn plain_lyrics(&self, video_id: &str) -> Result<Option<String>>;

    /// Home page recommendations (charts / new releases) - works anonymously.
    async fn home(&self) -> Result<(Vec<Track>, Vec<AlbumSummary>)>;

    // ---- account (cookie login) ----

    fn is_logged_in(&self) -> bool;
    /// `input` may be a raw Cookie header, cookies.txt content, or a path to
    /// a cookies.txt file. Validates by fetching the user's library.
    async fn login_cookie(&self, input: &str) -> Result<()>;
    async fn logout(&self) -> Result<()>;
    async fn liked_tracks(&self) -> Result<Vec<Track>>;
    async fn history(&self) -> Result<Vec<Track>>;
    async fn saved_playlists(&self) -> Result<Vec<PlaylistSummary>>;
    async fn saved_albums(&self) -> Result<Vec<AlbumSummary>>;
    async fn saved_artists(&self) -> Result<Vec<ArtistSummary>>;
}
