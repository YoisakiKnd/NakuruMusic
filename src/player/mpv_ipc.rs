//! mpv subprocess + JSON IPC transport.
//!
//! Protocol: newline-delimited JSON over a named pipe (Windows) or unix
//! socket. Requests carry `request_id` and are matched to responses; async
//! `event` messages are translated into [`PlayerEvent`]s.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, warn};

use super::{PlayerEvent, TrackEvent};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;
type TrackMap = Arc<StdMutex<MpvTrackMap>>;

#[derive(Default)]
struct MpvTrackMap {
    by_entry: HashMap<i64, u64>,
    recent_entries: VecDeque<i64>,
    active_entry: Option<i64>,
    deferred: VecDeque<(i64, TrackEvent)>,
}

impl MpvTrackMap {
    fn assign(&mut self, entry: i64, play_seq: u64) -> Vec<PlayerEvent> {
        if self.by_entry.insert(entry, play_seq).is_none() {
            self.recent_entries.push_back(entry);
            if self.recent_entries.len() > 32 {
                if let Some(old) = self.recent_entries.pop_front() {
                    self.by_entry.remove(&old);
                }
            }
        }
        let mut ready = Vec::new();
        self.deferred.retain(|(id, event)| {
            if *id == entry {
                ready.push(PlayerEvent::Track {
                    play_seq,
                    event: event.clone(),
                });
                false
            } else {
                true
            }
        });
        ready
    }

    fn track_event(&mut self, entry: i64, event: TrackEvent) -> Option<PlayerEvent> {
        if let Some(&play_seq) = self.by_entry.get(&entry) {
            Some(PlayerEvent::Track { play_seq, event })
        } else {
            if self.deferred.len() == 128 {
                self.deferred.pop_front();
            }
            self.deferred.push_back((entry, event));
            None
        }
    }

    fn handle_mpv_event(&mut self, msg: &Value) -> Option<PlayerEvent> {
        let event = msg.get("event")?.as_str()?;
        match event {
            "start-file" => {
                self.active_entry = msg.get("playlist_entry_id").and_then(Value::as_i64);
                None
            }
            "property-change" => {
                let name = msg.get("name").and_then(Value::as_str)?;
                let data = msg.get("data")?;
                match name {
                    "volume" => data.as_f64().map(|f| PlayerEvent::Volume(f.round() as i64)),
                    "mute" => data.as_bool().map(PlayerEvent::Muted),
                    "time-pos" => self.active_entry.and_then(|id| {
                        data.as_f64()
                            .and_then(|v| self.track_event(id, TrackEvent::TimePos(v)))
                    }),
                    "duration" => self.active_entry.and_then(|id| {
                        data.as_f64()
                            .and_then(|v| self.track_event(id, TrackEvent::Duration(v)))
                    }),
                    "pause" => self.active_entry.and_then(|id| {
                        data.as_bool()
                            .and_then(|v| self.track_event(id, TrackEvent::Paused(v)))
                    }),
                    _ => None,
                }
            }
            "file-loaded" => self
                .active_entry
                .and_then(|id| self.track_event(id, TrackEvent::FileLoaded)),
            "end-file" => {
                let id = msg.get("playlist_entry_id").and_then(Value::as_i64)?;
                if self.active_entry == Some(id) {
                    self.active_entry = None;
                }
                match msg.get("reason").and_then(Value::as_str).unwrap_or("") {
                    "eof" => self.track_event(id, TrackEvent::TrackEnded),
                    "error" => self.track_event(
                        id,
                        TrackEvent::LoadFailed(
                            msg.get("file_error")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown error")
                                .to_string(),
                        ),
                    ),
                    _ => None,
                }
            }
            other => {
                debug!("mpv event ignored: {other}");
                None
            }
        }
    }
}

pub struct Mpv {
    child: Child,
    line_tx: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: AtomicU64,
    shutting_down: Arc<AtomicBool>,
    track_map: TrackMap,
    ev_tx: mpsc::UnboundedSender<PlayerEvent>,
}

impl Mpv {
    /// Spawn mpv, connect to its IPC endpoint and start reader/writer tasks.
    /// mpv resolves YouTube URLs itself through its built-in yt-dlp hook.
    pub async fn spawn(
        mpv_path: &str,
        ytdlp_path: Option<&str>,
        initial_volume: i64,
        ev_tx: mpsc::UnboundedSender<PlayerEvent>,
    ) -> Result<Self> {
        Self::spawn_inner(mpv_path, ytdlp_path, initial_volume, None, ev_tx).await
    }

    #[cfg(test)]
    pub async fn spawn_with_audio_device(
        mpv_path: &str,
        initial_volume: i64,
        audio_device: &str,
        ev_tx: mpsc::UnboundedSender<PlayerEvent>,
    ) -> Result<Self> {
        Self::spawn_inner(mpv_path, None, initial_volume, Some(audio_device), ev_tx).await
    }

