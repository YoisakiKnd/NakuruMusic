//! Player facade: the app talks to a [`PlayerHandle`] with [`PlayerCmd`]s
//! and receives [`PlayerEvent`]s; the mpv process/IPC lives in `mpv_ipc`.

mod mpv_ipc;
mod native;
mod progressive;
pub mod queue;

use serde_json::json;
use tokio::sync::mpsc;
use tracing::{error, info};

#[derive(Debug, Clone)]
pub enum PlayerCmd {
    /// Load and play a media URL.
    ///
    /// Normally a direct audio-stream URL resolved by the API layer. A
    /// `watch?v=` URL also works and makes mpv fall back to its yt-dlp hook,
    /// which is what happens when our own resolution fails.
    Load {
        url: String,
        play_seq: u64,
        /// On a recovery attempt, decode only after the entire temporary file
        /// is present so a streaming demuxer cannot mistake a gap for EOF.
        wait_for_complete: bool,
    },
    Stop,
    TogglePause,
    ToggleMute,
    SeekRel(f64),
    SeekAbs(f64),
    AddVolume(i64),
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum PlayerEvent {
    Ready,
    InitFailed(String),
    Track {
        play_seq: u64,
        event: TrackEvent,
    },
    Volume(i64),
    Muted(bool),
    /// mpv process/IPC gone unexpectedly.
    Died,
}

#[derive(Debug, Clone)]
pub enum TrackEvent {
    FileLoaded,
    TimePos(f64),
    Duration(f64),
    Paused(bool),
    /// end-file reason=eof - natural track end, advance the queue.
    TrackEnded,
    /// end-file reason=error - extraction/decode failed for this track.
    LoadFailed(String),
}

#[derive(Clone)]
pub struct PlayerHandle {
    cmd_tx: mpsc::UnboundedSender<PlayerCmd>,
}

impl PlayerHandle {
    pub fn send(&self, cmd: PlayerCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn send_checked(&self, cmd: PlayerCmd) -> bool {
        self.cmd_tx.send(cmd).is_ok()
    }

    #[cfg(test)]
    pub fn test_handle() -> (Self, mpsc::UnboundedReceiver<PlayerCmd>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        (Self { cmd_tx }, cmd_rx)
    }
}

pub fn spawn_player(
    mpv_path: String,
    ytdlp_path: Option<String>,
    initial_volume: i64,
    ev_tx: mpsc::UnboundedSender<PlayerEvent>,
) -> PlayerHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    tokio::spawn(player_task(
        mpv_path,
        ytdlp_path,
        initial_volume,
        cmd_rx,
        ev_tx,
    ));
    PlayerHandle { cmd_tx }
}

/// In-process AAC/MP4 playback. Audio is streamed through an anonymous
/// temporary file with a short prebuffer; the file disappears on close.
pub fn spawn_native_player(
    initial_volume: i64,
    ev_tx: mpsc::UnboundedSender<PlayerEvent>,
) -> PlayerHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    tokio::spawn(native::run(initial_volume, cmd_rx, ev_tx));
    PlayerHandle { cmd_tx }
}

async fn player_task(
    mpv_path: String,
    ytdlp_path: Option<String>,
    initial_volume: i64,
    mut cmd_rx: mpsc::UnboundedReceiver<PlayerCmd>,
    ev_tx: mpsc::UnboundedSender<PlayerEvent>,
) {
    let mut mpv = match mpv_ipc::Mpv::spawn(
        &mpv_path,
        ytdlp_path.as_deref(),
        initial_volume,
        ev_tx.clone(),
    )
    .await
    {
        Ok(m) => m,
        Err(e) => {
            error!("player init failed: {e:#}");
            let _ = ev_tx.send(PlayerEvent::InitFailed(format!("{e:#}")));
            return;
        }
    };
    if let Err(e) = mpv.observe_default_properties().await {
        error!("observe_property failed: {e:#}");
        let _ = ev_tx.send(PlayerEvent::InitFailed(format!("{e:#}")));
        return;
    }
    info!("mpv ready");
    let _ = ev_tx.send(PlayerEvent::Ready);

    while let Some(cmd) = cmd_rx.recv().await {
        let result = match cmd {
            PlayerCmd::Load { url, play_seq, .. } => {
                // Stream URLs carry credentials in the query string - log the
                // origin only, never the full URL.
                info!(
                    "loadfile {}",
                    url.split_once('?').map_or(url.as_str(), |(base, _)| base)
                );
                let result = mpv.load(&url, play_seq).await;
                if let Err(e) = &result {
                    let _ = ev_tx.send(PlayerEvent::Track {
                        play_seq,
                        event: TrackEvent::LoadFailed(format!("mpv 加载命令失败: {e}")),
                    });
                }
                result
            }
            PlayerCmd::Stop => mpv.command(json!(["stop"])).await,
            PlayerCmd::TogglePause => mpv.command(json!(["cycle", "pause"])).await,
            PlayerCmd::ToggleMute => mpv.command(json!(["cycle", "mute"])).await,
            PlayerCmd::SeekRel(secs) => mpv.command(json!(["seek", secs, "relative"])).await,
            PlayerCmd::SeekAbs(secs) => mpv.command(json!(["seek", secs, "absolute"])).await,
            PlayerCmd::AddVolume(delta) => mpv.command(json!(["add", "volume", delta])).await,
            PlayerCmd::Shutdown => {
                mpv.shutdown().await;
                break;
            }
        };
        if let Err(e) = result {
            // Seek before anything is loaded etc. - not fatal, just log.
            tracing::warn!("player command failed: {e:#}");
        }
    }
    info!("player task ended");
}
