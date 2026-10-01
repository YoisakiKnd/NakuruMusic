mod api;
mod app;
mod atomic_file;
mod browser_cookies;
mod browser_login;
mod config;
mod history;
mod lyrics;
mod player;
mod session;
mod smoke_tests;
mod sponsorblock;
mod ui;

use std::sync::Arc;

use anyhow::{bail, Result};
use app::{App, AppEvent};
use ratatui::crossterm::event::{Event, KeyEventKind};
use tokio::sync::mpsc;
use tracing::info;

fn main() -> Result<()> {
    let dirs = config::project_dirs()?;

    // File logging - the TUI owns the terminal, so nothing may print to it.
    std::fs::create_dir_all(&dirs.data_dir)?;
    let (log_writer, _log_guard) = tracing_appender::non_blocking(
        tracing_appender::rolling::never(&dirs.data_dir, "ytbm-tui.log"),
    );
    tracing_subscriber::fmt()
        .with_writer(log_writer)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    browser_cookies::cleanup_stale_scratch();

    let mut cfg = config::load(&dirs)?;
    let startup_warning = preflight(&mut cfg)?;
    info!("preflight ok, starting runtime");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // Ask the terminal which graphics protocol it speaks before taking over
    // the screen. Failure just means no images - half-blocks still work.
    let picker = match ratatui_image::picker::Picker::from_query_stdio() {
        Ok(p) => {
            info!("image protocol: {:?}", p.protocol_type());
            Some(p)
        }
        Err(e) => {
            // Half-blocks need no terminal support at all, so images still
            // show up - just at half the vertical resolution.
            info!("image protocol query failed ({e}), falling back to halfblocks");
            Some(ratatui_image::picker::Picker::halfblocks())
        }
    };

    // ratatui::init installs a panic hook that restores the terminal.
    let terminal = ratatui::init();
    // Bracketed paste lets multi-line cookie pastes arrive as one event
    // instead of a key stream (Enter inside would submit prematurely).
    // Mouse capture enables click/scroll (text selection needs Shift+drag).
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::EnableBracketedPaste,
        ratatui::crossterm::event::EnableMouseCapture
    );
    let result = rt.block_on(run(terminal, cfg, picker, startup_warning));
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::DisableBracketedPaste,
        ratatui::crossterm::event::DisableMouseCapture
    );
    ratatui::restore();
    result
}

/// mpv is required only for the mpv backend. yt-dlp is optional: stream URLs
/// are resolved in-process, and it can serve as mpv's fallback resolver.
/// Both tools are located beyond PATH (scoop/winget dirs) so a terminal
/// opened before installation still works.
fn preflight(cfg: &mut config::Config) -> Result<Option<String>> {
    if cfg.playback.engine == config::PlaybackEngine::Mpv {
        match config::resolve_tool(&cfg.playback.mpv_path, "mpv", "mpv") {
            Some(path) => {
                info!("mpv resolved: {path}");
                cfg.playback.mpv_path = path;
            }
            None => bail!(
                "未检测到 mpv（已尝试 PATH、scoop、Program Files）。\n\n\
                 请安装 mpv，或在配置文件 [playback] 中设置 engine = \"native\" 使用内置播放器。"
            ),
        }
        // Only mpv can use this fallback. Native playback never probes or
        // starts either external executable.
        match config::resolve_tool("yt-dlp", "yt-dlp", "yt-dlp") {
            Some(path) => {
                info!("yt-dlp resolved: {path}");
                cfg.ytdlp_path = Some(path);
            }
            None => info!("yt-dlp not found (optional mpv URL fallback unavailable)"),
        }
    }
    Ok(None)
}

