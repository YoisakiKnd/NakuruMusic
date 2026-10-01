//! YouTube Music backend built on the `rustypipe` crate (InnerTube
//! DesktopMusic client). Anonymous for public data; cookie-authenticated
//! for the personal library. Verified against rustypipe 0.11.4.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use innertube_rs::{FormatFilter, FormatType, GetVideoInfoOptions, Innertube, QualityPreference};
use rustypipe::client::{RustyPipe, RustyPipeQuery};
use rustypipe::error::Error as RpError;
use rustypipe::model::{AlbumItem, ArtistId, ArtistItem, MusicPlaylistItem, TrackItem};
use tokio::sync::OnceCell;

use super::models::{
    AlbumDetail, AlbumSummary, ArtistDetail, ArtistSummary, PlaylistDetail, PlaylistSummary,
    SearchKind, SearchResults, Track,
};
use super::MusicApi;

pub struct RustyPipeApi {
    rp: RustyPipe,
    query: RustyPipeQuery,
    stream_client: OnceCell<Innertube>,
    /// rustypipe persists the auth cookie in its own cache; this marker file
    /// lets us know login state without poking its internals.
    login_marker: PathBuf,
    logged_in: AtomicBool,
}

impl RustyPipeApi {
    /// `storage_dir` holds rustypipe's client-version + auth cache; keep it
    /// inside our own data dir so nothing is written next to the executable.
    pub fn new(storage_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&storage_dir)?;
        let rp = RustyPipe::builder()
            .storage_dir(&storage_dir)
            .build()
            .context("初始化 rustypipe 客户端失败")?;
        let login_marker = storage_dir.join("logged_in");
        let logged_in = AtomicBool::new(login_marker.exists());
        Ok(Self {
            query: rp.query(),
            rp,
            stream_client: OnceCell::new(),
            login_marker,
            logged_in,
        })
    }
}

// YouTube returns these lists one page at a time (~100 items for playlists,
// ~20 for search). Without following continuations the UI silently shows
// only the first page, so every paginated list is extended up to a cap.
const PLAYLIST_LIMIT: usize = 1000;
const LIBRARY_LIMIT: usize = 500;
const SEARCH_LIMIT: usize = 60;

/// Follow continuations up to `limit`. A mid-way failure keeps whatever was
/// fetched - a partial list beats an error page.
macro_rules! fetch_all {
    ($self:expr, $paginator:expr, $limit:expr, $what:literal) => {
        if let Err(e) = $paginator.extend_limit(&$self.query, $limit).await {
            tracing::warn!("{} 分页加载中断: {e}", $what);
        }
    };
}

fn join_artists(artists: &[ArtistId]) -> String {
    if artists.is_empty() {
        "未知歌手".to_string()
    } else {
        artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(" / ")
    }
}

/// Cover art size we ask for - plenty for any terminal, still small enough
/// to fetch and decode quickly.
const COVER_PX: u32 = 544;

/// Pick a cover and ask the image host for a usable size.
///
/// Search results often only advertise 60–120px thumbnails, which look like
/// mush once scaled up. Google's image hosts accept size hints in the URL
/// (`=w120-h120-..`), so rewriting them yields a proper cover.
fn pick_cover(covers: &[rustypipe::model::Thumbnail]) -> Option<String> {
    let best = covers.iter().max_by_key(|c| c.width)?;
    Some(upscale_cover_url(&best.url, COVER_PX))
}

fn upscale_cover_url(url: &str, target: u32) -> String {
    let Some(eq) = url.rfind('=') else {
        return url.to_string(); // e.g. i.ytimg.com/... - fixed size
    };
    let (base, params) = url.split_at(eq + 1);
    // Only touch the trailing size-parameter group ("w120-h120-l90-rj").
    if !params.starts_with('w') && !params.starts_with('s') {
        return url.to_string();
    }
    let rewritten: Vec<String> = params
        .split('-')
        .map(|p| {
            for prefix in ['w', 'h', 's'] {
                if let Some(rest) = p.strip_prefix(prefix) {
                    if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                        return format!("{prefix}{target}");
                    }
                }
            }
            p.to_string()
        })
        .collect();
    format!("{base}{}", rewritten.join("-"))
}