    async fn spawn_inner(
        mpv_path: &str,
        ytdlp_path: Option<&str>,
        initial_volume: i64,
        audio_device: Option<&str>,
        ev_tx: mpsc::UnboundedSender<PlayerEvent>,
    ) -> Result<Self> {
        let ipc_addr = ipc_addr();

        let mut cmd = Command::new(mpv_path);
        cmd.args([
            "--no-video",
            "--idle=yes",
            "--no-terminal",
            "--really-quiet",
            "--no-config",
            "--keep-open=no",
            "--ytdl-format=bestaudio/best",
        ])
        .arg(format!("--volume={initial_volume}"))
        .arg(format!("--input-ipc-server={ipc_addr}"));
        if let Some(audio_device) = audio_device {
            cmd.arg(format!("--audio-device={audio_device}"));
        }
        // Point mpv's ytdl hook at the resolved yt-dlp binary so playback
        // does not depend on the PATH of whatever terminal launched us.
        if let Some(ytdlp) = ytdlp_path {
            cmd.arg(format!("--script-opts=ytdl_hook-ytdl_path={ytdlp}"));
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let child = cmd
            .spawn()
            .with_context(|| format!("无法启动 mpv（{mpv_path}）"))?;

        let (reader, writer) = connect_with_retry(&ipc_addr).await?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let shutting_down = Arc::new(AtomicBool::new(false));
        let track_map: TrackMap = Arc::new(StdMutex::new(MpvTrackMap::default()));

        // Writer task: serialize one JSON line per command.
        let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let mut writer = writer;
            while let Some(line) = line_rx.recv().await {
                if writer.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if writer.write_all(b"\n").await.is_err() {
                    break;
                }
                let _ = writer.flush().await;
            }
        });

        // Reader task: route responses by request_id, translate events.
        {
            let pending = pending.clone();
            let shutting_down = shutting_down.clone();
            let track_map = track_map.clone();
            let ev_tx = ev_tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(reader).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => handle_line(&line, &pending, &track_map, &ev_tx).await,
                        Ok(None) => break,
                        Err(e) => {
                            warn!("mpv ipc read error: {e}");
                            break;
                        }
                    }
                }
                // Connection gone: fail all in-flight commands.
                pending.lock().await.clear();
                if !shutting_down.load(Ordering::SeqCst) {
                    let _ = ev_tx.send(PlayerEvent::Died);
                }
            });
        }

        Ok(Self {
            child,
            line_tx,
            pending,
            next_id: AtomicU64::new(1),
            shutting_down,
            track_map,
            ev_tx,
        })
    }

    pub async fn load(&self, url: &str, play_seq: u64) -> Result<Value> {
        let result = self.command(json!(["loadfile", url, "replace"])).await?;
        // loadfile only changes the playlist; loading and events happen later.
        // The playlist entry ID ties even early start-file events to this
        // request. The reader buffers events until the ID has been registered.
        let playlist = self.command(json!(["get_property", "playlist"])).await?;
        let entry = playlist
            .as_array()
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| item.get("filename").and_then(Value::as_str) == Some(url))
                    .or_else(|| (items.len() == 1).then(|| &items[0]))
            })
            .and_then(|item| item.get("id").and_then(Value::as_i64))
            .ok_or_else(|| anyhow!("无法识别 mpv 播放列表条目"))?;
        let events = self.track_map.lock().unwrap().assign(entry, play_seq);
        for event in events {
            let _ = self.ev_tx.send(event);
        }
        Ok(result)
    }

    /// Send a command and await mpv's reply. Returns the `data` field.
    pub async fn command(&self, args: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let msg = json!({ "command": args, "request_id": id }).to_string();
        if self.line_tx.send(msg).is_err() {
            self.pending.lock().await.remove(&id);
            bail!("mpv 连接已断开");
        }

        let reply = match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                bail!("mpv 连接已断开");
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!("mpv 响应超时");
            }
        };

        match reply.get("error").and_then(Value::as_str) {
            Some("success") => Ok(reply.get("data").cloned().unwrap_or(Value::Null)),
            Some(err) => bail!("mpv 命令失败: {err}"),
            None => Ok(Value::Null),
        }
    }

    pub async fn observe_default_properties(&self) -> Result<()> {
        for (i, prop) in ["time-pos", "duration", "pause", "volume", "mute"]
            .iter()
            .enumerate()
        {
            self.command(json!(["observe_property", i as u64 + 1, prop]))
                .await?;
        }
        Ok(())
    }

    /// Graceful quit; falls back to kill after a short grace period.
    pub async fn shutdown(&mut self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let _ = self.command(json!(["quit"])).await;
        let waited =
            tokio::time::timeout(std::time::Duration::from_secs(2), self.child.wait()).await;
        if waited.is_err() {
            let _ = self.child.kill().await;
        }
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    #[ignore = "requires YTBM_TEST_MPV and YTBM_TEST_AAC_FIXTURE"]
    async fn real_mpv_events_keep_track_identity_after_reload() {
        let mpv_path = std::env::var("YTBM_TEST_MPV").expect("set YTBM_TEST_MPV");
        let fixture = std::env::var("YTBM_TEST_AAC_FIXTURE").expect("set YTBM_TEST_AAC_FIXTURE");
        let fixture = std::fs::canonicalize(fixture).unwrap();
        let url = format!("file://{}", fixture.display());
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let mut mpv = Mpv::spawn(&mpv_path, None, 0, ev_tx).await.unwrap();
        mpv.observe_default_properties().await.unwrap();

        for play_seq in [1, 2] {
            mpv.load(&url, play_seq).await.unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match ev_rx.recv().await.expect("mpv event channel closed") {
                        PlayerEvent::Track {
                            play_seq: id,
                            event: TrackEvent::FileLoaded,
                        } if id == play_seq => break,
                        PlayerEvent::Track {
                            play_seq: id,
                            event: TrackEvent::LoadFailed(error),
                        } if id == play_seq => panic!("mpv failed to load: {error}"),
                        _ => {}
                    }
                }
            })
            .await
            .expect("mpv did not load the current track");
        }

        mpv.command(json!(["seek", 9.0, "absolute"])).await.unwrap();
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if matches!(
                    ev_rx.recv().await,
                    Some(PlayerEvent::Track {
                        play_seq: 2,
                        event: TrackEvent::TrackEnded
                    })
                ) {
                    break;
                }
            }
        })
        .await
        .expect("mpv did not attribute EOF to the second load");
        mpv.shutdown().await;
    }
}