async fn run(
    mut terminal: ratatui::DefaultTerminal,
    cfg: config::Config,
    picker: Option<ratatui_image::picker::Picker>,
    startup_warning: Option<String>,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();

    spawn_input_thread(tx.clone());
    spawn_tick_task(tx.clone());
    let player = wire_player(&cfg, tx.clone());

    let dirs = config::project_dirs()?;
    let api: Arc<dyn api::MusicApi> = Arc::new(api::rustypipe::RustyPipeApi::new(
        dirs.data_dir.join("rustypipe"),
    )?);
    let http = reqwest::Client::builder()
        .user_agent(concat!("ytbm-tui/", env!("CARGO_PKG_VERSION")))
        .build()?;

    let app = App::new(
        cfg,
        player,
        api,
        http,
        dirs.data_dir.clone(),
        picker,
        tx.clone(),
    );
    let mut app = app;
    app.restore_session();
    app.load_history();
    app.on_start();
    match startup_warning {
        Some(w) => app.toast(w),
        None => app.toast("欢迎使用 ytbm-tui - / 搜索 - L 音乐库 - ? 帮助"),
    }

    let mut redraw = true;
    while !app.should_quit {
        if redraw {
            let mut layout = ui::UiLayout::default();
            terminal.draw(|f| layout = ui::draw(f, &mut app))?;
            app.ui_layout = layout;
        }
        let Some(ev) = rx.recv().await else { break };
        redraw = app.handle(ev);
        // Coalesce whatever else is already queued before redrawing.
        while let Ok(ev) = rx.try_recv() {
            redraw |= app.handle(ev);
        }
        if app.player_restart_requested {
            app.player_restart_requested = false;
            app.player = wire_player(&app.config, tx.clone());
            app.toast("正在重启播放器..");
            redraw = true;
        }
    }

    // Graceful player teardown; mpv's kill_on_drop is its safety net.
    app.player.send(player::PlayerCmd::Shutdown);
    app.wait_persistence().await;
    app.save_session();
    app.save_history();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    info!("clean shutdown");
    Ok(())
}

/// Spawn the player task and bridge its events into the app channel.
fn wire_player(cfg: &config::Config, tx: mpsc::UnboundedSender<AppEvent>) -> player::PlayerHandle {
    let (pev_tx, mut pev_rx) = mpsc::unbounded_channel::<player::PlayerEvent>();
    let handle = match cfg.playback.engine {
        config::PlaybackEngine::Mpv => player::spawn_player(
            cfg.playback.mpv_path.clone(),
            cfg.ytdlp_path.clone(),
            cfg.playback.volume,
            pev_tx,
        ),
        config::PlaybackEngine::Native => player::spawn_native_player(cfg.playback.volume, pev_tx),
    };
    tokio::spawn(async move {
        while let Some(pe) = pev_rx.recv().await {
            if tx.send(AppEvent::Player(pe)).is_err() {
                break;
            }
        }
    });
    handle
}

/// Blocking stdin reader on a plain OS thread; crossterm's async event
/// stream needs an extra feature + executor coupling we don't need.
fn spawn_input_thread(tx: mpsc::UnboundedSender<AppEvent>) {
    std::thread::spawn(move || loop {
        match ratatui::crossterm::event::read() {
            // Windows delivers Press AND Release events - only forward Press.
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                if tx.send(AppEvent::Key(k)).is_err() {
                    break;
                }
            }
            Ok(Event::Paste(data)) => {
                if tx.send(AppEvent::Paste(data)).is_err() {
                    break;
                }
            }
            // Forward only the mouse events we act on (Move/Drag are noise).
            Ok(Event::Mouse(m))
                if matches!(
                    m.kind,
                    ratatui::crossterm::event::MouseEventKind::Down(_)
                        | ratatui::crossterm::event::MouseEventKind::ScrollUp
                        | ratatui::crossterm::event::MouseEventKind::ScrollDown
                ) =>
            {
                if tx.send(AppEvent::Mouse(m)).is_err() {
                    break;
                }
            }
            Ok(Event::Resize(_, _)) => {
                if tx.send(AppEvent::Resize).is_err() {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    });
}

fn spawn_tick_task(tx: mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            iv.tick().await;
            if tx.send(AppEvent::Tick).is_err() {
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_preflight_needs_no_external_player() {
        let mut cfg = config::Config::default();
        cfg.playback.engine = config::PlaybackEngine::Native;
        cfg.playback.mpv_path = "definitely-not-installed-mpv".into();
        assert!(preflight(&mut cfg).is_ok());
        assert!(cfg.ytdlp_path.is_none());
    }
}