fn conv_track(t: TrackItem) -> Track {
    Track {
        artists: join_artists(&t.artists),
        video_id: t.id,
        title: t.name,
        album: t.album.map(|a| a.name),
        duration_secs: t.duration,
        cover_url: pick_cover(&t.cover),
    }
}

fn conv_album(a: AlbumItem) -> AlbumSummary {
    AlbumSummary {
        artists: join_artists(&a.artists),
        id: a.id,
        title: a.name,
        year: a.year,
    }
}

fn conv_artist(a: ArtistItem) -> ArtistSummary {
    ArtistSummary {
        id: a.id,
        name: a.name,
    }
}

fn conv_playlist(p: MusicPlaylistItem) -> PlaylistSummary {
    PlaylistSummary {
        id: p.id,
        title: p.name,
        track_count: p.track_count.map(|n| n as u32),
    }
}

#[async_trait]
impl MusicApi for RustyPipeApi {
    async fn search(&self, query: &str, kind: SearchKind) -> Result<SearchResults> {
        let mut out = SearchResults::default();
        match kind {
            SearchKind::Songs => {
                let mut r = self.query.music_search_tracks(query).await?;
                fetch_all!(self, r.items, SEARCH_LIMIT, "搜索歌曲");
                out.tracks = r.items.items.into_iter().map(conv_track).collect();
            }
            SearchKind::Albums => {
                let mut r = self.query.music_search_albums(query).await?;
                fetch_all!(self, r.items, SEARCH_LIMIT, "搜索专辑");
                out.albums = r.items.items.into_iter().map(conv_album).collect();
            }
            SearchKind::Artists => {
                let mut r = self.query.music_search_artists(query).await?;
                fetch_all!(self, r.items, SEARCH_LIMIT, "搜索歌手");
                out.artists = r.items.items.into_iter().map(conv_artist).collect();
            }
            SearchKind::Playlists => {
                // community=true: user-created playlists, far more results
                // for arbitrary queries than YTM-curated ones.
                let mut r = self.query.music_search_playlists(query, true).await?;
                fetch_all!(self, r.items, SEARCH_LIMIT, "搜索歌单");
                out.playlists = r.items.items.into_iter().map(conv_playlist).collect();
            }
        }
        Ok(out)
    }

    async fn album(&self, id: &str) -> Result<AlbumDetail> {
        let a = self.query.music_album(id).await?;
        let album_name = a.name.clone();
        let album_artists = join_artists(&a.artists);
        let tracks = a
            .tracks
            .into_iter()
            .map(|t| {
                let mut track = conv_track(t);
                // Album pages omit per-track album / sometimes artists.
                if track.album.is_none() {
                    track.album = Some(album_name.clone());
                }
                if track.artists == "未知歌手" {
                    track.artists = album_artists.clone();
                }
                track
            })
            .collect();
        Ok(AlbumDetail {
            title: a.name,
            artists: album_artists,
            year: a.year,
            tracks,
        })
    }

    async fn artist(&self, id: &str) -> Result<ArtistDetail> {
        let a = self.query.music_artist(id, false).await?;
        Ok(ArtistDetail {
            name: a.name,
            top_tracks: a.tracks.into_iter().map(conv_track).collect(),
            albums: a.albums.into_iter().map(conv_album).collect(),
        })
    }

    async fn playlist(&self, id: &str) -> Result<PlaylistDetail> {
        let mut p = self.query.music_playlist(id).await?;
        fetch_all!(self, p.tracks, PLAYLIST_LIMIT, "歌单");
        Ok(PlaylistDetail {
            title: p.name,
            tracks: p.tracks.items.into_iter().map(conv_track).collect(),
        })
    }

    async fn radio(&self, video_id: &str) -> Result<Vec<Track>> {
        let r = self.query.music_radio_track(video_id).await?;
        // The first item is usually the seed track itself; the queue layer
        // dedupes by videoId, so it is safe to pass everything through.
        Ok(r.items.into_iter().map(conv_track).collect())
    }

