use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Position;
use tokio::sync::{mpsc, Semaphore};

use crate::api::models::{
    AlbumDetail, AlbumSummary, ArtistDetail, ArtistSummary, PlaylistDetail, PlaylistSummary,
    SearchKind, SearchResults, Track,
};
use crate::api::{MusicApi, StreamFormat};
use crate::config::{Config, PlaybackEngine};
use crate::lyrics::{self, LyricsData};
use crate::player::queue::{Advance, Queue};
use crate::player::{PlayerCmd, PlayerEvent, PlayerHandle, TrackEvent};
use crate::sponsorblock::{self, Segment};

/// All inputs to the app converge into this enum; the main loop owns the
/// only mutable reference to `App` and applies events sequentially.
#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    /// Bracketed paste - needed for multi-line cookie input.
    Paste(String),
    Resize,
    Tick,
    Player {
        epoch: u64,
        event: PlayerEvent,
    },
    EngineChecked {
        engine: PlaybackEngine,
        mpv_path: Option<String>,
        ytdlp_path: Option<String>,
    },
    Api(ApiMsg),
}

/// Results of spawned async work. `seq` guards against stale responses
/// overwriting newer state.
#[derive(Debug)]
pub enum ApiMsg {
    SearchDone {
        seq: u64,
        kind: SearchKind,
        result: Result<SearchResults, String>,
    },
    AlbumDone {
        seq: u64,
        result: Result<AlbumDetail, String>,
    },
    ArtistDone {
        seq: u64,
        result: Result<ArtistDetail, String>,
    },
    PlaylistDone {
        seq: u64,
        result: Result<PlaylistDetail, String>,
    },
    RadioDone {
        queue_seq: u64,
        seed: String,
        result: Result<Vec<Track>, String>,
    },
    /// Audio stream URL resolved for a track (see [`MusicApi::stream_url`]).
    StreamResolved {
        play_seq: u64,
        video_id: String,
        attempt: u8,
        result: Result<String, String>,
    },
    LyricsDone {
        play_seq: u64,
        data: LyricsData,
    },
    SponsorDone {
        play_seq: u64,
        segments: Vec<Segment>,
    },
    CoverLoaded {
        play_seq: u64,
        image: Option<Box<image::DynamicImage>>,
    },
    HomeDone {
        seq: u64,
        result: Result<(Vec<Track>, Vec<AlbumSummary>), String>,
    },
    LibraryTracks {
        seq: u64,
        title: String,
        result: Result<Vec<Track>, String>,
    },
    LibraryPlaylists {
        seq: u64,
        result: Result<Vec<PlaylistSummary>, String>,
    },
    LibraryAlbums {
        seq: u64,
        result: Result<Vec<AlbumSummary>, String>,
    },
    LibraryArtists {
        seq: u64,
        result: Result<Vec<ArtistSummary>, String>,
    },
    LoginDone {
        seq: u64,
        result: Result<(), String>,
    },
    LogoutDone {
        seq: u64,
        result: Result<(), String>,
    },
}

/// Remappable global actions. List-navigation keys (j/k/Enter/a/x/..) are
/// intentionally fixed - only playback/mode keys can be overridden in
/// `[keys]` of config.toml, keyed by the name in [`DEFAULT_KEYS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Quit,
    Search,
    Library,
    Help,
    FocusToggle,
    PlayPause,
    Mute,
    NextTrack,
    PrevTrack,
    SeekBack,
    SeekFwd,
    VolDown,
    VolUp,
    RepeatCycle,
    RadioToggle,
    Shuffle,
    LyricsToggle,
    RestartPlayer,
    History,
    Settings,
}

/// (action, config name, default key)
const DEFAULT_KEYS: &[(Action, &str, KeyCode)] = &[
    (Action::Quit, "quit", KeyCode::Char('q')),
    (Action::Search, "search", KeyCode::Char('/')),
    (Action::Library, "library", KeyCode::Char('L')),
    (Action::Help, "help", KeyCode::Char('?')),
    (Action::FocusToggle, "focus", KeyCode::Tab),
    (Action::PlayPause, "play_pause", KeyCode::Char(' ')),
    (Action::Mute, "mute", KeyCode::Char('m')),
    (Action::NextTrack, "next", KeyCode::Char('n')),
    (Action::PrevTrack, "prev", KeyCode::Char('p')),
    (Action::SeekBack, "seek_back", KeyCode::Left),
    (Action::SeekFwd, "seek_fwd", KeyCode::Right),
    (Action::VolDown, "vol_down", KeyCode::Char('-')),
    (Action::VolUp, "vol_up", KeyCode::Char('=')),
    (Action::RepeatCycle, "repeat", KeyCode::Char('r')),
    (Action::RadioToggle, "radio", KeyCode::Char('t')),
    (Action::Shuffle, "shuffle", KeyCode::Char('s')),
    (Action::LyricsToggle, "lyrics", KeyCode::Char('l')),
    (Action::RestartPlayer, "restart_player", KeyCode::Char('R')),
    (Action::History, "history", KeyCode::Char('H')),
    (Action::Settings, "settings", KeyCode::Char(',')),
];

/// Download and decode album art. Failures are silent - a missing cover
/// must never interrupt playback.
static COVER_DECODE_LIMIT: Semaphore = Semaphore::const_new(2);

async fn fetch_cover(http: reqwest::Client, url: String) -> Option<image::DynamicImage> {
    const MAX_BYTES: usize = 4 * 1024 * 1024;
    let mut response = http.get(&url).send().await.ok()?.error_for_status().ok()?;
    if response
        .content_length()
        .is_some_and(|n| n > MAX_BYTES as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len() + chunk.len() > MAX_BYTES {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    // Aborting an async task cannot stop a decoder already running in
    // spawn_blocking. Keep those orphaned decodes bounded during fast skips.
    let permit = COVER_DECODE_LIMIT.acquire().await.ok()?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .ok()?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(4096);
        limits.max_image_height = Some(4096);
        limits.max_alloc = Some(64 * 1024 * 1024);
        reader.limits(limits);
        reader.decode().ok().map(|img| img.thumbnail(544, 544))
    })
    .await
    .ok()?
}

/// Detected browsers first (the one-keystroke path), then the fallbacks.
fn login_methods() -> Vec<LoginMethod> {
    let mut methods: Vec<LoginMethod> = crate::browser_cookies::detect()
        .into_iter()
        .map(|profile| LoginMethod::Browser {
            display: profile.display.clone(),
            profile: Box::new(profile),
        })
        .collect();
    methods.push(LoginMethod::OpenBrowser);
    methods.push(LoginMethod::Manual);
    methods
}

/// Shared list navigation: j/k, arrows, PageUp/Down, g/G.
/// Returns true if the key was a navigation key (even on an empty list).
fn nav_list(selected: &mut usize, len: usize, key: KeyCode) -> bool {
    let last = len.saturating_sub(1);
    match key {
        KeyCode::Up | KeyCode::Char('k') => {
            *selected = selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            *selected = (*selected + 1).min(last);
        }
        KeyCode::PageUp => *selected = selected.saturating_sub(10),
        KeyCode::PageDown => *selected = (*selected + 10).min(last),
        KeyCode::Char('g') => *selected = 0,
        KeyCode::Char('G') => *selected = last,
        _ => return false,
    }
    true
}

fn build_keymap(overrides: &HashMap<String, String>) -> HashMap<KeyCode, Action> {
    let mut map: HashMap<KeyCode, Action> = DEFAULT_KEYS
        .iter()
        .map(|(action, _, key)| (*key, *action))
        .collect();
    for (action, name, default) in DEFAULT_KEYS {
        let Some(raw) = overrides.get(*name) else {
            continue;
        };
        let Some(key) = crate::config::parse_key(raw) else {
            tracing::warn!("忽略无效键位配置 {name}={raw:?}");
            continue;
        };
        if key == *default {
            continue;
        }
        if matches!(
            key,
            KeyCode::Enter
                | KeyCode::Esc
                | KeyCode::Backspace
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Char(
                    'j' | 'k'
                        | 'g'
                        | 'G'
                        | 'a'
                        | 'A'
                        | 'x'
                        | 'J'
                        | 'K'
                        | 'P'
                        | '1'
                        | '2'
                        | '3'
                        | '4'
                        | '['
                        | ']'
                )
        ) || map.contains_key(&key)
        {
            tracing::warn!("忽略冲突键位配置 {name}={raw:?}");
            continue;
        }
        map.remove(default);
        map.insert(key, *action);
    }
    map
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Main,
    Queue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainView {
    Home,
    Search,
    Browse,
    /// Full-page player: cover art, metadata and synced lyrics.
    NowPlaying,
    Library,
    History,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Search,
    Login,
}

/// Charts + new releases shown on the home screen.
pub struct HomeState {
    pub tracks: Vec<Track>,
    pub albums: Vec<AlbumSummary>,
    pub selected: usize,
    pub loading: bool,
}

impl HomeState {
    pub fn row_count(&self) -> usize {
        self.tracks.len() + self.albums.len()
    }
}

pub const LIBRARY_MENU: [&str; 5] = [
    "喜欢的音乐",
    "我的歌单",
    "收藏的专辑",
    "关注的歌手",
    "播放历史",
];

pub struct LibraryState {
    pub selected: usize,
    pub loading: bool,
    pub failed: bool,
}

/// One row on the login screen.
#[derive(Debug, Clone)]
pub enum LoginMethod {
    /// Import cookies by reading an installed browser's own cookie database.
    /// Boxed to keep the enum small - the other variants carry nothing.
    Browser {
        display: String,
        profile: Box<crate::browser_cookies::BrowserProfile>,
    },
    /// Open music.youtube.com so the user can sign in first.
    OpenBrowser,
    /// Fall back to pasting a cookie / cookies.txt path.
    Manual,
}

impl LoginMethod {
    pub fn label(&self) -> String {
        match self {
            LoginMethod::Browser { display, .. } => format!("从 {display} 导入登录"),
            LoginMethod::OpenBrowser => "先在浏览器登录 music.youtube.com（打开网页）".into(),
            LoginMethod::Manual => "手动粘贴 Cookie 或 cookies.txt 路径".into(),
        }
    }
}

pub struct LoginState {
    pub methods: Vec<LoginMethod>,
    pub selected: usize,
    pub busy: bool,
}

pub struct SearchState {
    pub query: String,
    pub kind_idx: usize,
    pub results: SearchResults,
    pub selected: usize,
    pub loading: [bool; 4],
    pub errors: [Option<String>; 4],
}

impl SearchState {
    pub fn is_loading(&self) -> bool {
        self.loading[self.kind_idx]
    }

    pub fn kind(&self) -> SearchKind {
        SearchKind::ALL[self.kind_idx]
    }

    pub fn list_len(&self) -> usize {
        match self.kind() {
            SearchKind::Songs => self.results.tracks.len(),
            SearchKind::Albums => self.results.albums.len(),
            SearchKind::Artists => self.results.artists.len(),
            SearchKind::Playlists => self.results.playlists.len(),
        }
    }
}

pub enum BrowsePage {
    Album {
        title: String,
        data: Option<AlbumDetail>,
        selected: usize,
    },
    Artist {
        name: String,
        data: Option<ArtistDetail>,
        selected: usize,
    },
    Playlist {
        title: String,
        data: Option<PlaylistDetail>,
        selected: usize,
    },
    /// Pre-fetched flat pages (library lists, history, liked music).
    Tracks {
        title: String,
        tracks: Vec<Track>,
        selected: usize,
    },
    Albums {
        title: String,
        items: Vec<AlbumSummary>,
        selected: usize,
    },
    Artists {
        title: String,
        items: Vec<ArtistSummary>,
        selected: usize,
    },
    Playlists {
        title: String,
        items: Vec<PlaylistSummary>,
        selected: usize,
    },
}

impl BrowsePage {
    /// Number of selectable rows (artist pages: top tracks then albums).
    pub fn row_count(&self) -> usize {
        match self {
            BrowsePage::Album { data, .. } => data.as_ref().map_or(0, |d| d.tracks.len()),
            BrowsePage::Playlist { data, .. } => data.as_ref().map_or(0, |d| d.tracks.len()),
            BrowsePage::Artist { data, .. } => data
                .as_ref()
                .map_or(0, |d| d.top_tracks.len() + d.albums.len()),
            BrowsePage::Tracks { tracks, .. } => tracks.len(),
            BrowsePage::Albums { items, .. } => items.len(),
            BrowsePage::Artists { items, .. } => items.len(),
            BrowsePage::Playlists { items, .. } => items.len(),
        }
    }

    fn selected_mut(&mut self) -> &mut usize {
        match self {
            BrowsePage::Album { selected, .. }
            | BrowsePage::Artist { selected, .. }
            | BrowsePage::Playlist { selected, .. }
            | BrowsePage::Tracks { selected, .. }
            | BrowsePage::Albums { selected, .. }
            | BrowsePage::Artists { selected, .. }
            | BrowsePage::Playlists { selected, .. } => selected,
        }
    }
}

pub struct LyricsState {
    pub data: LyricsData,
    pub loading: bool,
    /// Manual scroll offset for plain lyrics.
    pub scroll: u16,
}

/// Album art for the now-playing page. The protocol object is built once
/// per track and re-encoded by the widget whenever the area changes.
pub struct CoverState {
    pub protocol: Option<ratatui_image::protocol::StatefulProtocol>,
    pub loading: bool,
    attempted: bool,
}

/// Mirror of the mpv-side playback state for rendering.
pub struct PlaybackState {
    pub current_title: Option<String>,
    pub loading: bool,
    /// Seconds spent in the loading state (stream-resolution feedback).
    pub loading_secs: f32,
    pub time_pos: f64,
    pub duration: f64,
    pub paused: bool,
    pub volume: i64,
    pub muted: bool,
    pub alive: bool,
}

pub struct App {
    pub config: Config,
    pub should_quit: bool,
    pub player: PlayerHandle,
    pub pb: PlaybackState,
    /// Set when the user asks to restart a dead player; main re-wires it.
    pub player_restart_requested: bool,
    pub player_epoch: u64,
    restart_pending_track: bool,
    restart_resume_paused: bool,
    pub settings_selected: usize,
    pub engine_switch_pending: bool,
    pub status: Option<String>,
    pub playback_error: Option<String>,
    status_ttl: u8,
    pub detail_retry_hint: Option<String>,

    pub api: Arc<dyn MusicApi>,
    pub http: reqwest::Client,
    /// Scratch space for the browser cookie export.
    data_dir: std::path::PathBuf,
    config_dir: std::path::PathBuf,
    tx: mpsc::UnboundedSender<AppEvent>,

    pub focus: Focus,
    pub main_view: MainView,
    prev_main_view: MainView,
    /// Some((mode, buffer)) while the user is typing (search query / cookie).
    pub input: Option<(InputMode, String)>,
    pub search: SearchState,
    pub home: HomeState,
    pub library: LibraryState,
    pub login: LoginState,
    pub browse_stack: Vec<BrowsePage>,
    browse_root_view: MainView,
    pub queue: Queue,
    pub queue_selected: usize,
    pub radio_on: bool,
    radio_inflight: bool,
    queue_seq: u64,
    play_seq: u64,
    pub lyrics: LyricsState,
    pub cover: CoverState,
    /// None when the terminal cannot show images at all.
    picker: Option<ratatui_image::picker::Picker>,
    /// The track being shown on the now-playing page.
    pub now_playing: Option<Track>,
    pub history: crate::history::PlaybackHistory,
    pub history_selected: usize,
    sponsor_video_id: String,
    sponsor_segments: Vec<Segment>,
    current_video_id: Option<String>,
    stream_attempt: u8,
    load_requested: bool,
    resume_position: Option<f64>,
    resume_target_id: Option<String>,
    pub help_visible: bool,
    pub help_scroll: u16,
    search_seq: u64,
    home_seq: u64,
    auth_seq: u64,
    browse_seq: u64,
    library_seq: u64,
    keymap: HashMap<KeyCode, Action>,
    /// Filled by main after each draw; consumed by the mouse handler.
    pub ui_layout: crate::ui::UiLayout,
    last_click: Option<(Instant, u16, u16)>,
    last_persist: Instant,
    persist_task: Option<tokio::task::JoinHandle<()>>,
    track_tasks: TrackTasks,
    search_tasks: [Option<tokio::task::JoinHandle<()>>; 4],
    browse_task: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Default)]