async fn handle_line(
    line: &str,
    pending: &Pending,
    track_map: &TrackMap,
    ev_tx: &mpsc::UnboundedSender<PlayerEvent>,
) {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            // mpv may echo signed stream URLs in IPC data.
            warn!("bad ipc json: {e}");
            return;
        }
    };

    if let Some(id) = msg.get("request_id").and_then(Value::as_u64) {
        if let Some(tx) = pending.lock().await.remove(&id) {
            let _ = tx.send(msg);
        }
        return;
    }

    let ev = track_map.lock().unwrap().handle_mpv_event(&msg);
    if let Some(ev) = ev {
        let _ = ev_tx.send(ev);
    }
}

fn ipc_addr() -> String {
    let pid = std::process::id();
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\nakuru-mpv-{pid}")
    }
    #[cfg(not(windows))]
    {
        std::env::temp_dir()
            .join(format!("nakuru-mpv-{pid}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

type IpcReader = Box<dyn AsyncRead + Send + Unpin>;
type IpcWriter = Box<dyn AsyncWrite + Send + Unpin>;

/// mpv needs a moment to create the IPC endpoint after spawning.
async fn connect_with_retry(addr: &str) -> Result<(IpcReader, IpcWriter)> {
    for _ in 0..40 {
        match try_connect(addr).await {
            Ok(pair) => return Ok(pair),
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
        }
    }
    bail!("连接 mpv IPC 超时（{addr}）");
}

#[cfg(windows)]
async fn try_connect(addr: &str) -> Result<(IpcReader, IpcWriter)> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let client = ClientOptions::new().open(addr)?;
    let (r, w) = tokio::io::split(client);
    Ok((Box::new(r), Box::new(w)))
}

#[cfg(not(windows))]
async fn try_connect(addr: &str) -> Result<(IpcReader, IpcWriter)> {
    let stream = tokio::net::UnixStream::connect(addr).await?;
    let (r, w) = tokio::io::split(stream);
    Ok((Box::new(r), Box::new(w)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn early_and_late_mpv_events_keep_their_playlist_entry() {
        let mut map = MpvTrackMap::default();
        assert!(map
            .handle_mpv_event(&json!({"event":"start-file","playlist_entry_id":10}))
            .is_none());
        assert!(map
            .handle_mpv_event(&json!({"event":"file-loaded"}))
            .is_none());
        assert!(matches!(
            map.assign(10, 1).as_slice(),
            [PlayerEvent::Track {
                play_seq: 1,
                event: TrackEvent::FileLoaded
            }]
        ));

        map.handle_mpv_event(&json!({"event":"start-file","playlist_entry_id":11}));
        let old_end = map.handle_mpv_event(&json!({
            "event":"end-file", "playlist_entry_id":10, "reason":"eof"
        }));
        assert!(matches!(
            old_end,
            Some(PlayerEvent::Track {
                play_seq: 1,
                event: TrackEvent::TrackEnded
            })
        ));
        assert_eq!(map.active_entry, Some(11));
        assert!(map
            .handle_mpv_event(&json!({"event":"file-loaded"}))
            .is_none());
        assert!(matches!(
            map.assign(11, 2).as_slice(),
            [PlayerEvent::Track {
                play_seq: 2,
                event: TrackEvent::FileLoaded
            }]
        ));
        assert!(matches!(
            map.handle_mpv_event(
                &json!({"event":"property-change", "name":"time-pos", "data":12.5})
            ),
            Some(PlayerEvent::Track {
                play_seq: 2,
                event: TrackEvent::TimePos(12.5)
            })
        ));
        assert!(map
            .handle_mpv_event(&json!({
                "event":"end-file", "playlist_entry_id":11, "reason":"stop"
            }))
            .is_none());
        assert_eq!(map.active_entry, None);
    }
}