    async fn stream_url(&self, video_id: &str, _format: super::StreamFormat) -> Result<String> {
        // RustyPipe's iOS URL currently stops after roughly 1 MiB with 403.
        // VisionOS supplies regular byte-range AAC URLs without an external
        // player, JS executable or PO-token helper. Its embedded QuickJS
        // decipher engine is initialized once on the first playback request.
        let client = self
            .stream_client
            .get_or_try_init(Innertube::new)
            .await
            .context("初始化内置音频地址解析器失败")?;
        let options = GetVideoInfoOptions {
            client: Some("VISIONOS".into()),
            ..Default::default()
        };
        let info = client
            .get_basic_info(video_id, Some(&options))
            .await
            .context("获取 VisionOS 音频地址失败")?;
        // Keep MP4/AAC for both engines until other codecs have end-to-end
        // device coverage. "Best" currently means the best AAC stream.
        let filter = FormatFilter {
            format_type: FormatType::AudioOnly,
            quality: QualityPreference::Highest,
            container: Some("mp4".into()),
        };
        let stream = innertube_rs::endpoints::player::select_format(&info.player_response, &filter)
            .context("该曲目没有可用的 AAC 音频流")?;
        let url = innertube_rs::endpoints::player::resolve_stream_url_full(
            stream,
            &client.player.decipherer,
            client.session.po_token.as_deref(),
            None,
        )
        .context("解码音频地址失败")?;
        let source_client = reqwest::Url::parse(&url)?
            .query_pairs()
            .find(|(key, _)| key == "c")
            .map(|(_, value)| value.into_owned());
        if source_client.as_deref() != Some("VISIONOS") {
            bail!("VisionOS 音频地址不可用，已拒绝可能截断的备用地址");
        }
        tracing::debug!(
            "resolved VisionOS stream: itag={} mime={} bitrate={}",
            stream.itag,
            stream.mime_type,
            stream.bitrate,
        );
        Ok(url)
    }

    async fn plain_lyrics(&self, video_id: &str) -> Result<Option<String>> {
        let details = self.query.music_details(video_id).await?;
        match details.lyrics_id {
            Some(lyrics_id) => {
                let lyrics = self.query.music_lyrics(lyrics_id).await?;
                Ok(Some(lyrics.body))
            }
            None => Ok(None),
        }
    }