struct TrackTasks {
    stream: Option<tokio::task::JoinHandle<()>>,
    lyrics: Option<tokio::task::JoinHandle<()>>,
    sponsor: Option<tokio::task::JoinHandle<()>>,
    cover: Option<tokio::task::JoinHandle<()>>,
}

impl TrackTasks {
    fn abort(&mut self) {
        for task in [
            &mut self.stream,
            &mut self.lyrics,
            &mut self.sponsor,
            &mut self.cover,
        ] {
            if let Some(task) = task.take() {
                task.abort();
            }
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.track_tasks.abort();
        for task in &mut self.search_tasks {
            if let Some(task) = task.take() {
                task.abort();
            }
        }
        if let Some(task) = self.browse_task.take() {
            task.abort();
        }
    }
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Config,
        player: PlayerHandle,
        api: Arc<dyn MusicApi>,
        http: reqwest::Client,
        data_dir: std::path::PathBuf,
        config_dir: std::path::PathBuf,
        picker: Option<ratatui_image::picker::Picker>,
        tx: mpsc::UnboundedSender<AppEvent>,
    ) -> Self {
        let volume = config.playback.volume;
        let radio_on = config.playback.radio_auto;
        let keymap = build_keymap(&config.keys);
        Self {
            config,
            should_quit: false,
            player,
            pb: PlaybackState {
                current_title: None,
                loading: false,
                loading_secs: 0.0,
                time_pos: 0.0,
                duration: 0.0,
                paused: false,
                volume,
                muted: false,
                alive: false,
            },
            player_restart_requested: false,
            player_epoch: 0,
            restart_pending_track: false,
            restart_resume_paused: false,
            settings_selected: 0,
            engine_switch_pending: false,
            status: None,
            playback_error: None,
            status_ttl: 0,
            detail_retry_hint: None,
            api,
            http,
            data_dir,
            config_dir,
            tx,
            focus: Focus::Main,
            main_view: MainView::Home,
            prev_main_view: MainView::Home,
            input: None,
            search: SearchState {
                query: String::new(),
                kind_idx: 0,
                results: SearchResults::default(),
                selected: 0,
                loading: [false; 4],
                errors: std::array::from_fn(|_| None),
            },
            home: HomeState {
                tracks: Vec::new(),
                albums: Vec::new(),
                selected: 0,
                loading: false,
            },
            library: LibraryState {
                selected: 0,
                loading: false,
                failed: false,
            },
            login: LoginState {
                methods: login_methods(),
                selected: 0,
                busy: false,
            },
            browse_stack: Vec::new(),
            browse_root_view: MainView::Home,
            queue: Queue::default(),
            queue_selected: 0,
            radio_on,
            radio_inflight: false,
            queue_seq: 0,
            play_seq: 0,
            lyrics: LyricsState {
                data: LyricsData::None,
                loading: false,
                scroll: 0,
            },
            cover: CoverState {
                protocol: None,
                loading: false,
                attempted: false,
            },
            picker,
            now_playing: None,
            history: crate::history::PlaybackHistory::default(),
            history_selected: 0,
            sponsor_video_id: String::new(),
            sponsor_segments: Vec::new(),
            current_video_id: None,
            stream_attempt: 0,
            load_requested: false,
            resume_position: None,
            resume_target_id: None,
            help_visible: false,
            help_scroll: 0,
            search_seq: 0,
            home_seq: 0,
            auth_seq: 0,
            browse_seq: 0,
            library_seq: 0,
            keymap,
            ui_layout: crate::ui::UiLayout::default(),
            last_click: None,
            last_persist: Instant::now(),
            persist_task: None,
            track_tasks: TrackTasks::default(),
            search_tasks: std::array::from_fn(|_| None),
            browse_task: None,
        }
    }

    pub fn key_label(&self, action: Action) -> String {
        let key = self
            .keymap
            .iter()
            .find_map(|(key, bound)| (*bound == action).then_some(key));
        match key {
            Some(KeyCode::Char(' ')) => "Space".into(),
            Some(KeyCode::Char(c)) => c.to_string(),
            Some(KeyCode::Tab) => "Tab".into(),
            Some(KeyCode::Left) => "Left".into(),
            Some(KeyCode::Right) => "Right".into(),
            Some(KeyCode::Up) => "Up".into(),
            Some(KeyCode::Down) => "Down".into(),
            Some(other) => format!("{other:?}"),
            None => "?".into(),
        }
    }

    pub fn toast(&mut self, msg: impl Into<String>) {
        self.status = Some(msg.into());
        self.status_ttl = 12; // ~3s at 250ms ticks
    }

    /// How many rows a `cols`-wide square cover needs.
    ///
    /// Terminal cells are taller than they are wide, and the exact ratio
    /// varies by font, so ask the picker rather than assuming 1:2 — getting
    /// this wrong is what makes the art come out stretched.
    pub fn cover_rows(&self, cols: u16) -> u16 {
        let (fw, fh) = match &self.picker {
            Some(p) => {
                let fs = p.font_size();
                (fs.width.max(1) as u32, fs.height.max(1) as u32)
            }
            None => (1, 2),
        };
        ((cols as u32 * fw + fh / 2) / fh).max(1) as u16
    }

    pub fn restore_session(&mut self) {
        let path = self.session_path();
        let Ok(Some(saved)) = crate::session::SavedSession::load(&path) else {
            return;
        };
        let resume_target_id = saved
            .current
            .and_then(|i| saved.tracks.get(i))
            .map(|t| t.video_id.clone());
        if self
            .queue
            .restore(saved.tracks, saved.current, saved.repeat)
        {
            self.radio_on = saved.radio_on;
            self.queue_selected = saved.current.unwrap_or(0);
            self.resume_position = saved
                .current
                .and_then(|_| (saved.position > 3.0).then_some(saved.position));
            self.resume_target_id = resume_target_id;
            self.toast("已恢复上次队列，按 Enter 继续播放");
        }
    }

    fn session_path(&self) -> std::path::PathBuf {
        self.data_dir.join("playback-session.json")
    }

    pub fn save_session(&self) {
        match crate::session::SavedSession::from_queue(&self.queue, self.pb.time_pos, self.radio_on)
        {
            Some(saved) => {
                if let Err(e) = saved.save(&self.session_path()) {
                    tracing::warn!("{e:#}");
                }
            }
            None => {
                let _ = std::fs::remove_file(self.session_path());
            }
        }
    }