    async fn home(&self) -> Result<(Vec<Track>, Vec<AlbumSummary>)> {
        // Fire both requests concurrently; either failing alone is tolerated.
        let (charts, new_albums) =
            tokio::join!(self.query.music_charts(None), self.query.music_new_albums(),);
        let tracks = charts
            .map(|c| {
                let list = if c.top_tracks.is_empty() {
                    c.trending_tracks
                } else {
                    c.top_tracks
                };
                list.into_iter().map(conv_track).collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let albums = new_albums
            .map(|a| a.into_iter().map(conv_album).collect::<Vec<_>>())
            .unwrap_or_default();
        if tracks.is_empty() && albums.is_empty() {
            bail!("获取推荐内容失败（网络不可达？）");
        }
        Ok((tracks, albums))
    }

    // ---- account ----

    fn is_logged_in(&self) -> bool {
        self.logged_in.load(Ordering::SeqCst)
    }

    async fn login_cookie(&self, input: &str) -> Result<()> {
        let text = input.trim();
        if text.is_empty() {
            bail!("输入为空");
        }
        // A short single-line input pointing at an existing file → read it.
        let content =
            if text.len() < 500 && !text.contains('\n') && std::path::Path::new(text).is_file() {
                std::fs::read_to_string(text).with_context(|| format!("读取 {text} 失败"))?
            } else {
                text.to_string()
            };

        // Netscape cookies.txt (browser import / extension export) is parsed
        // by us into a plain Cookie header; anything else is taken as one.
        let is_netscape = content.lines().any(|l| l.split('\t').count() >= 7)
            || content.starts_with("# Netscape")
            || content.starts_with("# HTTP Cookie");
        let header = if is_netscape {
            crate::browser_login::cookie_header_from_netscape(&content)?
        } else {
            content
        };

        if !crate::browser_login::has_auth_credential(&header) {
            bail!(
                "这份 Cookie 里没有 YouTube 的登录凭据。\n请确认浏览器已登录 music.youtube.com 后再导入。"
            );
        }

        // rustypipe verifies the session against youtube.com here.
        if let Err(e) = self.rp.user_auth_set_cookie(header).await {
            if matches!(e, RpError::Auth(_)) {
                bail!(
                    "YouTube 不认可这份登录信息（会话已过期或该浏览器未登录）。\n请在浏览器中重新登录 music.youtube.com，然后再次导入。"
                );
            }
            return Err(anyhow::anyhow!(e).context("导入登录信息失败"));
        }

        // Validate by hitting an authenticated endpoint.
        if let Err(e) = self.query.music_saved_playlists().await {
            let _ = self.rp.user_auth_remove_cookie().await;
            return Err(anyhow::anyhow!(e).context("登录验证失败，音乐库接口不可访问"));
        }

        let _ = std::fs::write(&self.login_marker, b"1");
        self.logged_in.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn logout(&self) -> Result<()> {
        self.rp
            .user_auth_remove_cookie()
            .await
            .context("清除登录信息失败")?;
        let _ = std::fs::remove_file(&self.login_marker);
        self.logged_in.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn liked_tracks(&self) -> Result<Vec<Track>> {
        let mut playlist = self.query.music_liked_tracks().await?;
        fetch_all!(self, playlist.tracks, PLAYLIST_LIMIT, "喜欢的音乐");
        Ok(playlist.tracks.items.into_iter().map(conv_track).collect())
    }

    async fn history(&self) -> Result<Vec<Track>> {
        // HistoryItem does not implement FromYtItem, so Paginator::extend is
        // unavailable here - follow the continuation tokens by hand.
        let mut page = self.query.music_history().await?;
        let mut tracks: Vec<Track> = page.items.drain(..).map(|h| conv_track(h.item)).collect();
        let mut ctoken = page.ctoken.clone();
        let visitor = page.visitor_data.clone();
        while tracks.len() < LIBRARY_LIMIT {
            let Some(token) = ctoken.take() else { break };
            match self
                .query
                .music_history_continuation(&token, visitor.as_deref())
                .await
            {
                Ok(mut next) => {
                    if next.items.is_empty() {
                        break;
                    }
                    tracks.extend(next.items.drain(..).map(|h| conv_track(h.item)));
                    ctoken = next.ctoken;
                }
                Err(e) => {
                    tracing::warn!("播放历史分页加载中断: {e}");
                    break;
                }
            }
        }
        Ok(tracks)
    }

    async fn saved_playlists(&self) -> Result<Vec<PlaylistSummary>> {
        let mut items = self.query.music_saved_playlists().await?;
        fetch_all!(self, items, LIBRARY_LIMIT, "我的歌单");
        Ok(items.items.into_iter().map(conv_playlist).collect())
    }

    async fn saved_albums(&self) -> Result<Vec<AlbumSummary>> {
        let mut items = self.query.music_saved_albums().await?;
        fetch_all!(self, items, LIBRARY_LIMIT, "收藏的专辑");
        Ok(items.items.into_iter().map(conv_album).collect())
    }

    async fn saved_artists(&self) -> Result<Vec<ArtistSummary>> {
        let mut items = self.query.music_saved_artists().await?;
        fetch_all!(self, items, LIBRARY_LIMIT, "关注的歌手");
        Ok(items.items.into_iter().map(conv_artist).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upscales_google_image_size_hints() {
        assert_eq!(
            upscale_cover_url(
                "https://lh3.googleusercontent.com/abc=w120-h120-l90-rj",
                544
            ),
            "https://lh3.googleusercontent.com/abc=w544-h544-l90-rj"
        );
        assert_eq!(
            upscale_cover_url(
                "https://yt3.googleusercontent.com/x=s88-c-k-c0x00ffffff",
                544
            ),
            "https://yt3.googleusercontent.com/x=s544-c-k-c0x00ffffff"
        );
    }

    #[test]
    fn leaves_urls_without_size_hints_alone() {
        let fixed = "https://i.ytimg.com/vi/abc/maxresdefault.jpg";
        assert_eq!(upscale_cover_url(fixed, 544), fixed);
        // A query string must not be mistaken for a size group.
        let query = "https://example.com/img.jpg?v=2";
        assert_eq!(upscale_cover_url(query, 544), query);
    }
}