    fn maybe_persist(&mut self) {
        if self
            .persist_task
            .as_ref()
            .is_some_and(|task| !task.is_finished())
            || self.last_persist.elapsed() < Duration::from_secs(15)
        {
            return;
        }
        self.persist_task = None;
        self.last_persist = Instant::now();
        if self.queue.is_empty() && self.history.entries().is_empty() {
            return;
        }
        let saved =
            crate::session::SavedSession::from_queue(&self.queue, self.pb.time_pos, self.radio_on);
        let history = self.history.clone();
        let session_path = self.session_path();
        let history_path = self.data_dir.join("playback-history.json");
        self.persist_task = Some(tokio::task::spawn_blocking(move || {
            if let Some(saved) = saved {
                if let Err(e) = saved.save(&session_path) {
                    tracing::warn!("{e:#}");
                }
            } else if let Err(e) = std::fs::remove_file(&session_path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!("删除旧播放会话失败: {e}");
                }
            }
            if let Err(e) = history.save(&history_path) {
                tracing::warn!("{e:#}");
            }
        }));
    }

    pub async fn wait_persistence(&mut self) {
        if let Some(task) = self.persist_task.take() {
            let _ = task.await;
        }
    }

    pub fn load_history(&mut self) {
        match crate::history::PlaybackHistory::load(&self.data_dir.join("playback-history.json")) {
            Ok(history) => self.history = history,
            Err(e) => tracing::warn!("{e:#}"),
        }
    }

    pub fn save_history(&self) {
        if let Err(e) = self
            .history
            .save(&self.data_dir.join("playback-history.json"))
        {
            tracing::warn!("{e:#}");
        }
    }

    /// Kick off startup fetches (home recommendations).
    pub fn on_start(&mut self) {
        self.load_home();
    }

    fn load_home(&mut self) {
        if self.home.loading {
            return;
        }
        self.home.loading = true;
        self.home_seq = self.home_seq.wrapping_add(1);
        let seq = self.home_seq;
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api.home().await.map_err(|e| format!("{e}"));
            let _ = tx.send(AppEvent::Api(ApiMsg::HomeDone { seq, result }));
        });
    }

    /// Returns whether this event changed pixels that need to be redrawn.
    pub fn handle(&mut self, ev: AppEvent) -> bool {
        match ev {
            AppEvent::Key(key) => self.on_key(key),
            AppEvent::Mouse(me) => self.on_mouse(me),
            AppEvent::Paste(text) => {
                if let Some((_, buf)) = self.input.as_mut() {
                    buf.push_str(&text);
                }
            }
            AppEvent::Resize => {}
            AppEvent::Tick => {
                let mut redraw = false;
                if self.status_ttl > 0 {
                    self.status_ttl -= 1;
                    if self.status_ttl == 0 {
                        self.status = None;
                        redraw = true;
                    }
                }
                if self.pb.loading {
                    self.pb.loading_secs += 0.25;
                    redraw = true;
                }
                self.maybe_persist();
                return redraw;
            }
            AppEvent::Player { epoch, event } => {
                if epoch != self.player_epoch {
                    return false;
                }
                self.on_player_event(event);
            }
            AppEvent::EngineChecked {
                engine,
                mpv_path,
                ytdlp_path,
            } => {
                self.finish_engine_switch(engine, mpv_path, ytdlp_path);
            }
            AppEvent::Api(msg) => self.on_api_msg(msg),
        }
        true
    }

    // ---------- playback ----------

    fn start_track_at(&mut self, index: usize) {
        let Some(track) = self.queue.track_at(index).cloned() else {
            return;
        };
        self.start_track(track);
    }

    fn retry_stream_resolution(&mut self, play_seq: u64, video_id: String) -> bool {
        if self.stream_attempt >= 2 {
            return false;
        }
        self.stream_attempt += 1;
        self.player.send(PlayerCmd::Stop);
        self.playback_error = None;
        self.load_requested = false;
        self.pb.loading = true;
        self.pb.loading_secs = 0.0;
        self.pb.paused = true;
        self.toast(format!("重新解析播放地址 ({}/2)", self.stream_attempt));
        let api = self.api.clone();
        let tx = self.tx.clone();
        let attempt = self.stream_attempt;
        let format = if self.config.playback.engine == PlaybackEngine::Native {
            StreamFormat::Mp4Aac
        } else {
            StreamFormat::Best
        };
        if let Some(task) = self.track_tasks.stream.take() {
            task.abort();
        }
        self.track_tasks.stream = Some(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
            let result =
                tokio::time::timeout(Duration::from_secs(20), api.stream_url(&video_id, format))
                    .await
                    .map_err(|_| "解析播放地址超时".to_string())
                    .and_then(|result| result.map_err(|e| format!("{e:#}")));
            let _ = tx.send(AppEvent::Api(ApiMsg::StreamResolved {
                play_seq,
                video_id,
                attempt,
                result,
            }));
        }));
        true
    }

    fn native_ended_early(&self) -> Option<f64> {
        let expected = self
            .now_playing
            .as_ref()
            .and_then(|track| track.duration_secs)
            .map(f64::from)
            .unwrap_or(self.pb.duration);
        (expected >= 20.0
            && self.pb.time_pos.is_finite()
            && self.pb.time_pos + 5.0 < expected * 0.7)
            .then_some(expected)
    }

    fn fail_current_track(&mut self, error: String) {
        tracing::warn!(position = self.pb.time_pos, "current track failed: {error}");
        self.player.send(PlayerCmd::Stop);
        self.load_requested = false;
        self.pb.loading = false;
        self.pb.paused = true;
        self.playback_error = Some("当前曲目播放中断：按 R 重试，或按 n 播放下一首".into());
        self.toast(error);
    }

    fn start_track(&mut self, track: Track) {
        self.track_tasks.abort();
        self.play_seq = self.play_seq.wrapping_add(1);
        let play_seq = self.play_seq;
        self.player.send(PlayerCmd::Stop);
        self.now_playing = Some(track.clone());
        self.history.record(track.clone());
        self.pb.current_title = Some(format!("{} - {}", track.title, track.artists));
        self.pb.loading = true;
        self.pb.loading_secs = 0.0;
        self.pb.time_pos = 0.0;
        self.pb.duration = track.duration_secs.map(f64::from).unwrap_or(0.0);
        if self.resume_target_id.take().as_deref() != Some(track.video_id.as_str()) {
            self.resume_position = None;
        }
        self.current_video_id = Some(track.video_id.clone());
        self.playback_error = None;
        self.stream_attempt = 0;
        self.load_requested = false;

        // Resolve the stream URL in-process rather than letting mpv shell out
        // to yt-dlp. The result is applied in `ApiMsg::StreamResolved`, which
        // drops it if the user has moved on to another track meanwhile.
        {
            let api = self.api.clone();
            let tx = self.tx.clone();
            let vid = track.video_id.clone();
            let format = if self.config.playback.engine == PlaybackEngine::Native {
                StreamFormat::Mp4Aac
            } else {
                StreamFormat::Best
            };
            self.track_tasks.stream = Some(tokio::spawn(async move {
                let result =
                    tokio::time::timeout(Duration::from_secs(20), api.stream_url(&vid, format))
                        .await
                        .map_err(|_| "解析播放地址超时".to_string())
                        .and_then(|result| result.map_err(|e| format!("{e:#}")));
                let _ = tx.send(AppEvent::Api(ApiMsg::StreamResolved {
                    play_seq,
                    video_id: vid,
                    attempt: 0,
                    result,
                }));
            }));
        }

        // Lyrics for the new track.
        self.lyrics = LyricsState {
            data: LyricsData::None,
            loading: self.config.lyrics.enabled,
            scroll: 0,
        };
        if self.config.lyrics.enabled {
            let http = self.http.clone();
            let api = self.api.clone();
            let tx = self.tx.clone();
            let t = track.clone();
            self.track_tasks.lyrics = Some(tokio::spawn(async move {
                let data = lyrics::fetch(http, api, t.clone()).await;
                let _ = tx.send(AppEvent::Api(ApiMsg::LyricsDone { play_seq, data }));
            }));
        }

        // SponsorBlock segments for the new track.
        // Album art - only fetched when the terminal can display it.
        self.cover = CoverState {
            protocol: None,
            loading: false,
            attempted: false,
        };
        self.maybe_load_cover();

        self.sponsor_video_id = track.video_id.clone();
        self.sponsor_segments.clear();
        if self.config.sponsorblock.enabled {
            let http = self.http.clone();
            let tx = self.tx.clone();
            let vid = track.video_id.clone();
            let cats = self.config.sponsorblock.categories.clone();
            self.track_tasks.sponsor = Some(tokio::spawn(async move {
                let segments = sponsorblock::fetch(http, vid.clone(), cats).await;
                let _ = tx.send(AppEvent::Api(ApiMsg::SponsorDone { play_seq, segments }));
            }));
        }

        self.maybe_refill_radio();
    }

    fn maybe_load_cover(&mut self) {
        if self.main_view != MainView::NowPlaying || self.cover.attempted || self.picker.is_none() {
            return;
        }
        let Some(url) = self.now_playing.as_ref().and_then(|t| t.cover_url.clone()) else {
            return;
        };
        self.cover.attempted = true;
        self.cover.loading = true;
        let http = self.http.clone();
        let tx = self.tx.clone();
        let play_seq = self.play_seq;
        self.track_tasks.cover = Some(tokio::spawn(async move {
            let image = fetch_cover(http, url).await.map(Box::new);
            let _ = tx.send(AppEvent::Api(ApiMsg::CoverLoaded { play_seq, image }));
        }));
    }

    /// Play `start` within `tracks`, making that list the queue so playback
    /// continues in list order (an album keeps playing as an album).
    fn play_context(&mut self, tracks: Vec<Track>, start: usize) {
        let count = tracks.len();
        match self.queue.set_context(tracks, start) {
            Some(idx) => {
                self.queue_seq = self.queue_seq.wrapping_add(1);
                self.start_track_at(idx);
                if count > 1 {
                    self.toast(format!("播放中 - 队列 {} 首（第 {} 首）", count, idx + 1));
                }
            }
            None => self.toast("列表为空"),
        }
    }

    fn stop_playback_ui(&mut self) {
        self.track_tasks.abort();
        self.lyrics.loading = false;
        self.cover.loading = false;
        self.play_seq = self.play_seq.wrapping_add(1);
        self.player.send(PlayerCmd::Stop);
        self.pb.current_title = None;
        self.pb.loading = false;
        self.pb.time_pos = 0.0;
        self.pb.duration = 0.0;
        self.current_video_id = None;
        self.playback_error = None;
        self.resume_target_id = None;
        self.resume_position = None;
        self.load_requested = false;
    }

    fn advance(&mut self, advance: Advance) {
        match advance {
            Advance::Play(i) => self.start_track_at(i),
            Advance::Stop => {
                self.stop_playback_ui();
                self.toast("队列播放完毕");
            }
        }
    }

    fn on_player_event(&mut self, ev: PlayerEvent) {
        match ev {
            PlayerEvent::Ready => {
                self.pb.alive = true;
                if self.restart_pending_track {
                    self.restart_pending_track = false;
                    if let Some(index) = self.queue.current_index() {
                        self.resume_position = (self.pb.time_pos > 3.0).then_some(self.pb.time_pos);
                        self.resume_target_id = self.current_video_id.clone();
                        self.start_track_at(index);
                    }
                }
            }
            PlayerEvent::InitFailed(e) => {
                self.restart_pending_track = false;
                self.restart_resume_paused = false;
                self.track_tasks.abort();
                self.pb.alive = false;
                self.pb.loading = false;
                self.pb.paused = true;
                self.load_requested = false;
                self.toast(format!("播放器启动失败: {e}"));
            }
            PlayerEvent::Track { play_seq, event } => {
                if play_seq == self.play_seq {
                    self.on_track_event(event);
                }
            }
            PlayerEvent::Volume(v) => self.pb.volume = v,
            PlayerEvent::Muted(m) => self.pb.muted = m,
            PlayerEvent::Died => {
                self.track_tasks.abort();
                self.pb.alive = false;
                self.pb.loading = false;
                self.pb.paused = true;
                self.load_requested = false;
                self.toast("播放器已退出 - 按 R 重启");
            }
        }
    }

    fn on_track_event(&mut self, ev: TrackEvent) {
        match ev {
            TrackEvent::FileLoaded => {
                if !self.load_requested {
                    return;
                }
                self.load_requested = false;
                self.pb.loading = false;
                self.pb.loading_secs = 0.0;
                self.pb.time_pos = 0.0;
                if let Some(position) = self.resume_position.take() {
                    self.player.send(PlayerCmd::SeekAbs(position));
                    self.pb.time_pos = position;
                }
                if self.restart_resume_paused {
                    self.restart_resume_paused = false;
                    self.player.send(PlayerCmd::TogglePause);
                }
            }
            TrackEvent::TimePos(t) => {
                if !self.pb.loading && self.current_video_id.is_some() {
                    self.pb.time_pos = t;
                    self.check_sponsor_skip(t);
                }
            }
            TrackEvent::Duration(d)
                if self.current_video_id.is_some() && (!self.pb.loading || self.load_requested) =>
            {
                self.pb.duration = d;
            }
            TrackEvent::Duration(_) => {}
            TrackEvent::Paused(p) => self.pb.paused = p,
            TrackEvent::TrackEnded => {
                if self.pb.loading || self.current_video_id.is_none() {
                    return;
                }
                if self.config.playback.engine == PlaybackEngine::Native {
                    if let Some(expected) = self.native_ended_early() {
                        tracing::warn!(
                            position = self.pb.time_pos,
                            expected,
                            "native playback ended early"
                        );
                        if self.pb.time_pos > 1.0 {
                            self.resume_position = Some(self.pb.time_pos);
                        }
                        if self.retry_stream_resolution(
                            self.play_seq,
                            self.current_video_id.clone().unwrap(),
                        ) {
                            return;
                        }
                        self.fail_current_track("内置播放器提前结束，已停止自动切歌".into());
                        return;
                    }
                }
                let adv = self.queue.advance_on_end();
                self.advance(adv);
            }
            TrackEvent::LoadFailed(e) => {
                if self.current_video_id.is_none() || (self.pb.loading && !self.load_requested) {
                    return;
                }
                if e.starts_with("音频下载失败") {
                    if self.pb.time_pos > 1.0 {
                        self.resume_position = Some(self.pb.time_pos);
                    }
                    if self.retry_stream_resolution(
                        self.play_seq,
                        self.current_video_id.clone().unwrap(),
                    ) {
                        return;
                    }
                }
                self.fail_current_track(format!("加载失败: {e}"));
            }
        }
    }

    fn check_sponsor_skip(&mut self, t: f64) {
        if !self.config.sponsorblock.enabled {
            return;
        }
        if self.current_video_id.as_deref() != Some(self.sponsor_video_id.as_str()) {
            return;
        }
        if let Some((to, category, len)) = sponsorblock::check_skip(&mut self.sponsor_segments, t) {
            self.player.send(PlayerCmd::SeekAbs(to));
            self.toast(format!(
                "已跳过{} {:.0}s",
                sponsorblock::category_label(&category),
                len
            ));
        }
    }

    // ---------- async task launchers ----------

    fn fire_kind_search(&mut self) {
        if self.search.query.is_empty() || self.search.is_loading() {
            return;
        }
        let kind_idx = self.search.kind_idx;
        self.search.loading[kind_idx] = true;
        self.search.errors[kind_idx] = None;
        let seq = self.search_seq;
        let kind = self.search.kind();
        let query = self.search.query.clone();
        let api = self.api.clone();
        let tx = self.tx.clone();
        if let Some(task) = self.search_tasks[kind_idx].take() {
            task.abort();
        }
        self.search_tasks[kind_idx] = Some(tokio::spawn(async move {
            let result = api.search(&query, kind).await.map_err(|e| format!("{e}"));
            let _ = tx.send(AppEvent::Api(ApiMsg::SearchDone { seq, kind, result }));
        }));
    }

    fn open_album(&mut self, id: String, title: String) {
        if self.detail_retry_hint.take().is_some() {
            self.status = None;
            self.status_ttl = 0;
        }
        if let Some(task) = self.browse_task.take() {
            task.abort();
        }
        if self.main_view != MainView::Browse {
            self.browse_stack.clear();
            self.browse_root_view = self.main_view;
        }
        self.browse_stack.push(BrowsePage::Album {
            title,
            data: None,
            selected: 0,
        });
        self.main_view = MainView::Browse;
        self.browse_seq += 1;
        let seq = self.browse_seq;
        let api = self.api.clone();
        let tx = self.tx.clone();
        self.browse_task = Some(tokio::spawn(async move {
            let result = api.album(&id).await.map_err(|e| format!("{e}"));
            let _ = tx.send(AppEvent::Api(ApiMsg::AlbumDone { seq, result }));
        }));
    }

    fn open_artist(&mut self, id: String, name: String) {
        if self.detail_retry_hint.take().is_some() {
            self.status = None;
            self.status_ttl = 0;
        }
        if let Some(task) = self.browse_task.take() {
            task.abort();
        }
        if self.main_view != MainView::Browse {
            self.browse_stack.clear();
            self.browse_root_view = self.main_view;
        }
        self.browse_stack.push(BrowsePage::Artist {
            name,
            data: None,
            selected: 0,
        });
        self.main_view = MainView::Browse;
        self.browse_seq += 1;
        let seq = self.browse_seq;
        let api = self.api.clone();
        let tx = self.tx.clone();
        self.browse_task = Some(tokio::spawn(async move {
            let result = api.artist(&id).await.map_err(|e| format!("{e}"));
            let _ = tx.send(AppEvent::Api(ApiMsg::ArtistDone { seq, result }));
        }));
    }

    fn open_playlist(&mut self, id: String, title: String) {
        if self.detail_retry_hint.take().is_some() {
            self.status = None;
            self.status_ttl = 0;
        }
        if let Some(task) = self.browse_task.take() {
            task.abort();
        }
        if self.main_view != MainView::Browse {
            self.browse_stack.clear();
            self.browse_root_view = self.main_view;
        }
        self.browse_stack.push(BrowsePage::Playlist {
            title,
            data: None,
            selected: 0,
        });
        self.main_view = MainView::Browse;
        self.browse_seq += 1;
        let seq = self.browse_seq;
        let api = self.api.clone();
        let tx = self.tx.clone();
        self.browse_task = Some(tokio::spawn(async move {
            let result = api.playlist(&id).await.map_err(|e| format!("{e}"));
            let _ = tx.send(AppEvent::Api(ApiMsg::PlaylistDone { seq, result }));
        }));
    }

    fn maybe_refill_radio(&mut self) {
        if !self.radio_on || self.radio_inflight || self.queue.upcoming_count() >= 3 {
            return;
        }
        let Some(seed) = self.queue.items().last().map(|t| t.video_id.clone()) else {
            return;
        };
        self.radio_inflight = true;
        let queue_seq = self.queue_seq;
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api.radio(&seed).await.map_err(|e| format!("{e}"));
            let _ = tx.send(AppEvent::Api(ApiMsg::RadioDone {
                queue_seq,
                seed,
                result,
            }));
        });
    }

    // ---------- async results ----------

    fn on_api_msg(&mut self, msg: ApiMsg) {
        match msg {
            ApiMsg::SearchDone { seq, kind, result } => {
                if seq != self.search_seq {
                    return; // stale query
                }
                let kind_idx = SearchKind::ALL.iter().position(|k| *k == kind).unwrap_or(0);
                self.search_tasks[kind_idx] = None;
                self.search.loading[kind_idx] = false;
                match result {
                    Ok(r) => {
                        self.search.errors[kind_idx] = None;
                        match kind {
                            SearchKind::Songs => self.search.results.tracks = r.tracks,
                            SearchKind::Albums => self.search.results.albums = r.albums,
                            SearchKind::Artists => self.search.results.artists = r.artists,
                            SearchKind::Playlists => self.search.results.playlists = r.playlists,
                        }
                        if self.search.kind() == kind {
                            self.search.selected = 0;
                        }
                    }
                    Err(e) => {
                        self.search.errors[kind_idx] = Some(e.clone());
                        self.toast(format!("搜索失败: {e}"));
                    }
                }
            }
            ApiMsg::AlbumDone { seq, result } => {
                if seq != self.browse_seq
                    || !matches!(self.browse_stack.last(), Some(BrowsePage::Album { .. }))
                {
                    return;
                }
                self.browse_task = None;
                match (self.browse_stack.last_mut(), result) {
                    (Some(BrowsePage::Album { data, .. }), Ok(d)) => *data = Some(d),
                    (_, Err(e)) => {
                        self.browse_stack.pop();
                        self.sync_view_after_pop();
                        tracing::warn!("album load failed: {e}");
                        self.detail_retry_hint = Some("专辑打开失败，按 Enter 重试当前项".into());
                        self.toast(format!("专辑失败，按 Enter 重试: {e}"));
                    }
                    _ => {}
                }
            }
            ApiMsg::ArtistDone { seq, result } => {
                if seq != self.browse_seq
                    || !matches!(self.browse_stack.last(), Some(BrowsePage::Artist { .. }))
                {
                    return;
                }
                self.browse_task = None;
                match (self.browse_stack.last_mut(), result) {
                    (Some(BrowsePage::Artist { data, .. }), Ok(d)) => *data = Some(d),
                    (_, Err(e)) => {
                        self.browse_stack.pop();
                        self.sync_view_after_pop();
                        tracing::warn!("artist load failed: {e}");
                        self.detail_retry_hint = Some("歌手打开失败，按 Enter 重试当前项".into());
                        self.toast(format!("歌手失败，按 Enter 重试: {e}"));
                    }
                    _ => {}
                }
            }
            ApiMsg::PlaylistDone { seq, result } => {
                if seq != self.browse_seq
                    || !matches!(self.browse_stack.last(), Some(BrowsePage::Playlist { .. }))
                {
                    return;
                }
                self.browse_task = None;
                match (self.browse_stack.last_mut(), result) {
                    (Some(BrowsePage::Playlist { data, .. }), Ok(d)) => *data = Some(d),
                    (_, Err(e)) => {
                        self.browse_stack.pop();
                        self.sync_view_after_pop();
                        tracing::warn!("playlist load failed: {e}");
                        self.detail_retry_hint = Some("歌单打开失败，按 Enter 重试当前项".into());
                        self.toast(format!("歌单失败，按 Enter 重试: {e}"));
                    }
                    _ => {}
                }
            }
            ApiMsg::RadioDone {
                queue_seq,
                seed,
                result,
            } => {
                self.radio_inflight = false;
                if !self.radio_on
                    || queue_seq != self.queue_seq
                    || self.queue.items().last().map(|t| t.video_id.as_str()) != Some(seed.as_str())
                {
                    self.maybe_refill_radio();
                    return;
                }
                match result {
                    Ok(tracks) => {
                        let added = self.queue.append_unique(tracks);
                        if added > 0 {
                            self.toast(format!("电台已补充 {added} 首"));
                        }
                        // Queue had run dry while the request was in flight.
                        if self.pb.current_title.is_none() && added > 0 {
                            let adv = self.queue.next_manual();
                            if let Advance::Play(i) = adv {
                                self.start_track_at(i);
                            }
                        }
                    }
                    Err(e) => {
                        if self.radio_on {
                            self.toast(format!("电台获取失败: {e}"));
                        }
                    }
                }
            }
            ApiMsg::StreamResolved {
                play_seq,
                video_id,
                attempt,
                result,
            } => {
                if play_seq != self.play_seq
                    || self.current_video_id.as_deref() != Some(video_id.as_str())
                    || attempt != self.stream_attempt
                {
                    return;
                }
                self.track_tasks.stream = None;
                let url = match result {
                    Ok(url) => url,
                    Err(_e) if self.retry_stream_resolution(play_seq, video_id.clone()) => {
                        return;
                    }
                    Err(e) => {
                        if self.config.ytdlp_path.is_none()
                            || self.config.playback.engine == PlaybackEngine::Native
                        {
                            tracing::warn!("stream resolution failed, no ytdl fallback: {e}");
                            self.fail_current_track(format!("解析播放地址失败: {e}"));
                            return;
                        }
                        tracing::warn!("stream resolution failed, falling back to ytdl hook: {e}");
                        format!("https://music.youtube.com/watch?v={video_id}")
                    }
                };
                self.load_requested = self.player.send_checked(PlayerCmd::Load {
                    url,
                    play_seq,
                    wait_for_complete: attempt > 0
                        && self.config.playback.engine == PlaybackEngine::Native,
                });
                if !self.load_requested {
                    self.pb.loading = false;
                    self.toast("播放器未运行 - 按 R 重启");
                }
            }
            ApiMsg::LyricsDone { play_seq, data } => {
                if play_seq == self.play_seq {
                    self.track_tasks.lyrics = None;
                    self.lyrics.data = data;
                    self.lyrics.loading = false;
                    self.lyrics.scroll = 0;
                }
            }
            ApiMsg::SponsorDone { play_seq, segments } => {
                if play_seq == self.play_seq {
                    self.track_tasks.sponsor = None;
                    if !segments.is_empty() {
                        tracing::info!(
                            "sponsorblock: {} segments for {}",
                            segments.len(),
                            self.sponsor_video_id
                        );
                    }
                    self.sponsor_segments = segments;
                }
            }
            ApiMsg::CoverLoaded { play_seq, image } => {
                // A late arrival for a track we already moved past is dropped.
                if play_seq == self.play_seq {
                    self.track_tasks.cover = None;
                    self.cover.loading = false;
                    if let (Some(picker), Some(image)) = (&self.picker, image) {
                        self.cover.protocol = Some(picker.new_resize_protocol(*image));
                    }
                }
            }
            ApiMsg::HomeDone { seq, result } => {
                if seq != self.home_seq {
                    return;
                }
                self.home.loading = false;
                match result {
                    Ok((tracks, albums)) => {
                        self.home.tracks = tracks;
                        self.home.albums = albums;
                        self.home.selected = 0;
                    }
                    Err(e) if self.main_view == MainView::Home => {
                        self.toast(format!("加载推荐失败: {e}"));
                    }
                    Err(e) => tracing::warn!("home load failed in background: {e}"),
                }
            }
            ApiMsg::LibraryTracks { seq, title, result } => {
                if seq != self.library_seq {
                    return;
                }
                self.library.loading = false;
                if self.main_view != MainView::Library {
                    return;
                }
                match result {
                    Ok(tracks) => {
                        self.library.failed = false;
                        self.push_browse(BrowsePage::Tracks {
                            title,
                            tracks,
                            selected: 0,
                        });
                    }
                    Err(e) => {
                        self.library.failed = true;
                        tracing::warn!("library tracks load failed: {e}");
                        self.toast(format!("加载失败，按 Enter 重试: {e}"));
                    }
                }
            }
            ApiMsg::LibraryPlaylists { seq, result } => {
                if seq != self.library_seq {
                    return;
                }
                self.library.loading = false;
                if self.main_view != MainView::Library {
                    return;
                }
                match result {
                    Ok(items) => {
                        self.library.failed = false;
                        self.push_browse(BrowsePage::Playlists {
                            title: "我的歌单".into(),
                            items,
                            selected: 0,
                        });
                    }
                    Err(e) => {
                        self.library.failed = true;
                        tracing::warn!("library playlists load failed: {e}");
                        self.toast(format!("加载失败，按 Enter 重试: {e}"));
                    }
                }
            }
            ApiMsg::LibraryAlbums { seq, result } => {
                if seq != self.library_seq {
                    return;
                }
                self.library.loading = false;
                if self.main_view != MainView::Library {
                    return;
                }
                match result {
                    Ok(items) => {
                        self.library.failed = false;
                        self.push_browse(BrowsePage::Albums {
                            title: "收藏的专辑".into(),
                            items,
                            selected: 0,
                        });
                    }
                    Err(e) => {
                        self.library.failed = true;
                        tracing::warn!("library albums load failed: {e}");
                        self.toast(format!("加载失败，按 Enter 重试: {e}"));
                    }
                }
            }
            ApiMsg::LibraryArtists { seq, result } => {
                if seq != self.library_seq {
                    return;
                }
                self.library.loading = false;
                if self.main_view != MainView::Library {
                    return;
                }
                match result {
                    Ok(items) => {
                        self.library.failed = false;
                        self.push_browse(BrowsePage::Artists {
                            title: "关注的歌手".into(),
                            items,
                            selected: 0,
                        });
                    }
                    Err(e) => {
                        self.library.failed = true;
                        tracing::warn!("library artists load failed: {e}");
                        self.toast(format!("加载失败，按 Enter 重试: {e}"));
                    }
                }
            }
            ApiMsg::LoginDone { seq, result } => {
                if seq != self.auth_seq {
                    return;
                }
                self.library.loading = false;
                self.login.busy = false;
                match result {
                    Ok(()) => {
                        self.library.selected = 0;
                        self.library.failed = false;
                        if self.main_view == MainView::Library {
                            self.toast("登录成功 - Enter 打开音乐库");
                        }
                    }
                    // Multi-line hints render as one status line; keep the
                    // first line, which carries the actionable part.
                    Err(e) => {
                        let first = e.lines().next().unwrap_or(&e).to_string();
                        if self.main_view == MainView::Library {
                            self.toast(format!("登录失败: {first}"));
                        } else {
                            tracing::warn!("background login failed: {first}");
                        }
                    }
                }
            }
            ApiMsg::LogoutDone { seq, result } => {
                if seq != self.auth_seq {
                    return;
                }
                self.login.busy = false;
                self.library.loading = false;
                match result {
                    Ok(()) => {
                        self.login.selected = 0;
                        self.library.failed = false;
                        self.library_seq = self.library_seq.wrapping_add(1);
                        if self.main_view == MainView::Library {
                            self.toast("已退出登录");
                        }
                    }
                    Err(e) if self.main_view == MainView::Library => {
                        self.toast(format!("退出失败: {e}"));
                    }
                    Err(e) => tracing::warn!("background logout failed: {e}"),
                }
            }
        }
    }

    fn push_browse(&mut self, page: BrowsePage) {
        if self.main_view != MainView::Browse {
            self.browse_stack.clear();
            self.browse_root_view = self.main_view;
        }
        self.browse_stack.push(page);
        self.main_view = MainView::Browse;
        self.focus = Focus::Main;
    }

    // ---------- key handling ----------

    fn on_key(&mut self, key: KeyEvent) {
        if key.code != KeyCode::Enter {
            self.detail_retry_hint = None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        if self.help_visible {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll = self.help_scroll.saturating_add(1).min(30)
                }
                KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(10),
                KeyCode::PageDown => self.help_scroll = self.help_scroll.saturating_add(10).min(30),
                _ => self.help_visible = false,
            }
            return;
        }
        if self.input.is_some() {
            self.on_input_key(key);
            return;
        }

        if let Some(action) = self.keymap.get(&key.code).copied() {
            self.run_action(action);
            return;
        }
        match self.focus {
            Focus::Queue => self.on_queue_key(key),
            Focus::Main => match self.main_view {
                MainView::Search => self.on_search_key(key),
                MainView::Browse => self.on_browse_key(key),
                MainView::NowPlaying => self.on_lyrics_key(key),
                MainView::Home => self.on_home_key(key),
                MainView::Library => self.on_library_key(key),
                MainView::History => self.on_history_key(key),
                MainView::Settings => self.on_settings_key(key),
            },
        }
    }

    fn run_action(&mut self, action: Action) {
        match action {
            Action::Quit => self.should_quit = true,
            Action::Search => {
                self.input = Some((InputMode::Search, self.search.query.clone()));
            }
            Action::Library => {
                self.focus = Focus::Main;
                self.main_view = MainView::Library;
            }
            Action::Help => {
                self.help_visible = true;
                self.help_scroll = 0;
            }
            Action::FocusToggle => {
                self.focus = match self.focus {
                    Focus::Main => Focus::Queue,
                    Focus::Queue => Focus::Main,
                };
            }
            Action::PlayPause => self.player.send(PlayerCmd::TogglePause),
            Action::Mute => self.player.send(PlayerCmd::ToggleMute),
            Action::SeekBack => self.player.send(PlayerCmd::SeekRel(-5.0)),
            Action::SeekFwd => self.player.send(PlayerCmd::SeekRel(5.0)),
            Action::VolDown => self.player.send(PlayerCmd::AddVolume(-5)),
            Action::VolUp => self.player.send(PlayerCmd::AddVolume(5)),
            Action::NextTrack => {
                let adv = self.queue.next_manual();
                self.advance(adv);
            }
            Action::PrevTrack => {
                if self.pb.time_pos > 3.0 {
                    self.player.send(PlayerCmd::SeekAbs(0.0));
                } else {
                    let adv = self.queue.prev_manual();
                    self.advance(adv);
                }
            }
            Action::RepeatCycle => {
                self.queue.repeat = self.queue.repeat.cycle();
                self.toast(format!("循环模式: {}", self.queue.repeat.label()));
            }
            Action::RadioToggle => {
                self.radio_on = !self.radio_on;
                if !self.radio_on {
                    self.queue_seq = self.queue_seq.wrapping_add(1);
                }
                self.toast(if self.radio_on {
                    "电台续播: 开"
                } else {
                    "电台续播: 关"
                });
                self.maybe_refill_radio();
            }
            Action::Shuffle => {
                self.queue.shuffle_upcoming();
                self.toast("已打乱待播队列");
            }
            Action::LyricsToggle => {
                if self.main_view == MainView::NowPlaying {
                    self.main_view = self.prev_main_view;
                } else {
                    self.prev_main_view = self.main_view;
                    self.main_view = MainView::NowPlaying;
                    self.maybe_load_cover();
                }
            }
            Action::RestartPlayer => {
                if self.pb.alive && self.playback_error.is_some() {
                    if let Some(index) = self.queue.current_index() {
                        self.start_track_at(index);
                    }
                } else if !self.pb.alive {
                    self.restart_pending_track = self.current_video_id.is_some();
                    self.player_epoch = self.player_epoch.wrapping_add(1);
                    self.player_restart_requested = true;
                }
            }
            Action::History => {
                self.focus = Focus::Main;
                self.main_view = MainView::History;
            }
            Action::Settings => {
                self.focus = Focus::Main;
                self.main_view = MainView::Settings;
                self.settings_selected =
                    usize::from(self.config.playback.engine == PlaybackEngine::Mpv);
            }
        }
    }

    fn on_settings_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('1') => self.settings_selected = 0,
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('2') => self.settings_selected = 1,
            KeyCode::Enter => self.begin_engine_switch(),
            KeyCode::Esc => self.main_view = MainView::Home,
            _ => {}
        }
    }

    fn begin_engine_switch(&mut self) {
        let engine = if self.settings_selected == 0 {
            PlaybackEngine::Native
        } else {
            PlaybackEngine::Mpv
        };
        if engine == self.config.playback.engine || self.engine_switch_pending {
            return;
        }
        self.engine_switch_pending = true;
        self.toast(if engine == PlaybackEngine::Mpv {
            "正在检查 mpv.."
        } else {
            "正在切换到内置播放器.."
        });
        let tx = self.tx.clone();
        let configured = self.config.playback.mpv_path.clone();
        tokio::task::spawn_blocking(move || {
            let mpv_path = if engine == PlaybackEngine::Mpv {
                crate::config::resolve_tool(&configured, "mpv", "mpv")
            } else {
                None
            };
            let ytdlp_path = if mpv_path.is_some() {
                crate::config::resolve_tool("yt-dlp", "yt-dlp", "yt-dlp")
            } else {
                None
            };
            let _ = tx.send(AppEvent::EngineChecked {
                engine,
                mpv_path,
                ytdlp_path,
            });
        });
    }

    fn finish_engine_switch(
        &mut self,
        engine: PlaybackEngine,
        mpv_path: Option<String>,
        ytdlp_path: Option<String>,
    ) {
        if !self.engine_switch_pending {
            return;
        }
        self.engine_switch_pending = false;
        if engine == PlaybackEngine::Mpv && mpv_path.is_none() {
            self.toast("未找到 mpv。请先安装，或在配置中设置 mpv_path");
            return;
        }
        if let Err(e) = crate::config::save_engine(&self.config_dir, engine) {
            self.toast(format!("保存播放器设置失败: {e}"));
            return;
        }
        self.config.playback.engine = engine;
        if let Some(path) = mpv_path {
            self.config.playback.mpv_path = path;
        }
        self.config.ytdlp_path = ytdlp_path;
        self.restart_pending_track = self.current_video_id.is_some();
        self.restart_resume_paused = self.pb.paused;
        self.track_tasks.abort();
        self.player.send(PlayerCmd::Shutdown);
        self.player_epoch = self.player_epoch.wrapping_add(1);
        self.player_restart_requested = true;
        self.pb.alive = false;
        self.toast(if engine == PlaybackEngine::Native {
            "已切换到内置播放器"
        } else {
            "已切换到 mpv"
        });
    }

    fn on_input_key(&mut self, key: KeyEvent) {
        let Some((mode, buf)) = self.input.as_mut() else {
            return;
        };
        let mode = *mode;
        match key.code {
            KeyCode::Esc => self.input = None,
            KeyCode::Enter => {
                let text = self
                    .input
                    .take()
                    .map(|(_, b)| b.trim().to_string())
                    .unwrap_or_default();
                if text.is_empty() {
                    return;
                }
                match mode {
                    InputMode::Search => self.submit_search(text),
                    InputMode::Login => self.submit_login(text),
                }
            }
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Char(c) => buf.push(c),
            _ => {}
        }
    }

    fn submit_search(&mut self, query: String) {
        for task in &mut self.search_tasks {
            if let Some(task) = task.take() {
                task.abort();
            }
        }
        self.search.query = query;
        self.search.results = SearchResults::default();
        self.search.loading = [false; 4];
        self.search.errors = std::array::from_fn(|_| None);
        self.search.selected = 0;
        self.main_view = MainView::Search;
        self.focus = Focus::Main;
        self.search_seq += 1;
        self.fire_kind_search();
    }

    fn submit_login(&mut self, input: String) {
        if self.login.busy {
            return;
        }
        self.login.busy = true;
        self.library.loading = true;
        self.auth_seq = self.auth_seq.wrapping_add(1);
        let seq = self.auth_seq;
        self.toast("正在验证登录..");
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api
                .login_cookie(&input)
                .await
                .map_err(|_| "登录验证失败，请检查 Cookie 是否有效及网络连接".to_string());
            let _ = tx.send(AppEvent::Api(ApiMsg::LoginDone { seq, result }));
        });
    }

    fn on_search_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('g') && self.search.errors[self.search.kind_idx].is_some() {
            self.fire_kind_search();
            return;
        }
        let len = self.search.list_len();
        if nav_list(&mut self.search.selected, len, key.code) {
            return;
        }
        match key.code {
            KeyCode::Char(c @ '1'..='4') => {
                self.search.kind_idx = (c as u8 - b'1') as usize;
                self.search.selected = 0;
                if self.search.list_len() == 0 {
                    self.fire_kind_search();
                }
            }
            KeyCode::Char('[') | KeyCode::Char(']') => {
                let n = SearchKind::ALL.len();
                self.search.kind_idx = if key.code == KeyCode::Char(']') {
                    (self.search.kind_idx + 1) % n
                } else {
                    (self.search.kind_idx + n - 1) % n
                };
                self.search.selected = 0;
                if self.search.list_len() == 0 {
                    self.fire_kind_search();
                }
            }
            KeyCode::Enter => self.activate_search_selection(),
            KeyCode::Char('a') => {
                if let Some(t) = self.selected_search_track().cloned() {
                    self.queue.append(t);
                    self.toast("已加入队列");
                    self.maybe_refill_radio();
                }
            }
            KeyCode::Char('A') => {
                if let Some(t) = self.selected_search_track().cloned() {
                    self.queue.play_next(t);
                    self.toast("将作为下一首播放");
                }
            }
            KeyCode::Esc => self.main_view = MainView::Home,
            _ => {}
        }
    }

    fn selected_search_track(&self) -> Option<&Track> {
        if self.search.kind() == SearchKind::Songs {
            self.search.results.tracks.get(self.search.selected)
        } else {
            None
        }
    }

    fn activate_search_selection(&mut self) {
        let sel = self.search.selected;
        match self.search.kind() {
            SearchKind::Songs => {
                // Continue through the remaining results rather than
                // handing over to radio after a single song.
                let list = self.search.results.tracks.clone();
                self.play_context(list, sel);
            }
            SearchKind::Albums => {
                if let Some(a) = self.search.results.albums.get(sel) {
                    self.open_album(a.id.clone(), a.title.clone());
                }
            }
            SearchKind::Artists => {
                if let Some(a) = self.search.results.artists.get(sel) {
                    self.open_artist(a.id.clone(), a.name.clone());
                }
            }
            SearchKind::Playlists => {
                if let Some(p) = self.search.results.playlists.get(sel) {
                    self.open_playlist(p.id.clone(), p.title.clone());
                }
            }
        }
    }

    fn sync_view_after_pop(&mut self) {
        if self.browse_stack.is_empty() {
            self.main_view = self.browse_root_view;
        }
    }

    fn on_browse_key(&mut self, key: KeyEvent) {
        let Some(page) = self.browse_stack.last_mut() else {
            self.sync_view_after_pop();
            return;
        };
        let rows = page.row_count();
        if nav_list(page.selected_mut(), rows, key.code) {
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Backspace => {
                if let Some(task) = self.browse_task.take() {
                    task.abort();
                }
                self.browse_stack.pop();
                self.browse_seq = self.browse_seq.wrapping_add(1);
                self.sync_view_after_pop();
            }
            KeyCode::Enter => self.activate_browse_selection(false),
            KeyCode::Char('a') => self.activate_browse_selection(true),
            KeyCode::Char('A') => {
                if let Some(t) = self.selected_browse_track().cloned() {
                    self.queue.play_next(t);
                    self.toast("将作为下一首播放");
                }
            }
            KeyCode::Char('P') => self.play_all_browse(),
            _ => {}
        }
    }

    fn selected_browse_track(&self) -> Option<&Track> {
        let page = self.browse_stack.last()?;
        match page {
            BrowsePage::Album { data, selected, .. } => data.as_ref()?.tracks.get(*selected),
            BrowsePage::Playlist { data, selected, .. } => data.as_ref()?.tracks.get(*selected),
            BrowsePage::Artist { data, selected, .. } => {
                let d = data.as_ref()?;
                if *selected < d.top_tracks.len() {
                    d.top_tracks.get(*selected)
                } else {
                    None
                }
            }
            BrowsePage::Tracks {
                tracks, selected, ..
            } => tracks.get(*selected),
            _ => None,
        }
    }

    /// Enter (append=false): play track / open album row.
    /// 'a' (append=true): append track to queue.
    fn activate_browse_selection(&mut self, append: bool) {
        enum Act {
            /// Play the whole list from this index (list order continues).
            PlayFrom(Vec<Track>, usize),
            Append(Track),
            OpenAlbum(String, String),
            OpenArtist(String, String),
            OpenPlaylist(String, String),
        }
        fn track_act(list: &[Track], index: usize, append: bool) -> Option<Act> {
            let track = list.get(index)?;
            Some(if append {
                Act::Append(track.clone())
            } else {
                Act::PlayFrom(list.to_vec(), index)
            })
        }
        let act = {
            let Some(page) = self.browse_stack.last() else {
                return;
            };
            let act = match page {
                BrowsePage::Album { data, selected, .. } => data
                    .as_ref()
                    .and_then(|d| track_act(&d.tracks, *selected, append)),
                BrowsePage::Playlist { data, selected, .. } => data
                    .as_ref()
                    .and_then(|d| track_act(&d.tracks, *selected, append)),
                BrowsePage::Tracks {
                    tracks, selected, ..
                } => track_act(tracks, *selected, append),
                BrowsePage::Artist { data, selected, .. } => data.as_ref().and_then(|d| {
                    if *selected < d.top_tracks.len() {
                        track_act(&d.top_tracks, *selected, append)
                    } else {
                        d.albums
                            .get(*selected - d.top_tracks.len())
                            .map(|a| Act::OpenAlbum(a.id.clone(), a.title.clone()))
                    }
                }),
                BrowsePage::Albums {
                    items, selected, ..
                } => items
                    .get(*selected)
                    .map(|a| Act::OpenAlbum(a.id.clone(), a.title.clone())),
                BrowsePage::Artists {
                    items, selected, ..
                } => items
                    .get(*selected)
                    .map(|a| Act::OpenArtist(a.id.clone(), a.name.clone())),
                BrowsePage::Playlists {
                    items, selected, ..
                } => items
                    .get(*selected)
                    .map(|p| Act::OpenPlaylist(p.id.clone(), p.title.clone())),
            };
            match act {
                Some(a) => a,
                None => return,
            }
        };
        match act {
            Act::PlayFrom(list, idx) => self.play_context(list, idx),
            Act::Append(t) => {
                self.queue.append(t);
                self.toast("已加入队列");
                self.maybe_refill_radio();
            }
            Act::OpenAlbum(id, title) => self.open_album(id, title),
            Act::OpenArtist(id, name) => self.open_artist(id, name),
            Act::OpenPlaylist(id, title) => self.open_playlist(id, title),
        }
    }

    /// 'P' on a browse page: queue every track on the page and start playing
    /// from the first one.
    fn play_all_browse(&mut self) {
        let tracks: Vec<Track> = {
            let Some(page) = self.browse_stack.last() else {
                return;
            };
            match page {
                BrowsePage::Album { data, .. } => {
                    data.as_ref().map(|d| d.tracks.clone()).unwrap_or_default()
                }
                BrowsePage::Playlist { data, .. } => {
                    data.as_ref().map(|d| d.tracks.clone()).unwrap_or_default()
                }
                BrowsePage::Tracks { tracks, .. } => tracks.clone(),
                BrowsePage::Artist { data, .. } => data
                    .as_ref()
                    .map(|d| d.top_tracks.clone())
                    .unwrap_or_default(),
                _ => Vec::new(),
            }
        };
        self.play_context(tracks, 0);
    }

    fn on_lyrics_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.main_view = self.prev_main_view,
            KeyCode::Up | KeyCode::Char('k') => {
                self.lyrics.scroll = self.lyrics.scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.lyrics.scroll = self.lyrics.scroll.saturating_add(1);
            }
            _ => {}
        }
    }

    fn on_home_key(&mut self, key: KeyEvent) {
        let rows = self.home.row_count();
        if rows == 0 && key.code == KeyCode::Char('g') {
            self.load_home(); // retry after failure
            return;
        }
        if nav_list(&mut self.home.selected, rows, key.code) {
            return;
        }
        match key.code {
            KeyCode::Enter => self.activate_home_selection(),
            KeyCode::Char('a') | KeyCode::Char('A') => {
                let sel = self.home.selected;
                if let Some(t) = self.home.tracks.get(sel).cloned() {
                    if key.code == KeyCode::Char('a') {
                        self.queue.append(t);
                        self.toast("已加入队列");
                        self.maybe_refill_radio();
                    } else {
                        self.queue.play_next(t);
                        self.toast("将作为下一首播放");
                    }
                }
            }
            _ => {}
        }
    }

    fn activate_home_selection(&mut self) {
        let sel = self.home.selected;
        if sel < self.home.tracks.len() {
            let list = self.home.tracks.clone();
            self.play_context(list, sel);
        } else if let Some(a) = self.home.albums.get(sel - self.home.tracks.len()) {
            self.open_album(a.id.clone(), a.title.clone());
        }
    }

    // ---------- mouse ----------

    fn on_mouse(&mut self, me: MouseEvent) {
        if matches!(
            me.kind,
            MouseEventKind::Down(_) | MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            self.detail_retry_hint = None;
        }
        if self.input.is_some() {
            return;
        }
        if self.help_visible {
            match me.kind {
                MouseEventKind::ScrollUp => self.help_scroll = self.help_scroll.saturating_sub(3),
                MouseEventKind::ScrollDown => {
                    self.help_scroll = self.help_scroll.saturating_add(3).min(30)
                }
                MouseEventKind::Down(_) => self.help_visible = false,
                _ => {}
            }
            return;
        }
        match me.kind {
            MouseEventKind::Down(MouseButton::Left) => self.on_click(me.column, me.row),
            MouseEventKind::ScrollUp => self.on_scroll(me.column, me.row, -3),
            MouseEventKind::ScrollDown => self.on_scroll(me.column, me.row, 3),
            _ => {}
        }
    }

    /// True when this click is the second of a double-click on the same cell.
    fn register_click(&mut self, x: u16, y: u16) -> bool {
        let now = Instant::now();
        let double = matches!(
            self.last_click,
            Some((t, lx, ly))
                if lx == x && ly == y && now.duration_since(t).as_millis() <= 400
        );
        self.last_click = if double { None } else { Some((now, x, y)) };
        double
    }

    /// Jump to a top-level destination (nav bar click).
    pub fn goto_view(&mut self, view: MainView) {
        self.focus = Focus::Main;
        match view {
            // The search tab is a no-op without a query; open the editor.
            MainView::Search if self.search.query.is_empty() => {
                self.main_view = MainView::Search;
                self.input = Some((InputMode::Search, String::new()));
            }
            MainView::NowPlaying if self.main_view != MainView::NowPlaying => {
                self.prev_main_view = self.main_view;
                self.main_view = MainView::NowPlaying;
                self.maybe_load_cover();
            }
            v => self.main_view = v,
        }
    }

    fn on_click(&mut self, x: u16, y: u16) {
        let layout = self.ui_layout;
        let pos = Position::new(x, y);
        let double = self.register_click(x, y);

        // Navigation bar takes precedence over everything below it.
        for tab in layout.nav_tabs.iter().flatten() {
            if tab.0.contains(pos) {
                self.goto_view(tab.1);
                return;
            }
        }

        if self.main_view == MainView::Settings {
            for (i, row) in layout.settings_rows.iter().enumerate() {
                if row.is_some_and(|area| area.contains(pos)) {
                    self.settings_selected = i;
                    if double {
                        self.begin_engine_switch();
                    }
                    return;
                }
            }
        }

        // Progress bar → seek to the clicked ratio.
        if let Some(g) = layout.gauge {
            if g.contains(pos) && self.pb.duration > 0.0 {
                let ratio = f64::from(x.saturating_sub(g.x)) / f64::from(g.width.max(1));
                self.player
                    .send(PlayerCmd::SeekAbs(ratio.clamp(0.0, 1.0) * self.pb.duration));
                return;
            }
        }

        // Search category tabs use their rendered widths for hit testing.
        if self.main_view == MainView::Search {
            for (idx, tab) in layout.search_tabs.iter().enumerate() {
                if !tab.is_some_and(|area| area.contains(pos)) {
                    continue;
                }
                if idx != self.search.kind_idx {
                    self.search.kind_idx = idx;
                    self.search.selected = 0;
                    if self.search.list_len() == 0 {
                        self.fire_kind_search();
                    }
                }
                return;
            }
        }

        // Queue rows.
        if let Some((qa, off)) = layout.queue_list {
            if qa.contains(pos) {
                self.focus = Focus::Queue;
                let idx = (y - qa.y) as usize + off;
                if idx < self.queue.len() {
                    self.queue_selected = idx;
                    if double {
                        if let Some(i) = self.queue.jump_to(idx) {
                            self.start_track_at(i);
                        }
                    }
                }
                return;
            }
        }
        if let Some(qp) = layout.queue_pane {
            if qp.contains(pos) {
                self.focus = Focus::Queue;
                return;
            }
        }

        // Main pane lists.
        if let Some((kind, la, off)) = layout.main_list {
            if la.contains(pos) {
                self.focus = Focus::Main;
                let row = (y - la.y) as usize + off;
                use crate::ui::MainListKind;
                match kind {
                    MainListKind::Search => {
                        if row < self.search.list_len() {
                            self.search.selected = row;
                            if double {
                                self.activate_search_selection();
                            }
                        }
                    }
                    MainListKind::Browse => {
                        let rows = self.browse_stack.last().map_or(0, |p| p.row_count());
                        if row < rows {
                            if let Some(page) = self.browse_stack.last_mut() {
                                *page.selected_mut() = row;
                            }
                            if double {
                                self.activate_browse_selection(false);
                            }
                        }
                    }
                    MainListKind::Library => {
                        if self.api.is_logged_in() {
                            if row < LIBRARY_MENU.len() {
                                self.library.selected = row;
                                if double {
                                    self.open_library_item(row);
                                }
                            }
                        } else if row < self.login.methods.len() {
                            self.login.selected = row;
                            if double {
                                self.activate_login_method(row);
                            }
                        }
                    }
                    MainListKind::History => {
                        if row < self.history.entries().len() {
                            self.history_selected = row;
                            if double {
                                self.play_history_selection();
                            }
                        }
                    }
                    MainListKind::Home => {
                        // Physical rows include the two section headers.
                        let t = self.home.tracks.len();
                        let logical = if row == 0 || row == t + 1 {
                            None
                        } else if row <= t {
                            Some(row - 1)
                        } else {
                            Some(row - 2)
                        };
                        if let Some(idx) = logical.filter(|i| *i < self.home.row_count()) {
                            self.home.selected = idx;
                            if double {
                                self.activate_home_selection();
                            }
                        }
                    }
                }
            }
        }
    }

    fn on_scroll(&mut self, x: u16, y: u16, delta: i32) {
        let layout = self.ui_layout;
        let pos = Position::new(x, y);
        let step = |sel: usize, len: usize| -> usize {
            if len == 0 {
                return 0;
            }
            if delta < 0 {
                sel.saturating_sub(delta.unsigned_abs() as usize)
            } else {
                (sel + delta as usize).min(len - 1)
            }
        };
        if let Some((qa, _)) = layout.queue_list {
            if qa.contains(pos) {
                self.queue_selected = step(self.queue_selected, self.queue.len());
                return;
            }
        }
        if let Some((kind, la, _)) = layout.main_list {
            if la.contains(pos) {
                use crate::ui::MainListKind;
                match kind {
                    MainListKind::Search => {
                        self.search.selected = step(self.search.selected, self.search.list_len());
                    }
                    MainListKind::Browse => {
                        if let Some(page) = self.browse_stack.last_mut() {
                            let rows = page.row_count();
                            let sel = page.selected_mut();
                            *sel = step(*sel, rows);
                        }
                    }
                    MainListKind::Library => {
                        if self.api.is_logged_in() {
                            self.library.selected = step(self.library.selected, LIBRARY_MENU.len());
                        } else {
                            self.login.selected =
                                step(self.login.selected, self.login.methods.len());
                        }
                    }
                    MainListKind::History => {
                        self.history_selected =
                            step(self.history_selected, self.history.entries().len());
                    }
                    MainListKind::Home => {
                        self.home.selected = step(self.home.selected, self.home.row_count());
                    }
                }
            }
        } else if self.main_view == MainView::NowPlaying {
            self.lyrics.scroll = if delta < 0 {
                self.lyrics
                    .scroll
                    .saturating_sub(delta.unsigned_abs() as u16)
            } else {
                self.lyrics.scroll.saturating_add(delta as u16)
            };
        }
    }

    fn on_history_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.history_selected = self.history_selected.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.history_selected =
                    (self.history_selected + 1).min(self.history.entries().len().saturating_sub(1));
            }
            KeyCode::Enter => self.play_history_selection(),
            KeyCode::Esc => self.main_view = MainView::Home,
            _ => {}
        }
    }

    fn play_history_selection(&mut self) {
        if let Some(entry) = self.history.entries().get(self.history_selected) {
            self.queue.set_context(vec![entry.track.clone()], 0);
            self.queue_seq = self.queue_seq.wrapping_add(1);
            self.start_track_at(0);
        }
    }

    fn on_library_key(&mut self, key: KeyEvent) {
        if !self.api.is_logged_in() {
            let len = self.login.methods.len();
            if nav_list(&mut self.login.selected, len, key.code) {
                return;
            }
            match key.code {
                KeyCode::Enter => self.activate_login_method(self.login.selected),
                KeyCode::Esc => self.main_view = MainView::Home,
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.library.selected = self.library.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.library.selected = (self.library.selected + 1).min(LIBRARY_MENU.len() - 1);
            }
            KeyCode::Enter => self.open_library_item(self.library.selected),
            KeyCode::Char('x') => {
                if self.login.busy {
                    return;
                }
                self.login.busy = true;
                self.library.loading = true;
                self.auth_seq = self.auth_seq.wrapping_add(1);
                let seq = self.auth_seq;
                let api = self.api.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = api.logout().await.map_err(|e| format!("{e}"));
                    let _ = tx.send(AppEvent::Api(ApiMsg::LogoutDone { seq, result }));
                });
            }
            KeyCode::Esc => self.main_view = MainView::Home,
            _ => {}
        }
    }

    fn activate_login_method(&mut self, index: usize) {
        if self.login.busy {
            return;
        }
        let Some(method) = self.login.methods.get(index).cloned() else {
            return;
        };
        match method {
            LoginMethod::Manual => {
                self.input = Some((InputMode::Login, String::new()));
            }
            LoginMethod::OpenBrowser => {
                match crate::browser_login::open_in_browser("https://music.youtube.com/") {
                    Ok(()) => self.toast("已打开浏览器 - 登录后回来选择「从 .. 导入登录」"),
                    Err(e) => self.toast(format!("打开浏览器失败: {e}")),
                }
            }
            LoginMethod::Browser { display, profile } => {
                self.login.busy = true;
                self.library.loading = true;
                self.auth_seq = self.auth_seq.wrapping_add(1);
                let seq = self.auth_seq;
                self.toast(format!("正在从 {display} 读取登录信息.."));
                let api = self.api.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    // The cookie text stays inside this task: read, handed to
                    // the API, then dropped. Never logged.
                    // Reading is blocking file I/O, so keep it off the async
                    // worker threads.
                    let read = tokio::task::spawn_blocking(move || {
                        crate::browser_cookies::read_cookies(&profile)
                    })
                    .await;
                    let result = match read {
                        Ok(Ok(cookies)) => api.login_cookie(&cookies).await.map_err(|_| {
                            "登录验证失败，请检查浏览器登录状态及网络连接".to_string()
                        }),
                        Ok(Err(e)) => Err(format!("{e:#}")),
                        Err(e) => Err(format!("读取任务失败: {e}")),
                    };
                    let _ = tx.send(AppEvent::Api(ApiMsg::LoginDone { seq, result }));
                });
            }
        }
    }

    fn open_library_item(&mut self, index: usize) {
        if self.library.loading {
            return;
        }
        self.library.loading = true;
        self.library.failed = false;
        self.library_seq = self.library_seq.wrapping_add(1);
        let seq = self.library_seq;
        let api = self.api.clone();
        let tx = self.tx.clone();
        match index {
            0 => {
                tokio::spawn(async move {
                    let result = api.liked_tracks().await.map_err(|e| format!("{e}"));
                    let _ = tx.send(AppEvent::Api(ApiMsg::LibraryTracks {
                        seq,
                        title: "喜欢的音乐".into(),
                        result,
                    }));
                });
            }
            1 => {
                tokio::spawn(async move {
                    let result = api.saved_playlists().await.map_err(|e| format!("{e}"));
                    let _ = tx.send(AppEvent::Api(ApiMsg::LibraryPlaylists { seq, result }));
                });
            }
            2 => {
                tokio::spawn(async move {
                    let result = api.saved_albums().await.map_err(|e| format!("{e}"));
                    let _ = tx.send(AppEvent::Api(ApiMsg::LibraryAlbums { seq, result }));
                });
            }
            3 => {
                tokio::spawn(async move {
                    let result = api.saved_artists().await.map_err(|e| format!("{e}"));
                    let _ = tx.send(AppEvent::Api(ApiMsg::LibraryArtists { seq, result }));
                });
            }
            _ => {
                tokio::spawn(async move {
                    let result = api.history().await.map_err(|e| format!("{e}"));
                    let _ = tx.send(AppEvent::Api(ApiMsg::LibraryTracks {
                        seq,
                        title: "播放历史".into(),
                        result,
                    }));
                });
            }
        }
    }

    fn on_queue_key(&mut self, key: KeyEvent) {
        let len = self.queue.len();
        if nav_list(&mut self.queue_selected, len, key.code) {
            return;
        }
        match key.code {
            KeyCode::Enter => {
                if let Some(i) = self.queue.jump_to(self.queue_selected) {
                    self.start_track_at(i);
                }
            }
            KeyCode::Char('x') => {
                if !self.queue.remove(self.queue_selected) {
                    if Some(self.queue_selected) == self.queue.current_index() {
                        self.toast("不能移除正在播放的曲目");
                    }
                } else if self.queue_selected >= self.queue.len() && self.queue_selected > 0 {
                    self.queue_selected -= 1;
                }
            }
            KeyCode::Char('K') => {
                if let Some(j) = self.queue.swap(self.queue_selected, true) {
                    self.queue_selected = j;
                }
            }
            KeyCode::Char('J') => {
                if let Some(j) = self.queue.swap(self.queue_selected, false) {
                    self.queue_selected = j;
                }
            }
            KeyCode::Esc => self.focus = Focus::Main,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{bail, Result};
    use ratatui::crossterm::event::KeyModifiers;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn settings_reject_missing_mpv_and_ignore_old_player_events() {
        let (mut app, _) = test_app();
        app.on_key(KeyEvent::new(KeyCode::Char(','), KeyModifiers::NONE));
        assert_eq!(app.main_view, MainView::Settings);
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.settings_selected, 1);
        app.engine_switch_pending = true;
        app.finish_engine_switch(PlaybackEngine::Mpv, None, None);
        assert_eq!(app.config.playback.engine, PlaybackEngine::Native);
        assert!(!app.player_restart_requested);

        app.pb.alive = true;
        app.player_epoch = 2;
        assert!(!app.handle(AppEvent::Player {
            epoch: 1,
            event: PlayerEvent::Died
        }));
        assert!(app.pb.alive);
    }

    #[tokio::test]
    async fn switching_engine_saves_choice_and_resumes_current_track() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = test_app();
        app.config_dir = tmp.path().to_path_buf();
        app.queue.set_context(vec![track("current")], 0);
        app.current_video_id = Some("current".into());
        app.pb.time_pos = 42.0;
        app.pb.paused = true;
        app.engine_switch_pending = true;

        app.finish_engine_switch(PlaybackEngine::Mpv, Some("/fake/mpv".into()), None);
        assert_eq!(app.config.playback.engine, PlaybackEngine::Mpv);
        assert_eq!(
            crate::config::load(&crate::config::Dirs {
                config_dir: tmp.path().to_path_buf(),
                data_dir: tmp.path().to_path_buf(),
            })
            .unwrap()
            .playback
            .engine,
            PlaybackEngine::Mpv
        );
        assert!(app.player_restart_requested);
        assert_eq!(app.player_epoch, 1);
        assert!(matches!(commands.try_recv(), Ok(PlayerCmd::Shutdown)));

        app.on_player_event(PlayerEvent::Ready);
        assert_eq!(app.resume_position, Some(42.0));
        app.load_requested = true;
        app.on_track_event(TrackEvent::FileLoaded);
        let sent: Vec<_> = std::iter::from_fn(|| commands.try_recv().ok()).collect();
        assert!(sent
            .iter()
            .any(|cmd| matches!(cmd, PlayerCmd::SeekAbs(t) if *t == 42.0)));
        assert!(sent.iter().any(|cmd| matches!(cmd, PlayerCmd::TogglePause)));
    }

    struct TestApi;

    #[async_trait::async_trait]
    impl MusicApi for TestApi {
        async fn search(&self, _: &str, _: SearchKind) -> Result<SearchResults> {
            std::future::pending().await
        }
        async fn album(&self, _: &str) -> Result<AlbumDetail> {
            std::future::pending().await
        }
        async fn artist(&self, _: &str) -> Result<ArtistDetail> {
            bail!("unused")
        }
        async fn playlist(&self, _: &str) -> Result<PlaylistDetail> {
            bail!("unused")
        }
        async fn radio(&self, _: &str) -> Result<Vec<Track>> {
            std::future::pending().await
        }
        async fn stream_url(&self, _: &str, _: StreamFormat) -> Result<String> {
            std::future::pending().await
        }
        async fn plain_lyrics(&self, _: &str) -> Result<Option<String>> {
            bail!("unused")
        }
        async fn home(&self) -> Result<(Vec<Track>, Vec<AlbumSummary>)> {
            bail!("unused")
        }
        fn is_logged_in(&self) -> bool {
            false
        }
        async fn login_cookie(&self, _: &str) -> Result<()> {
            bail!("unused")
        }
        async fn logout(&self) -> Result<()> {
            bail!("unused")
        }
        async fn liked_tracks(&self) -> Result<Vec<Track>> {
            bail!("unused")
        }
        async fn history(&self) -> Result<Vec<Track>> {
            bail!("unused")
        }
        async fn saved_playlists(&self) -> Result<Vec<PlaylistSummary>> {
            bail!("unused")
        }
        async fn saved_albums(&self) -> Result<Vec<AlbumSummary>> {
            bail!("unused")
        }
        async fn saved_artists(&self) -> Result<Vec<ArtistSummary>> {
            bail!("unused")
        }
    }

    fn track(id: &str) -> Track {
        Track {
            video_id: id.into(),
            title: id.into(),
            artists: "artist".into(),
            album: None,
            duration_secs: Some(120),
            cover_url: None,
        }
    }

    fn test_app() -> (App, mpsc::UnboundedReceiver<PlayerCmd>) {
        let (player, commands) = PlayerHandle::test_handle();
        let (tx, _) = mpsc::unbounded_channel();
        let mut config = Config::default();
        config.playback.radio_auto = false;
        config.lyrics.enabled = false;
        config.sponsorblock.enabled = false;
        let app = App::new(
            config,
            player,
            Arc::new(TestApi),
            reqwest::Client::new(),
            std::env::temp_dir(),
            std::env::temp_dir(),
            None,
            tx,
        );
        (app, commands)
    }

    #[tokio::test]
    async fn idle_tick_only_redraws_on_visible_change() {
        let (mut app, _) = test_app();
        assert!(!app.handle(AppEvent::Tick));

        app.toast("temporary");
        for _ in 0..11 {
            assert!(!app.handle(AppEvent::Tick));
        }
        assert!(app.handle(AppEvent::Tick));
        assert!(app.status.is_none());

        app.pb.loading = true;
        assert!(app.handle(AppEvent::Tick));
        assert_eq!(app.pb.loading_secs, 0.25);
    }

    #[tokio::test]
    async fn download_failure_re_resolves_with_bounded_attempts_and_position() {
        let (mut app, _) = test_app();
        app.current_video_id = Some("song".into());
        app.pb.loading = false;
        app.pb.time_pos = 42.0;
        app.load_requested = true;
        app.on_track_event(TrackEvent::LoadFailed("音频下载失败: HTTP 403".into()));
        assert_eq!(app.stream_attempt, 1);
        assert!(app.pb.loading);
        assert_eq!(app.resume_position, Some(42.0));

        app.load_requested = true;
        app.on_track_event(TrackEvent::LoadFailed("音频下载失败: HTTP 403".into()));
        assert_eq!(app.stream_attempt, 2);
        app.load_requested = true;
        app.on_track_event(TrackEvent::LoadFailed("音频下载失败: HTTP 403".into()));
        assert!(!app.pb.loading);
    }

    #[tokio::test]
    async fn premature_native_end_retries_without_advancing_queue() {
        let (mut app, _) = test_app();
        app.play_context(vec![track("first"), track("second")], 0);
        app.pb.loading = false;
        app.pb.time_pos = 4.0;
        app.pb.duration = 5.0; // A truncated decoder can report a short duration.
        app.on_track_event(TrackEvent::TrackEnded);
        assert_eq!(app.current_video_id.as_deref(), Some("first"));
        assert_eq!(app.stream_attempt, 1);
        assert_eq!(app.resume_position, Some(4.0));
        assert!(app.pb.loading);
    }

    #[tokio::test]
    async fn exhausted_native_retries_keep_track_and_allow_manual_retry() {
        let (mut app, _) = test_app();
        app.play_context(vec![track("first"), track("second")], 0);
        app.pb.loading = false;
        app.pb.time_pos = 4.0;
        app.stream_attempt = 2;
        app.on_track_event(TrackEvent::TrackEnded);
        assert_eq!(app.current_video_id.as_deref(), Some("first"));
        assert!(app.playback_error.is_some());
        assert!(app.pb.paused);

        app.pb.alive = true;
        app.on_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE));
        assert_eq!(app.current_video_id.as_deref(), Some("first"));
        assert_eq!(app.stream_attempt, 0);
        assert!(app.playback_error.is_none());
        assert!(app.pb.loading);
    }

    #[tokio::test]
    async fn exhausted_download_failure_does_not_skip_queue() {
        let (mut app, _) = test_app();
        app.play_context(vec![track("first"), track("second")], 0);
        app.pb.loading = false;
        app.load_requested = true;
        app.stream_attempt = 2;
        app.on_track_event(TrackEvent::LoadFailed("音频下载失败: HTTP 403".into()));
        assert_eq!(app.current_video_id.as_deref(), Some("first"));
        assert!(app.playback_error.is_some());
        assert!(!app.pb.loading);
    }

    #[tokio::test]
    async fn natural_native_end_advances_queue() {
        let (mut app, _) = test_app();
        app.play_context(vec![track("first"), track("second")], 0);
        app.pb.loading = false;
        app.pb.time_pos = 119.0;
        app.on_track_event(TrackEvent::TrackEnded);
        assert_eq!(app.current_video_id.as_deref(), Some("second"));
    }

    #[tokio::test]
    async fn failed_search_keeps_retry_state_for_current_category() {
        let (mut app, _) = test_app();
        app.search.query = "test".into();
        app.main_view = MainView::Search;
        app.on_api_msg(ApiMsg::SearchDone {
            seq: app.search_seq,
            kind: SearchKind::Songs,
            result: Err("offline".into()),
        });
        assert_eq!(app.search.errors[0].as_deref(), Some("offline"));
        app.on_search_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        assert!(app.search.loading[0]);
        assert!(app.search.errors[0].is_none());
    }

    #[test]
    fn conflicting_key_overrides_keep_essential_navigation() {
        let overrides = HashMap::from([
            ("next".to_string(), "j".to_string()),
            ("prev".to_string(), "q".to_string()),
        ]);
        let map = build_keymap(&overrides);
        assert_eq!(map.get(&KeyCode::Char('j')), None);
        assert_eq!(map.get(&KeyCode::Char('q')), Some(&Action::Quit));
        assert_eq!(map.get(&KeyCode::Char('n')), Some(&Action::NextTrack));
        assert_eq!(map.get(&KeyCode::Char('p')), Some(&Action::PrevTrack));
    }

    #[tokio::test]
    async fn old_resolution_cannot_replace_replayed_same_video() {
        let (mut app, mut commands) = test_app();
        app.play_context(vec![track("a")], 0);
        let old_seq = app.play_seq;
        app.play_context(vec![track("b")], 0);
        app.play_context(vec![track("a")], 0);
        app.on_api_msg(ApiMsg::StreamResolved {
            play_seq: old_seq,
            video_id: "a".into(),
            attempt: 0,
            result: Ok("https://example.com/old".into()),
        });
        assert!(!app.load_requested);
        while let Ok(command) = commands.try_recv() {
            assert!(!matches!(command, PlayerCmd::Load { .. }));
        }
    }

    #[tokio::test]
    async fn old_time_updates_do_not_move_new_loading_track() {
        let (mut app, _) = test_app();
        app.play_context(vec![track("a")], 0);
        app.pb.time_pos = 55.0;
        app.play_context(vec![track("b")], 0);
        app.on_player_event(PlayerEvent::Track {
            play_seq: app.play_seq - 1,
            event: TrackEvent::TimePos(56.0),
        });
        app.on_player_event(PlayerEvent::Track {
            play_seq: app.play_seq - 1,
            event: TrackEvent::Duration(999.0),
        });
        assert_eq!(app.pb.time_pos, 0.0);
        assert_eq!(app.pb.duration, 120.0);
    }

    #[tokio::test]
    async fn old_player_load_end_and_failure_events_cannot_change_new_track() {
        let (mut app, _) = test_app();
        app.play_context(vec![track("a"), track("b")], 0);
        let old_seq = app.play_seq;
        app.start_track_at(1);
        app.load_requested = true;
        for event in [
            TrackEvent::FileLoaded,
            TrackEvent::LoadFailed("old failure".into()),
            TrackEvent::TrackEnded,
        ] {
            app.on_player_event(PlayerEvent::Track {
                play_seq: old_seq,
                event,
            });
        }
        assert!(app.load_requested);
        assert!(app.pb.loading);
        assert_eq!(app.current_video_id.as_deref(), Some("b"));
    }

    #[tokio::test]
    async fn changing_tracks_cancels_old_stream_resolution() {
        let (mut app, _) = test_app();
        app.start_track(track("first"));
        let old_task = app.track_tasks.stream.as_ref().unwrap().abort_handle();
        app.start_track(track("second"));
        tokio::task::yield_now().await;
        assert!(old_task.is_finished());
        assert_eq!(app.current_video_id.as_deref(), Some("second"));

        let current_task = app.track_tasks.stream.as_ref().unwrap().abort_handle();
        app.stop_playback_ui();
        tokio::task::yield_now().await;
        assert!(current_task.is_finished());
    }

    #[tokio::test]
    async fn new_search_and_leaving_browse_cancel_old_page_requests() {
        let (mut app, _) = test_app();
        app.submit_search("first".into());
        let old_search = app.search_tasks[0].as_ref().unwrap().abort_handle();
        app.submit_search("second".into());
        tokio::task::yield_now().await;
        assert!(old_search.is_finished());
        assert_eq!(app.search.query, "second");

        app.open_album("album".into(), "Album".into());
        let old_browse = app.browse_task.as_ref().unwrap().abort_handle();
        app.on_browse_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        tokio::task::yield_now().await;
        assert!(old_browse.is_finished());
        assert!(app.browse_task.is_none());
    }

    #[tokio::test]
    async fn browse_returns_to_the_page_that_opened_it() {
        let (mut app, _) = test_app();
        app.search.query = "old search".into();
        app.main_view = MainView::Home;
        app.open_album("album".into(), "Album".into());
        app.on_browse_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.main_view, MainView::Home);

        app.main_view = MainView::Library;
        app.push_browse(BrowsePage::Tracks {
            title: "喜欢的音乐".into(),
            tracks: vec![track("liked")],
            selected: 0,
        });
        assert_eq!(app.browse_stack.len(), 1);
        app.on_browse_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.main_view, MainView::Library);

        app.main_view = MainView::Search;
        app.open_album("old".into(), "Old".into());
        app.main_view = MainView::Library;
        app.push_browse(BrowsePage::Tracks {
            title: "新列表".into(),
            tracks: vec![track("new")],
            selected: 0,
        });
        assert_eq!(app.browse_stack.len(), 1);
        app.on_browse_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.main_view, MainView::Library);
    }

    #[tokio::test]
    async fn failed_detail_keeps_retry_hint_after_toast_expires() {
        let (mut app, _) = test_app();
        app.main_view = MainView::Search;
        app.search.query = "album".into();
        app.open_album("missing".into(), "Missing".into());
        let seq = app.browse_seq;
        app.on_api_msg(ApiMsg::AlbumDone {
            seq,
            result: Err("offline".into()),
        });
        assert_eq!(app.main_view, MainView::Search);
        assert!(app.detail_retry_hint.is_some());
        for _ in 0..12 {
            app.handle(AppEvent::Tick);
        }
        assert!(app.status.is_none());
        assert!(app.detail_retry_hint.is_some());
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(app.detail_retry_hint.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires YTBM_TEST_AAC_FIXTURE, loopback TCP and an audio device"]
    async fn real_player_recovers_from_second_range_403() {
        const RANGE: usize = 1024 * 1024;
        let fixture = std::env::var("YTBM_TEST_AAC_FIXTURE").expect("set YTBM_TEST_AAC_FIXTURE");
        let mut body = std::fs::read(fixture).unwrap();
        let free_size = (RANGE + 1).saturating_sub(body.len()).max(8);
        body.extend_from_slice(&(free_size as u32).to_be_bytes());
        body.extend_from_slice(b"free");
        body.resize(body.len() + free_size - 8, 0);
        assert_eq!(body.len(), RANGE + 1);
        let body = Arc::new(body);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let body = body.clone();
                tokio::spawn(async move {
                    let mut request = [0_u8; 2048];
                    let Ok(size) = socket.read(&mut request).await else {
                        return;
                    };
                    let request = String::from_utf8_lossy(&request[..size]);
                    let bad = request.starts_with("GET /bad ");
                    let start = request
                        .lines()
                        .find_map(|line| {
                            line.split_once(':').and_then(|(name, value)| {
                                name.eq_ignore_ascii_case("range")
                                    .then(|| value.trim().strip_prefix("bytes="))
                                    .flatten()
                            })
                        })
                        .and_then(|range| range.split_once('-'))
                        .and_then(|(start, _)| start.parse::<usize>().ok())
                        .unwrap();
                    if bad && start >= RANGE {
                        let _ = socket.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                        return;
                    }
                    let end = (start + RANGE).min(body.len()) - 1;
                    let bytes = &body[start..=end];
                    let header = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len(), bytes.len()
                    );
                    if socket.write_all(header.as_bytes()).await.is_ok() {
                        let _ = socket.write_all(bytes).await;
                    }
                });
            }
        });

        let (player_tx, mut player_rx) = mpsc::unbounded_channel();
        let player = crate::player::spawn_native_player(0, player_tx);
        let (app_tx, _) = mpsc::unbounded_channel();
        let mut config = Config::default();
        config.playback.engine = PlaybackEngine::Native;
        config.playback.radio_auto = false;
        config.lyrics.enabled = false;
        config.sponsorblock.enabled = false;
        let mut app = App::new(
            config,
            player.clone(),
            Arc::new(TestApi),
            reqwest::Client::new(),
            std::env::temp_dir(),
            std::env::temp_dir(),
            None,
            app_tx,
        );
        let ready = tokio::time::timeout(Duration::from_secs(5), player_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ready, PlayerEvent::Ready), "{ready:?}");
        app.on_player_event(ready);
        app.play_context(vec![track("fixture")], 0);
        app.track_tasks.stream.take().unwrap().abort();
        app.on_api_msg(ApiMsg::StreamResolved {
            play_seq: app.play_seq,
            video_id: "fixture".into(),
            attempt: 0,
            result: Ok(format!("{origin}/bad")),
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = player_rx.recv().await.expect("player channel closed");
                let failed = matches!(
                    &event,
                    PlayerEvent::Track {
                        event: TrackEvent::LoadFailed(error),
                        ..
                    } if error.contains("403")
                );
                app.on_player_event(event);
                if failed {
                    break;
                }
            }
        })
        .await
        .expect("second-range 403 did not reach the app");
        assert_eq!(app.stream_attempt, 1);
        assert!(app.pb.loading);

        app.track_tasks.stream.take().unwrap().abort();
        app.on_api_msg(ApiMsg::StreamResolved {
            play_seq: app.play_seq,
            video_id: "fixture".into(),
            attempt: 1,
            result: Ok(format!("{origin}/good")),
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = player_rx.recv().await.expect("player channel closed");
                let loaded = matches!(
                    event,
                    PlayerEvent::Track {
                        event: TrackEvent::FileLoaded,
                        ..
                    }
                );
                app.on_player_event(event);
                if loaded {
                    break;
                }
            }
        })
        .await
        .expect("good replacement stream did not load");
        assert!(!app.pb.loading);
        assert_eq!(app.stream_attempt, 1);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                app.on_player_event(player_rx.recv().await.expect("player channel closed"));
                if app.pb.time_pos > 0.5 {
                    break;
                }
            }
        })
        .await
        .expect("replacement stream did not keep playing");
        player.send(PlayerCmd::Shutdown);
        server.abort();
    }

    #[tokio::test]
    async fn virtualized_home_click_uses_physical_row_offset() {
        use ratatui::{backend::TestBackend, Terminal};

        let (mut app, _) = test_app();
        app.home.tracks = (0..100).map(|i| track(&format!("h{i}"))).collect();
        app.home.selected = 90;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                let layout = crate::ui::draw(f, &mut app);
                app.ui_layout = layout;
            })
            .unwrap();
        let (_, area, offset) = app.ui_layout.main_list.unwrap();
        assert!(offset > 1);
        app.on_click(area.x, area.y);
        assert_eq!(app.home.selected, offset - 1);
    }

    #[tokio::test]
    async fn virtualized_search_click_uses_row_offset() {
        use ratatui::{backend::TestBackend, Terminal};

        let (mut app, _) = test_app();
        app.main_view = MainView::Search;
        app.search.query = "many".into();
        app.search.results.tracks = (0..1000).map(|i| track(&format!("s{i}"))).collect();
        app.search.selected = 900;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                let layout = crate::ui::draw(f, &mut app);
                app.ui_layout = layout;
            })
            .unwrap();
        let (_, area, offset) = app.ui_layout.main_list.unwrap();
        assert!(offset > 1);
        app.on_click(area.x, area.y + 1);
        assert_eq!(app.search.selected, offset + 1);
    }

    #[tokio::test]
    async fn common_pages_render_at_small_standard_and_wide_sizes() {
        use ratatui::{backend::TestBackend, Terminal};

        let (mut app, _) = test_app();
        app.home.tracks = (0..40).map(|i| track(&format!("h{i}"))).collect();
        app.search.query = "query".into();
        app.search.results.tracks = (0..40).map(|i| track(&format!("s{i}"))).collect();
        for (width, height) in [(40, 12), (80, 24), (120, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            for view in [
                MainView::Home,
                MainView::Search,
                MainView::NowPlaying,
                MainView::Settings,
            ] {
                app.main_view = view;
                app.focus = Focus::Queue;
                terminal
                    .draw(|f| {
                        let layout = crate::ui::draw(f, &mut app);
                        app.ui_layout = layout;
                    })
                    .unwrap();
                if width < 84 || view == MainView::NowPlaying {
                    assert_eq!(app.focus, Focus::Main);
                    assert!(app.ui_layout.queue_pane.is_none());
                }
                if let Some((_, area, _)) = app.ui_layout.main_list {
                    assert!(area.right() <= width && area.bottom() <= height);
                }
            }
        }
    }

    #[tokio::test]
    async fn stale_home_and_auth_results_leave_newer_state_untouched() {
        let (mut app, _) = test_app();
        app.home_seq = 2;
        app.home.loading = true;
        app.home.tracks.push(track("current"));
        app.on_api_msg(ApiMsg::HomeDone {
            seq: 1,
            result: Ok((vec![track("old")], Vec::new())),
        });
        assert!(app.home.loading);
        assert_eq!(app.home.tracks[0].video_id, "current");

        app.auth_seq = 2;
        app.login.busy = true;
        app.library.loading = true;
        app.on_api_msg(ApiMsg::LoginDone {
            seq: 1,
            result: Ok(()),
        });
        app.on_api_msg(ApiMsg::LogoutDone {
            seq: 1,
            result: Ok(()),
        });
        assert!(app.login.busy);
        assert!(app.library.loading);
    }

    #[tokio::test]
    async fn library_failure_keeps_selected_item_and_retry_state() {
        let (mut app, _) = test_app();
        app.main_view = MainView::Library;
        app.library.selected = 2;
        app.library.loading = true;
        app.library_seq = 1;
        app.on_api_msg(ApiMsg::LibraryAlbums {
            seq: 1,
            result: Err("offline".into()),
        });
        assert_eq!(app.main_view, MainView::Library);
        assert_eq!(app.library.selected, 2);
        assert!(!app.library.loading);
        assert!(app.library.failed);

        app.open_library_item(app.library.selected);
        assert!(app.library.loading);
        assert!(!app.library.failed);
    }

    #[tokio::test]
    async fn login_requests_are_serialized() {
        let (mut app, _) = test_app();
        app.submit_login("first".into());
        let seq = app.auth_seq;
        app.submit_login("second".into());
        assert!(app.login.busy);
        assert_eq!(app.auth_seq, seq);
    }

    #[tokio::test]
    async fn old_radio_result_cannot_extend_new_queue() {
        let (mut app, _) = test_app();
        app.play_context(vec![track("a")], 0);
        let old_seq = app.queue_seq;
        app.play_context(vec![track("b")], 0);
        app.on_api_msg(ApiMsg::RadioDone {
            queue_seq: old_seq,
            seed: "a".into(),
            result: Ok(vec![track("unexpected")]),
        });
        assert_eq!(app.queue.len(), 1);
        assert_eq!(app.queue.track_at(0).unwrap().video_id, "b");
    }

    #[tokio::test]
    async fn next_on_last_track_stops_audio() {
        let (mut app, mut commands) = test_app();
        app.play_context(vec![track("a")], 0);
        let advance = app.queue.next_manual();
        app.advance(advance);
        assert!(app.pb.current_title.is_none());
        let stops = std::iter::from_fn(|| commands.try_recv().ok())
            .filter(|cmd| matches!(cmd, PlayerCmd::Stop))
            .count();
        assert_eq!(stops, 2);
    }

    #[tokio::test]
    async fn restored_position_is_applied_on_first_play() {
        let dir = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = test_app();
        app.data_dir = dir.path().to_path_buf();
        let saved = crate::session::SavedSession {
            tracks: vec![track("resume")],
            current: Some(0),
            position: 42.0,
            repeat: crate::player::queue::RepeatMode::Off,
            radio_on: false,
        };
        saved.save(&app.session_path()).unwrap();
        app.restore_session();
        app.start_track_at(0);
        assert_eq!(app.resume_position, Some(42.0));
        app.load_requested = true;
        app.on_player_event(PlayerEvent::Track {
            play_seq: app.play_seq,
            event: TrackEvent::FileLoaded,
        });
        let commands: Vec<_> = std::iter::from_fn(|| commands.try_recv().ok()).collect();
        assert!(commands
            .iter()
            .any(|cmd| matches!(cmd, PlayerCmd::SeekAbs(p) if *p == 42.0)));
    }

    #[tokio::test]
    async fn player_restart_reloads_current_track_at_previous_position() {
        let (mut app, _) = test_app();
        app.play_context(vec![track("a")], 0);
        app.pb.alive = false;
        app.pb.time_pos = 52.0;
        app.restart_pending_track = true;
        let old_seq = app.play_seq;
        app.on_player_event(PlayerEvent::Ready);
        assert!(app.pb.alive);
        assert!(app.play_seq > old_seq);
        assert_eq!(app.resume_position, Some(52.0));
    }

    #[tokio::test]
    async fn player_init_failure_clears_loading_indicator() {
        let (mut app, _) = test_app();
        app.play_context(vec![track("a")], 0);
        app.on_player_event(PlayerEvent::InitFailed("no device".into()));
        assert!(!app.pb.alive);
        assert!(!app.pb.loading);
        assert!(!app.load_requested);
    }

    #[tokio::test]
    async fn leaving_loading_album_keeps_parent_on_late_error() {
        let (mut app, _) = test_app();
        app.browse_stack.push(BrowsePage::Albums {
            title: "parent".into(),
            items: Vec::new(),
            selected: 0,
        });
        app.main_view = MainView::Browse;
        app.browse_root_view = MainView::Library;
        app.open_album("album-a".into(), "A".into());
        let old_seq = app.browse_seq;
        app.on_browse_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.on_api_msg(ApiMsg::AlbumDone {
            seq: old_seq,
            result: Err("late error".into()),
        });
        assert_eq!(app.browse_stack.len(), 1);
        assert!(matches!(app.browse_stack[0], BrowsePage::Albums { .. }));
    }
}
