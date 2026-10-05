//! In-process MP4/AAC playback. Compressed bytes are staged in an anonymous
//! temporary file; decoding starts after a small prebuffer. The reader waits
//! for missing bytes, so network stalls cannot grow RAM. Bounded HTTP ranges
//! are fetched sequentially until a seek requests a missing distant range;
//! that request interrupts the current segment and takes priority.

use std::io::{BufReader, SeekFrom};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use rodio::{DeviceSinkBuilder, Player, Source};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc;

use super::progressive::{ProgressiveFile, ProgressiveReader, WriterGuard};
use super::{PlayerCmd, PlayerEvent, TrackEvent};

const MAX_AUDIO_BYTES: u64 = 512 * 1024 * 1024;
const PREBUFFER_BYTES: u64 = 1024 * 1024;
const RANGE_BYTES: u64 = 1024 * 1024;
const NETWORK_STALL_TIMEOUT: Duration = Duration::from_secs(30);

enum DownloadEvent {
    Ready(u64, ProgressiveReader),
    Finished(u64, Result<(), String>),
    Decoded(
        u64,
        Result<
            (
                rodio::Decoder<BufReader<ProgressiveReader>>,
                Option<Duration>,
            ),
            String,
        >,
    ),
    Seeked(u64, u64, Result<f64, String>),
}

pub(super) async fn run(
    initial_volume: i64,
    cmd_rx: mpsc::UnboundedReceiver<PlayerCmd>,
    ev_tx: mpsc::UnboundedSender<PlayerEvent>,
) {
    run_on_device(initial_volume, None, cmd_rx, ev_tx).await;
}

async fn run_on_device(
    initial_volume: i64,
    output_device: Option<rodio::cpal::Device>,
    mut cmd_rx: mpsc::UnboundedReceiver<PlayerCmd>,
    ev_tx: mpsc::UnboundedSender<PlayerEvent>,
) {
    let opened = match output_device {
        Some(device) => {
            DeviceSinkBuilder::from_device(device).and_then(|builder| builder.open_stream())
        }
        None => DeviceSinkBuilder::open_default_sink(),
    };
    let mut device = match opened {
        Ok(device) => device,
        Err(e) => {
            let _ = ev_tx.send(PlayerEvent::InitFailed(format!("无法打开音频设备: {e}")));
            return;
        }
    };
    // The TUI owns stdout/stderr; rodio's default drop notice would appear
    // after terminal restoration on every normal exit.
    device.log_on_drop(false);
    let sink = Arc::new(Player::connect_new(device.mixer()));
    let mut volume = initial_volume.clamp(0, 100);
    let mut muted = false;
    sink.set_volume(volume as f32 / 100.0);

    let http = match reqwest::Client::builder()
        .user_agent(concat!("nakuru-music/", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(http) => http,
        Err(e) => {
            let _ = ev_tx.send(PlayerEvent::InitFailed(format!(
                "播放器网络初始化失败: {e}"
            )));
            return;
        }
    };
    let _ = ev_tx.send(PlayerEvent::Ready);
    let _ = ev_tx.send(PlayerEvent::Volume(volume));
    let _ = ev_tx.send(PlayerEvent::Muted(muted));
    let (download_tx, mut download_rx) = mpsc::unbounded_channel();
    let mut downloader: Option<tokio::task::JoinHandle<()>> = None;
    let mut decoder_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut seek_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut generation = 0_u64;
    let mut play_seq = 0_u64;
    let mut seek_seq = 0_u64;
    let mut playing = false;
    let mut ticker = tokio::time::interval(Duration::from_millis(250));

    loop {
        tokio::select! {
            command = cmd_rx.recv() => {
                let Some(command) = command else { break };
                match command {
                    PlayerCmd::Load { url, play_seq: next_seq } => {
                        generation = generation.wrapping_add(1);
                        play_seq = next_seq;
                        if let Some(task) = downloader.take() { task.abort(); }
                        if let Some(task) = decoder_task.take() { task.abort(); }
                        if let Some(task) = seek_task.take() { task.abort(); }
                        sink.stop();
                        playing = false;
                        if !url.starts_with("https://") && !url.starts_with("http://") {
                            let _ = ev_tx.send(PlayerEvent::Track { play_seq, event: TrackEvent::LoadFailed("无效的音频地址".into()) });
                            continue;
                        }
                        let id = generation;
                        let client = http.clone();
                        let tx = download_tx.clone();
                        downloader = Some(tokio::spawn(async move {
                            let result = download_audio(&client, &url, id, &tx).await;
                            let _ = tx.send(DownloadEvent::Finished(id, result.map_err(|e| format!("{e:#}"))));
                        }));
                    }
                    PlayerCmd::Stop => {
                        generation = generation.wrapping_add(1);
                        if let Some(task) = downloader.take() { task.abort(); }
                        if let Some(task) = decoder_task.take() { task.abort(); }
                        if let Some(task) = seek_task.take() { task.abort(); }
                        sink.stop();
                        playing = false;
                    }
                    PlayerCmd::TogglePause if playing => {
                        if sink.is_paused() { sink.play(); } else { sink.pause(); }
                        let _ = ev_tx.send(PlayerEvent::Track { play_seq, event: TrackEvent::Paused(sink.is_paused()) });
                    }
                    PlayerCmd::ToggleMute => {
                        muted = !muted;
                        sink.set_volume(if muted { 0.0 } else { volume as f32 / 100.0 });
                        let _ = ev_tx.send(PlayerEvent::Muted(muted));
                    }
                    PlayerCmd::AddVolume(delta) => {
                        volume = volume.saturating_add(delta).clamp(0, 100);
                        sink.set_volume(if muted { 0.0 } else { volume as f32 / 100.0 });
                        let _ = ev_tx.send(PlayerEvent::Volume(volume));
                    }
                    PlayerCmd::SeekRel(delta) if playing => {
                        let target = (sink.get_pos().as_secs_f64() + delta).max(0.0);
                        seek_seq = seek_seq.wrapping_add(1);
                        seek_task = spawn_seek(sink.clone(), target, generation, seek_seq, download_tx.clone());
                    }
                    PlayerCmd::SeekAbs(position) if playing => {
                        seek_seq = seek_seq.wrapping_add(1);
                        seek_task = spawn_seek(sink.clone(), position, generation, seek_seq, download_tx.clone());
                    }
                    PlayerCmd::Shutdown => break,
                    _ => {}
                }
            }
            message = download_rx.recv() => {
                let Some(message) = message else { continue };
                match message {
                    DownloadEvent::Ready(id, reader) if id == generation => {
                        let tx = download_tx.clone();
                        decoder_task = Some(tokio::task::spawn_blocking(move || {
                            let len = reader.known_len();
                            let result = len
                                .ok_or_else(|| "音频文件长度未知".to_string())
                                .and_then(|len| {
                                    rodio::Decoder::builder()
                                        .with_data(BufReader::new(reader))
                                        .with_mime_type("audio/mp4")
                                        .with_byte_len(len)
                                        .with_seekable(true)
                                        .build()
                                        .map_err(|e| e.to_string())
                                })
                                .map(|decoder| {
                                    let duration = decoder.total_duration();
                                    (decoder, duration)
                                });
                            let _ = tx.send(DownloadEvent::Decoded(id, result));
                        }));
                    }
                    DownloadEvent::Finished(id, result) if id == generation => {
                        downloader = None;
                        if let Err(e) = result {
                            generation = generation.wrapping_add(1);
                            if let Some(task) = decoder_task.take() { task.abort(); }
                            if let Some(task) = seek_task.take() { task.abort(); }
                            sink.stop();
                            playing = false;
                            let _ = ev_tx.send(PlayerEvent::Track { play_seq, event: TrackEvent::LoadFailed(format!("音频下载失败: {e}")) });
                        }
                    }
                    DownloadEvent::Decoded(id, result) if id == generation => {
                        decoder_task = None;
                        match result {
                            Ok((decoder, duration)) => {
                                if let Some(duration) = duration {
                                    let _ = ev_tx.send(PlayerEvent::Track { play_seq, event: TrackEvent::Duration(duration.as_secs_f64()) });
                                }
                                sink.append(decoder);
                                sink.play();
                                playing = true;
                                let _ = ev_tx.send(PlayerEvent::Track { play_seq, event: TrackEvent::FileLoaded });
                                let _ = ev_tx.send(PlayerEvent::Track { play_seq, event: TrackEvent::Paused(false) });
                            }
                            Err(e) => {
                                generation = generation.wrapping_add(1);
                                if let Some(task) = downloader.take() { task.abort(); }
                                if let Some(task) = seek_task.take() { task.abort(); }
                                let _ = ev_tx.send(PlayerEvent::Track { play_seq, event: TrackEvent::LoadFailed(format!("内置播放器加载失败: {e}")) });
                            }
                        }
                    }
                    DownloadEvent::Seeked(id, seq, result) if id == generation && seq == seek_seq => {
                        seek_task = None;
                        match result {
                            Ok(position) => { let _ = ev_tx.send(PlayerEvent::Track { play_seq, event: TrackEvent::TimePos(position) }); }
                            Err(e) => tracing::warn!("内置播放器跳转失败: {e}"),
                        }
                    }
                    _ => {}
                }
            }
            _ = ticker.tick() => {
                if playing {
                    if sink.empty() && downloader.is_none() {
                        playing = false;
                        let _ = ev_tx.send(PlayerEvent::Track { play_seq, event: TrackEvent::TrackEnded });
                    } else {
                        let _ = ev_tx.send(PlayerEvent::Track { play_seq, event: TrackEvent::TimePos(sink.get_pos().as_secs_f64()) });
                    }
                }
            }
        }
    }
    if let Some(task) = downloader {
        task.abort();
    }
    if let Some(task) = decoder_task {
        task.abort();
    }
    if let Some(task) = seek_task {
        task.abort();
    }
    sink.stop();
}

fn spawn_seek(
    sink: Arc<Player>,
    target: f64,
    generation: u64,
    seq: u64,
    tx: mpsc::UnboundedSender<DownloadEvent>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !target.is_finite() {
        return None;
    }
    Some(tokio::task::spawn_blocking(move || {
        let result = sink
            .try_seek(Duration::from_secs_f64(target.max(0.0)))
            .map(|()| sink.get_pos().as_secs_f64())
            .map_err(|e| e.to_string());
        let _ = tx.send(DownloadEvent::Seeked(generation, seq, result));
    }))
}

async fn download_audio(
    client: &reqwest::Client,
    url: &str,
    id: u64,
    tx: &mpsc::UnboundedSender<DownloadEvent>,
) -> Result<()> {
    // Never include the signed URL in errors or logs: its query may contain
    // credentials. YouTube usually returns a few MiB per track.
    // Googlevideo accepts bounded byte ranges for some signed AAC URLs but
    // rejects both plain GET and an open-ended Range request with HTTP 403.
    // Do not force Accept-Encoding: identity: it changed later-range behavior
    // for real VisionOS URLs during device tests.
    let response = request_range(client, url, 0).await?;
    let expected_total = response_total(&response, 0)?;
    if expected_total.is_some_and(|size| size > MAX_AUDIO_BYTES) {
        bail!("音频文件超过 {} MiB 限制", MAX_AUDIO_BYTES / 1024 / 1024);
    }
    let shared = ProgressiveFile::new(tempfile::tempfile()?, expected_total);
    let mut guard = WriterGuard::new(shared.clone());
    let mut writer = tokio::fs::File::from_std(shared.writer()?);
    let ready_sent = if response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        download_ranged(client, url, response, &shared, &mut writer, id, tx).await?
    } else {
        download_linear(response, expected_total, &shared, &mut writer, id, tx).await?
    };
    writer.flush().await?;
    drop(writer);
    guard.complete();
    if !ready_sent {
        let _ = tx.send(DownloadEvent::Ready(id, shared.reader()));
    }
    Ok(())
}

async fn download_linear(
    mut response: reqwest::Response,
    expected_total: Option<u64>,
    shared: &Arc<ProgressiveFile>,
    writer: &mut tokio::fs::File,
    id: u64,
    tx: &mpsc::UnboundedSender<DownloadEvent>,
) -> Result<bool> {
    let mut written = 0_u64;
    let mut ready_sent = false;
    while let Some(chunk) = tokio::time::timeout(NETWORK_STALL_TIMEOUT, response.chunk())
        .await
        .map_err(|_| anyhow::anyhow!("音频下载无进展，已超时"))?
        .map_err(|_| anyhow::anyhow!("音频下载中断"))?
    {
        written = written.saturating_add(chunk.len() as u64);
        if written > MAX_AUDIO_BYTES || expected_total.is_some_and(|size| written > size) {
            bail!("音频文件大小超出预期");
        }
        writer.write_all(&chunk).await?;
        writer.flush().await?;
        shared.publish(written);
        if !ready_sent && expected_total.is_some() && written >= PREBUFFER_BYTES {
            let _ = tx.send(DownloadEvent::Ready(id, shared.reader()));
            ready_sent = true;
        }
    }
    if written == 0 {
        bail!("音频流为空");
    }
    if expected_total.is_some_and(|size| size != written) {
        bail!("音频下载不完整");
    }
    Ok(ready_sent)
}

async fn download_ranged(
    client: &reqwest::Client,
    url: &str,
    mut response: reqwest::Response,
    shared: &Arc<ProgressiveFile>,
    writer: &mut tokio::fs::File,
    id: u64,
    tx: &mpsc::UnboundedSender<DownloadEvent>,
) -> Result<bool> {
    let total = response_total(&response, 0)?.expect("partial response has a total");
    if total == 0 {
        bail!("音频流为空");
    }
    let segment_count = total.div_ceil(RANGE_BYTES) as usize;
    let mut complete = vec![false; segment_count];
    let mut segment = 0_usize;
    let mut ready_sent = false;
    loop {
        let start = segment as u64 * RANGE_BYTES;
        let expected_end = (start + RANGE_BYTES).min(total) - 1;
        let header = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|h| h.to_str().ok())
            .ok_or_else(|| anyhow::anyhow!("音频服务器未提供有效 Content-Range"))?;
        let (actual_start, actual_end, actual_total) = parse_content_range(header)
            .ok_or_else(|| anyhow::anyhow!("音频服务器返回无效 Content-Range"))?;
        if (actual_start, actual_end, actual_total) != (start, expected_end, total) {
            bail!("音频服务器返回了错误的字节范围");
        }
        writer.seek(SeekFrom::Start(start)).await?;
        let mut received = 0_u64;
        let mut preempted = None;
        loop {
            let chunk = tokio::select! {
                result = tokio::time::timeout(NETWORK_STALL_TIMEOUT, response.chunk()) => {
                    result
                        .map_err(|_| anyhow::anyhow!("音频下载无进展，已超时"))?
                        .map_err(|_| anyhow::anyhow!("音频下载中断"))?
                }
                _ = shared.request_notified() => {
                    if let Some(index) = shared
                        .take_requested()
                        .filter(|position| *position < total)
                        .map(|position| (position / RANGE_BYTES) as usize)
                        .filter(|index| *index != segment && !complete[*index])
                    {
                        preempted = Some(index);
                        break;
                    }
                    continue;
                }
            };
            let Some(chunk) = chunk else { break };
            received += chunk.len() as u64;
            if received > expected_end - start + 1 {
                bail!("音频文件大小超出预期");
            }
            writer.write_all(&chunk).await?;
        }
        if preempted.is_some() {
            // Discarding the HTTP response does not discard bytes already
            // staged by tokio::fs::File. Flush before seeking elsewhere;
            // this incomplete segment remains unpublished and is retried.
            writer.flush().await?;
        } else {
            if received != expected_end - start + 1 {
                bail!("音频下载不完整");
            }
            writer.flush().await?;
            complete[segment] = true;
            shared.publish_range(start, expected_end + 1);
            if !ready_sent && segment == 0 {
                let _ = tx.send(DownloadEvent::Ready(id, shared.reader()));
                ready_sent = true;
            }
            if complete.iter().all(|done| *done) {
                break;
            }
        }
        segment = preempted.unwrap_or_else(|| {
            shared
                .take_requested()
                .and_then(|position| {
                    (position < total).then_some((position / RANGE_BYTES) as usize)
                })
                .filter(|index| !complete[*index])
                .unwrap_or_else(|| complete.iter().position(|done| !done).unwrap())
        });
        let next_start = segment as u64 * RANGE_BYTES;
        response = request_range(client, url, next_start).await?;
        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT
            || response_total(&response, next_start)? != Some(total)
        {
            bail!("音频服务器未按字节范围返回后续数据");
        }
    }
    Ok(ready_sent)
}

async fn request_range(
    client: &reqwest::Client,
    url: &str,
    start: u64,
) -> Result<reqwest::Response> {
    let end = start.saturating_add(RANGE_BYTES - 1);
    let response = tokio::time::timeout(
        NETWORK_STALL_TIMEOUT,
        client
            .get(url)
            .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
            .send(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("连接音频服务器超时"))?
    .map_err(|_| anyhow::anyhow!("无法连接音频服务器"))?;
    if response.status() != reqwest::StatusCode::OK
        && response.status() != reqwest::StatusCode::PARTIAL_CONTENT
    {
        bail!("音频服务器在偏移 {start} 返回 HTTP {}", response.status());
    }
    Ok(response)
}

fn response_total(response: &reqwest::Response, start: u64) -> Result<Option<u64>> {
    if response.status() == reqwest::StatusCode::OK {
        if start != 0 {
            bail!("音频服务器未按字节范围返回后续数据");
        }
        return Ok(response.content_length());
    }
    let header = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| anyhow::anyhow!("音频服务器未提供有效 Content-Range"))?;
    let (actual_start, _, total) = parse_content_range(header)
        .ok_or_else(|| anyhow::anyhow!("音频服务器返回无效 Content-Range"))?;
    if actual_start != start {
        bail!("音频服务器返回了错误的字节范围");
    }
    Ok(Some(total))
}

fn parse_content_range(header: &str) -> Option<(u64, u64, u64)> {
    let range = header.strip_prefix("bytes ")?;
    let (span, total) = range.split_once('/')?;
    let (start, end) = span.split_once('-')?;
    let (start, end, total) = (
        start.parse::<u64>().ok()?,
        end.parse::<u64>().ok()?,
        total.parse::<u64>().ok()?,
    );
    (start <= end && end < total).then_some((start, end, total))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[test]
    fn parses_only_valid_content_ranges() {
        assert_eq!(parse_content_range("bytes 0-99/200"), Some((0, 99, 200)));
        assert_eq!(parse_content_range("bytes 99-0/200"), None);
        assert_eq!(parse_content_range("bytes 0-200/200"), None);
        assert_eq!(parse_content_range("bytes */200"), None);
    }

    async fn serve_once(body: &'static [u8], advertised: Option<u64>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = socket.read(&mut request).await;
            let length = advertised.unwrap_or(body.len() as u64);
            let header =
                format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n");
            socket.write_all(header.as_bytes()).await.unwrap();
            if advertised.is_none() {
                socket.write_all(body).await.unwrap();
            }
        });
        url
    }

    #[tokio::test]
    #[ignore = "requires a loopback TCP listener"]
    async fn downloads_to_seekable_temporary_file() {
        let url = serve_once(b"audio bytes", None).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        download_audio(&reqwest::Client::new(), &url, 7, &tx)
            .await
            .unwrap();
        let DownloadEvent::Ready(7, mut file) = rx.recv().await.unwrap() else {
            panic!("expected ready reader");
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"audio bytes");
    }

    #[tokio::test]
    #[ignore = "requires a loopback TCP listener"]
    async fn rejects_oversized_audio_before_download() {
        let url = serve_once(b"", Some(MAX_AUDIO_BYTES + 1)).await;
        let (tx, _) = mpsc::unbounded_channel();
        let error = download_audio(&reqwest::Client::new(), &url, 7, &tx)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("512 MiB"));
    }

    #[tokio::test]
    #[ignore = "requires a loopback TCP listener"]
    async fn reader_is_ready_before_download_finishes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = socket.read(&mut request).await;
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                PREBUFFER_BYTES * 2
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket
                .write_all(&vec![1_u8; PREBUFFER_BYTES as usize])
                .await
                .unwrap();
            let _ = release_rx.await;
            socket
                .write_all(&vec![2_u8; PREBUFFER_BYTES as usize])
                .await
                .unwrap();
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let task =
            tokio::spawn(
                async move { download_audio(&reqwest::Client::new(), &url, 9, &tx).await },
            );
        let ready = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ready, DownloadEvent::Ready(9, _)));
        assert!(!task.is_finished());
        release_tx.send(()).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    #[ignore = "requires a loopback TCP listener"]
    async fn bounded_range_download_assembles_the_whole_file() {
        let body = vec![7_u8; (RANGE_BYTES * 2 + 123) as usize];
        let expected = body.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 2048];
                let size = socket.read(&mut request).await.unwrap();
                let headers = std::str::from_utf8(&request[..size]).unwrap();
                let range = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("range")
                                .then(|| value.trim().strip_prefix("bytes="))
                                .flatten()
                        })
                    })
                    .expect("missing bounded Range header");
                let (start, end) = range.split_once('-').unwrap();
                let start = start.parse::<usize>().unwrap();
                let end = end.parse::<usize>().unwrap().min(body.len() - 1);
                let bytes = &body[start..=end];
                let header = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len(),
                    bytes.len()
                );
                socket.write_all(header.as_bytes()).await.unwrap();
                socket.write_all(bytes).await.unwrap();
            }
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        download_audio(&reqwest::Client::new(), &url, 17, &tx)
            .await
            .unwrap();
        server.await.unwrap();
        let DownloadEvent::Ready(17, mut reader) = rx.recv().await.unwrap() else {
            panic!("missing reader");
        };
        let mut actual = Vec::new();
        reader.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    #[ignore = "requires a loopback TCP listener"]
    async fn far_seek_prioritizes_its_range_before_sequential_download() {
        let total = RANGE_BYTES * 12;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        let requested = Arc::new(std::sync::Mutex::new(Vec::new()));
        let requests = requested.clone();
        let stall_first_middle = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let release_middle = Arc::new(tokio::sync::Notify::new());
        let server_release = release_middle.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = tokio::select! {
                    connection = listener.accept() => connection.unwrap(),
                    _ = &mut stop_rx => break,
                };
                let requests = requests.clone();
                let stall_first_middle = stall_first_middle.clone();
                let release_middle = server_release.clone();
                tokio::spawn(async move {
                    let mut request = [0_u8; 2048];
                    let size = socket.read(&mut request).await.unwrap();
                    let headers = std::str::from_utf8(&request[..size]).unwrap();
                    let start = headers
                        .lines()
                        .find_map(|line| {
                            line.split_once(':').and_then(|(name, value)| {
                                name.eq_ignore_ascii_case("range")
                                    .then(|| value.trim().strip_prefix("bytes="))
                                    .flatten()
                            })
                        })
                        .and_then(|range| range.split_once('-'))
                        .and_then(|(start, _)| start.parse::<u64>().ok())
                        .expect("missing range");
                    let index = start / RANGE_BYTES;
                    requests.lock().unwrap().push(index);
                    let end = start + RANGE_BYTES - 1;
                    let header = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {RANGE_BYTES}\r\nConnection: close\r\n\r\n"
                    );
                    if socket.write_all(header.as_bytes()).await.is_ok() {
                        if index == 1
                            && stall_first_middle.swap(false, std::sync::atomic::Ordering::SeqCst)
                        {
                            // The sequential request never delivers its
                            // body until the far reader has been satisfied.
                            // The downloader must cancel it to make progress.
                            release_middle.notified().await;
                        } else {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                        let _ = socket
                            .write_all(&vec![index as u8; RANGE_BYTES as usize])
                            .await;
                    }
                });
            }
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let download =
            tokio::spawn(
                async move { download_audio(&reqwest::Client::new(), &url, 19, &tx).await },
            );
        let DownloadEvent::Ready(19, mut reader) = rx.recv().await.unwrap() else {
            panic!("missing prebuffered reader");
        };
        let tail = tokio::task::spawn_blocking(move || {
            reader
                .seek(std::io::SeekFrom::Start(RANGE_BYTES * 11))
                .unwrap();
            let mut byte = [0];
            reader.read_exact(&mut byte).unwrap();
            byte[0]
        });
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(4), tail)
                .await
                .unwrap()
                .unwrap(),
            11
        );
        release_middle.notify_one();
        download.await.unwrap().unwrap();
        stop_tx.send(()).unwrap();
        server.await.unwrap();
        let requested = requested.lock().unwrap().clone();
        println!("range request order: {requested:?}");
        assert!(requested.len() >= 12);
        let far_index = requested.iter().position(|index| *index == 11).unwrap();
        assert!(
            far_index <= 3,
            "far range was requested too late: {requested:?}"
        );
    }

    #[tokio::test]
    #[ignore = "requires a loopback TCP listener"]
    async fn later_range_403_fails_and_wakes_reader() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 2048];
                let size = socket.read(&mut request).await.unwrap();
                let headers = std::str::from_utf8(&request[..size]).unwrap();
                let expected_range = if attempt == 0 {
                    "bytes=0-1048575"
                } else {
                    "bytes=1048576-2097151"
                };
                assert!(headers.contains(expected_range));
                if attempt == 0 {
                    socket
                        .write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-1048575/1048577\r\nContent-Length: 1048576\r\nConnection: close\r\n\r\n")
                        .await
                        .unwrap();
                    socket
                        .write_all(&vec![1_u8; RANGE_BYTES as usize])
                        .await
                        .unwrap();
                } else {
                    socket
                        .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await
                        .unwrap();
                }
            }
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let error = download_audio(&reqwest::Client::new(), &url, 18, &tx)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("偏移 1048576 返回 HTTP 403"));
        server.await.unwrap();
        let DownloadEvent::Ready(18, mut reader) = rx.recv().await.unwrap() else {
            panic!("missing prebuffered reader");
        };
        reader.seek(std::io::SeekFrom::Start(RANGE_BYTES)).unwrap();
        assert!(reader.read(&mut [0; 1]).is_err());
    }

    #[test]
    #[ignore = "requires a physical or virtual audio output device"]
    fn default_audio_device_opens() {
        let _device = DeviceSinkBuilder::open_default_sink().expect("no usable audio output");
    }

    fn steam_device(input: bool) -> rodio::cpal::Device {
        use rodio::cpal::traits::{DeviceTrait, HostTrait};

        let host = rodio::cpal::default_host();
        let devices = if input {
            host.input_devices().unwrap()
        } else {
            host.output_devices().unwrap()
        };
        devices
            .into_iter()
            .find(|device| {
                device
                    .description()
                    .is_ok_and(|info| info.name() == "Steam Streaming Speakers")
            })
            .expect("Steam virtual audio device missing")
    }

    fn steam_capture() -> (
        rodio::cpal::Stream,
        Arc<std::sync::atomic::AtomicBool>,
        std::sync::mpsc::Receiver<std::time::Instant>,
    ) {
        use rodio::cpal::traits::{DeviceTrait, StreamTrait};
        use rodio::cpal::SampleFormat;
        use std::sync::atomic::{AtomicBool, Ordering};

        let input = steam_device(true);
        let input_format = input.default_input_config().unwrap();
        let (signal_tx, signal_rx) = std::sync::mpsc::sync_channel(1);
        let sent = Arc::new(AtomicBool::new(true));
        let error = |e| eprintln!("loopback input error: {e}");
        let input_stream = match input_format.sample_format() {
            SampleFormat::F32 => input.build_input_stream(
                &input_format.config(),
                {
                    let sent = sent.clone();
                    let tx = signal_tx.clone();
                    move |data: &[f32], _| {
                        if data.iter().any(|sample| sample.abs() > 0.05)
                            && !sent.swap(true, Ordering::SeqCst)
                        {
                            let _ = tx.try_send(std::time::Instant::now());
                        }
                    }
                },
                error,
                None,
            ),
            SampleFormat::I16 => input.build_input_stream(
                &input_format.config(),
                {
                    let sent = sent.clone();
                    let tx = signal_tx.clone();
                    move |data: &[i16], _| {
                        if data.iter().any(|sample| sample.unsigned_abs() > 1_638)
                            && !sent.swap(true, Ordering::SeqCst)
                        {
                            let _ = tx.try_send(std::time::Instant::now());
                        }
                    }
                },
                error,
                None,
            ),
            SampleFormat::U16 => input.build_input_stream(
                &input_format.config(),
                {
                    let sent = sent.clone();
                    move |data: &[u16], _| {
                        if data.iter().any(|sample| sample.abs_diff(32_768) > 1_638)
                            && !sent.swap(true, Ordering::SeqCst)
                        {
                            let _ = signal_tx.try_send(std::time::Instant::now());
                        }
                    }
                },
                error,
                None,
            ),
            format => panic!("unsupported virtual input format: {format:?}"),
        }
        .unwrap();
        input_stream.play().unwrap();
        (input_stream, sent, signal_rx)
    }

    #[test]
    #[ignore = "requires a virtual device whose input captures its output"]
    fn steam_virtual_audio_device_loopback_probe() {
        use std::sync::atomic::Ordering;

        let (_input_stream, sent, signal_rx) = steam_capture();
        std::thread::sleep(Duration::from_millis(100));
        let mut device = DeviceSinkBuilder::from_device(steam_device(false))
            .unwrap()
            .open_stream()
            .unwrap();
        device.log_on_drop(false);
        let sink = Player::connect_new(device.mixer());
        sink.set_volume(0.5);
        sent.store(false, Ordering::SeqCst);
        sink.append(
            rodio::source::SineWave::new(1_000.0)
                .take_duration(Duration::from_millis(500))
                .amplify(0.5),
        );
        let started = std::time::Instant::now();
        let detected = signal_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("virtual device did not loop output back to input");
        println!(
            "virtual loopback first signal={} ms",
            detected.duration_since(started).as_millis()
        );
    }

    async fn next_loopback_signal(
        receiver: &std::sync::mpsc::Receiver<std::time::Instant>,
    ) -> Option<std::time::Instant> {
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        loop {
            if let Ok(instant) = receiver.try_recv() {
                return Some(instant);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires YTBM_TEST_MPV, YTBM_TEST_AAC_FIXTURE, loopback TCP and Steam virtual audio"]
    async fn actual_first_sound_latency_is_within_mpv_ratio() {
        use std::sync::atomic::Ordering;

        let mpv_path = std::env::var("YTBM_TEST_MPV").expect("set YTBM_TEST_MPV");
        let fixture = std::env::var("YTBM_TEST_AAC_FIXTURE").expect("set YTBM_TEST_AAC_FIXTURE");
        let body = Arc::new(std::fs::read(&fixture).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let body = body.clone();
                tokio::spawn(async move {
                    let mut request = [0_u8; 2048];
                    if socket.read(&mut request).await.is_err() {
                        return;
                    }
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    if socket.write_all(header.as_bytes()).await.is_ok() {
                        let _ = socket.write_all(&body).await;
                    }
                });
            }
        });

        let (_input, armed, signal_rx) = steam_capture();
        let (native_cmd, native_rx) = mpsc::unbounded_channel();
        let (native_tx, mut native_ev) = mpsc::unbounded_channel();
        let native_task = tokio::spawn(run_on_device(
            100,
            Some(steam_device(false)),
            native_rx,
            native_tx,
        ));
        let ready = tokio::time::timeout(Duration::from_secs(90), native_ev.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ready, PlayerEvent::Ready), "{ready:?}");
        let mut native_ms = Vec::new();
        println!("native virtual sink ready");
        for seq in 1..=3 {
            tokio::time::sleep(Duration::from_millis(400)).await;
            armed.store(false, Ordering::SeqCst);
            let started = std::time::Instant::now();
            native_cmd
                .send(PlayerCmd::Load {
                    url: url.clone(),
                    play_seq: seq,
                })
                .unwrap();
            let detected = next_loopback_signal(&signal_rx)
                .await
                .expect("native produced no audible loopback sample");
            native_ms.push(detected.duration_since(started).as_millis());
            println!("native {seq}: {} ms", native_ms.last().unwrap());
            native_cmd.send(PlayerCmd::Stop).unwrap();
        }
        native_cmd.send(PlayerCmd::Shutdown).unwrap();
        native_task.await.unwrap();

        let (mpv_tx, mut mpv_events) = mpsc::unbounded_channel();
        let mut mpv = super::super::mpv_ipc::Mpv::spawn_with_audio_device(
            &mpv_path,
            100,
            "avfoundation/SteamStreamingSpeakers_UID",
            mpv_tx,
        )
        .await
        .unwrap();
        mpv.observe_default_properties().await.unwrap();
        let mut mpv_ms = Vec::new();
        println!("mpv virtual sink ready");
        for seq in 1..=3 {
            tokio::time::sleep(Duration::from_millis(400)).await;
            armed.store(false, Ordering::SeqCst);
            let started = std::time::Instant::now();
            mpv.load(&fixture, seq).await.unwrap();
            let detected = next_loopback_signal(&signal_rx).await.unwrap_or_else(|| {
                let events: Vec<_> = std::iter::from_fn(|| mpv_events.try_recv().ok()).collect();
                panic!("mpv produced no audible loopback sample; events={events:?}");
            });
            mpv_ms.push(detected.duration_since(started).as_millis());
            println!("mpv {seq}: {} ms", mpv_ms.last().unwrap());
            mpv.command(serde_json::json!(["stop"])).await.unwrap();
        }
        mpv.shutdown().await;
        server.abort();

        native_ms.sort_unstable();
        mpv_ms.sort_unstable();
        println!("actual first-sound ms: native={native_ms:?}, mpv={mpv_ms:?}");
        assert!(
            native_ms[1] * 2 <= mpv_ms[1] * 3,
            "native first sound exceeded 1.5x mpv"
        );
    }

    #[test]
    #[ignore = "requires YTBM_TEST_AAC_FIXTURE and a real audio output device"]
    fn real_device_plays_and_seeks_aac_fixture() {
        let path = std::env::var("YTBM_TEST_AAC_FIXTURE").expect("set YTBM_TEST_AAC_FIXTURE");
        let file = std::fs::File::open(path).unwrap();
        let len = file.metadata().unwrap().len();
        let decoder = rodio::Decoder::builder()
            .with_data(BufReader::new(file))
            .with_mime_type("audio/mp4")
            .with_byte_len(len)
            .with_seekable(true)
            .build()
            .unwrap();
        let device = DeviceSinkBuilder::open_default_sink().expect("no usable audio output");
        let sink = Player::connect_new(device.mixer());
        sink.set_volume(0.02);
        sink.append(decoder);
        std::thread::sleep(Duration::from_secs(2));
        assert!(sink.get_pos() > Duration::from_millis(500));
        sink.try_seek(Duration::from_secs(10)).unwrap();
        assert!(sink.get_pos() >= Duration::from_secs(9));
        sink.pause();
        assert!(sink.is_paused());
        sink.play();
        assert!(!sink.is_paused());
        sink.stop();
    }

    #[tokio::test]
    #[ignore = "requires YTBM_TEST_AAC_FIXTURE and a loopback TCP listener"]
    async fn local_aac_without_content_length_decodes() {
        let path = std::env::var("YTBM_TEST_AAC_FIXTURE").expect("set YTBM_TEST_AAC_FIXTURE");
        let body = std::fs::read(path).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            socket.write_all(&body).await.unwrap();
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        download_audio(&reqwest::Client::new(), &url, 31, &tx)
            .await
            .unwrap();
        server.await.unwrap();
        let DownloadEvent::Ready(31, reader) = rx.recv().await.unwrap() else {
            panic!("missing decoder input");
        };
        let len = reader.known_len().unwrap();
        let samples = rodio::Decoder::builder()
            .with_data(BufReader::new(reader))
            .with_mime_type("audio/mp4")
            .with_byte_len(len)
            .with_seekable(true)
            .build()
            .unwrap()
            .take(48_000)
            .count();
        assert!(samples > 1_000);
    }

    #[cfg(unix)]
    fn rss_kib() -> usize {
        let output = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires YTBM_TEST_AAC_FIXTURE, loopback TCP and audio output"]
    async fn real_device_hundred_switches_keep_rss_bounded() {
        use std::time::Instant;

        let path = std::env::var("YTBM_TEST_AAC_FIXTURE").expect("set YTBM_TEST_AAC_FIXTURE");
        let body = Arc::new(std::fs::read(path).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let body = body.clone();
                tokio::spawn(async move {
                    let mut request = [0_u8; 2048];
                    if socket.read(&mut request).await.is_err() {
                        return;
                    }
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    if socket.write_all(header.as_bytes()).await.is_ok() {
                        let _ = socket.write_all(&body).await;
                    }
                });
            }
        });

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let player_started = Instant::now();
        let player = tokio::spawn(run(0, cmd_rx, ev_tx));
        let ready = tokio::time::timeout(Duration::from_secs(5), ev_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ready, PlayerEvent::Ready), "{ready:?}");
        let device_init_ms = player_started.elapsed().as_millis();
        let before = rss_kib();

        for play_seq in 1..=100 {
            let started = Instant::now();
            cmd_tx
                .send(PlayerCmd::Load {
                    url: url.clone(),
                    play_seq,
                })
                .unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match ev_rx.recv().await.unwrap() {
                        PlayerEvent::Track {
                            play_seq: id,
                            event: TrackEvent::FileLoaded,
                        } if id == play_seq => break,
                        PlayerEvent::Track {
                            play_seq: id,
                            event: TrackEvent::LoadFailed(error),
                        } if id == play_seq => panic!("switch {play_seq}: {error}"),
                        _ => {}
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("switch {play_seq} timed out"));
            if play_seq == 1 {
                let load_ms = started.elapsed().as_millis();
                let total_load_ms = player_started.elapsed().as_millis();
                tokio::time::sleep(Duration::from_secs(1)).await;
                println!(
                    "native device init={device_init_ms} ms, first FileLoaded={load_ms} ms, total={} ms, steady RSS={} KiB",
                    total_load_ms,
                    rss_kib()
                );
            }
            cmd_tx.send(PlayerCmd::Stop).unwrap();
        }
        cmd_tx.send(PlayerCmd::Shutdown).unwrap();
        tokio::time::timeout(Duration::from_secs(10), player)
            .await
            .unwrap()
            .unwrap();
        server.abort();
        let after = rss_kib();
        println!("100 switches RSS: before={before} KiB after={after} KiB");
        assert!(after.saturating_sub(before) <= 20 * 1024);
    }

    /// Run with YTBM_TEST_AAC_FIXTURE and optionally YTBM_SOAK_SECONDS.
    /// The default is two hours of actual audio-device output at volume zero.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires a two-hour audio-device soak and loopback TCP"]
    async fn real_device_two_hour_playback_stays_bounded() {
        use std::time::Instant;

        let path = std::env::var("YTBM_TEST_AAC_FIXTURE").expect("set YTBM_TEST_AAC_FIXTURE");
        let seconds = std::env::var("YTBM_SOAK_SECONDS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(7200);
        let body = Arc::new(std::fs::read(path).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let body = body.clone();
                tokio::spawn(async move {
                    let mut request = [0_u8; 2048];
                    if socket.read(&mut request).await.is_err() {
                        return;
                    }
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    if socket.write_all(header.as_bytes()).await.is_ok() {
                        let _ = socket.write_all(&body).await;
                    }
                });
            }
        });

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let player = tokio::spawn(run(0, cmd_rx, ev_tx));
        assert!(matches!(ev_rx.recv().await, Some(PlayerEvent::Ready)));
        let before = rss_kib();
        let start = Instant::now();
        let mut next_report = Duration::from_secs(300);
        let mut completed = 0_u64;

        while start.elapsed() < Duration::from_secs(seconds) {
            let play_seq = completed + 1;
            cmd_tx
                .send(PlayerCmd::Load {
                    url: url.clone(),
                    play_seq,
                })
                .unwrap();
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match ev_rx.recv().await.expect("player event channel closed") {
                        PlayerEvent::Track {
                            play_seq: id,
                            event: TrackEvent::TrackEnded,
                        } if id == play_seq => break,
                        PlayerEvent::Track {
                            play_seq: id,
                            event: TrackEvent::LoadFailed(error),
                        } if id == play_seq => panic!("soak track {play_seq}: {error}"),
                        _ => {}
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("soak track {play_seq} timed out"));
            completed += 1;
            if start.elapsed() >= next_report {
                println!(
                    "soak elapsed={:.0}s tracks={} RSS={} KiB",
                    start.elapsed().as_secs_f64(),
                    completed,
                    rss_kib()
                );
                next_report += Duration::from_secs(300);
            }
        }

        cmd_tx.send(PlayerCmd::Shutdown).unwrap();
        player.await.unwrap();
        server.abort();
        let after = rss_kib();
        println!(
            "soak finished: {:.0}s, {completed} tracks, RSS before={before} after={after} KiB",
            start.elapsed().as_secs_f64()
        );
        assert!(after.saturating_sub(before) <= 20 * 1024);
    }

    #[tokio::test]
    #[ignore = "downloads and decodes a real YouTube Music AAC stream"]
    async fn real_aac_stream_decodes_without_external_player() {
        use crate::api::models::SearchKind;
        use crate::api::rustypipe::RustyPipeApi;
        use crate::api::{MusicApi, StreamFormat};

        let dir = tempfile::tempdir().unwrap();
        let api = RustyPipeApi::new(dir.path().join("rustypipe")).unwrap();
        let songs = api
            .search("Never Gonna Give You Up", SearchKind::Songs)
            .await
            .unwrap();
        let track = songs.tracks.first().expect("no search result");
        let video_id = &track.video_id;
        let url = api
            .stream_url(video_id, StreamFormat::Mp4Aac)
            .await
            .unwrap();
        assert!(url.starts_with("https://"));

        let client = reqwest::Client::builder()
            .user_agent(concat!("nakuru-music/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        download_audio(&client, &url, 42, &tx).await.unwrap();
        let DownloadEvent::Ready(42, reader) = rx.recv().await.unwrap() else {
            panic!("missing AAC reader");
        };
        let len = reader.known_len().unwrap();
        let decoder = rodio::Decoder::builder()
            .with_data(BufReader::new(reader))
            .with_mime_type("audio/mp4")
            .with_byte_len(len)
            .with_seekable(true)
            .build()
            .unwrap();
        let samples = decoder.count();
        assert!(samples > 48_000 * 30, "AAC stream decoded too few samples");
    }

    #[tokio::test]
    #[ignore = "downloads and decodes three real YouTube Music AAC streams"]
    async fn three_real_aac_songs_download_and_decode() {
        use crate::api::models::SearchKind;
        use crate::api::rustypipe::RustyPipeApi;
        use crate::api::{MusicApi, StreamFormat};

        let dir = tempfile::tempdir().unwrap();
        let api = RustyPipeApi::new(dir.path().join("rustypipe")).unwrap();
        let songs = api
            .search("Never Gonna Give You Up", SearchKind::Songs)
            .await
            .unwrap();
        assert!(songs.tracks.len() >= 3);
        let client = reqwest::Client::builder()
            .user_agent(concat!("nakuru-music/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap();

        for (index, track) in songs.tracks.iter().take(3).enumerate() {
            let url = api
                .stream_url(&track.video_id, StreamFormat::Mp4Aac)
                .await
                .unwrap();
            let (tx, mut rx) = mpsc::unbounded_channel();
            download_audio(&client, &url, index as u64, &tx)
                .await
                .unwrap_or_else(|error| panic!("{}: {error:#}", track.video_id));
            let DownloadEvent::Ready(_, reader) = rx.recv().await.unwrap() else {
                panic!("missing AAC reader for {}", track.video_id);
            };
            let len = reader.known_len().unwrap();
            let samples = rodio::Decoder::builder()
                .with_data(BufReader::new(reader))
                .with_mime_type("audio/mp4")
                .with_byte_len(len)
                .with_seekable(true)
                .build()
                .unwrap()
                .count();
            println!("{}: {len} bytes, {samples} decoded samples", track.video_id);
            assert!(samples > 48_000 * 30);
        }
    }

    #[tokio::test]
    #[ignore = "downloads five real songs and measures fresh-URL recovery"]
    async fn five_real_aac_songs_recover_within_three_attempts() {
        use crate::api::models::SearchKind;
        use crate::api::rustypipe::RustyPipeApi;
        use crate::api::{MusicApi, StreamFormat};

        let dir = tempfile::tempdir().unwrap();
        let api = RustyPipeApi::new(dir.path().join("rustypipe")).unwrap();
        let songs = api
            .search("Never Gonna Give You Up", SearchKind::Songs)
            .await
            .unwrap();
        assert!(songs.tracks.len() >= 5);
        let client = reqwest::Client::builder()
            .user_agent(concat!("nakuru-music/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap();

        let mut recovered = 0;
        for (index, track) in songs.tracks.iter().take(5).enumerate() {
            let mut complete = None;
            for attempt in 0..3 {
                if attempt > 0 {
                    tokio::time::sleep(Duration::from_millis(500 * attempt)).await;
                }
                let result = match api.stream_url(&track.video_id, StreamFormat::Mp4Aac).await {
                    Ok(url) => {
                        let (tx, mut rx) = mpsc::unbounded_channel();
                        download_audio(&client, &url, index as u64, &tx)
                            .await
                            .map(|()| rx.try_recv().expect("completed download has no reader"))
                    }
                    Err(error) => Err(error),
                };
                match result {
                    Ok(DownloadEvent::Ready(_, reader)) => {
                        complete = Some(reader);
                        if attempt > 0 {
                            recovered += 1;
                        }
                        println!("{}: attempt {} succeeded", track.video_id, attempt + 1);
                        break;
                    }
                    Ok(_) => panic!("unexpected download event"),
                    Err(error) => println!(
                        "{}: attempt {} failed: {error:#}",
                        track.video_id,
                        attempt + 1
                    ),
                }
            }
            let reader =
                complete.unwrap_or_else(|| panic!("{} failed all attempts", track.video_id));
            let len = reader.known_len().unwrap();
            let samples = rodio::Decoder::builder()
                .with_data(BufReader::new(reader))
                .with_mime_type("audio/mp4")
                .with_byte_len(len)
                .with_seekable(true)
                .build()
                .unwrap()
                .count();
            assert!(
                samples > 48_000 * 30,
                "{} decoded too few samples",
                track.video_id
            );
            println!("{}: {len} bytes, {samples} samples", track.video_id);
        }
        println!("five tracks completed; {recovered} recovered after retry");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "plays two complete online tracks on a real audio device"]
    async fn two_real_songs_play_to_end_on_one_device() {
        use crate::api::models::SearchKind;
        use crate::api::rustypipe::RustyPipeApi;
        use crate::api::{MusicApi, StreamFormat};

        let dir = tempfile::tempdir().unwrap();
        let api = RustyPipeApi::new(dir.path().join("rustypipe")).unwrap();
        let songs = api
            .search("Never Gonna Give You Up", SearchKind::Songs)
            .await
            .unwrap();
        assert!(songs.tracks.len() >= 2);

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let player = tokio::spawn(run(0, cmd_rx, ev_tx));
        assert!(matches!(ev_rx.recv().await, Some(PlayerEvent::Ready)));
        for (index, track) in songs.tracks.iter().take(2).enumerate() {
            let url = api
                .stream_url(&track.video_id, StreamFormat::Mp4Aac)
                .await
                .unwrap();
            let play_seq = index as u64 + 1;
            cmd_tx.send(PlayerCmd::Load { url, play_seq }).unwrap();
            let mut loaded = false;
            let mut furthest = 0.0_f64;
            tokio::time::timeout(Duration::from_secs(600), async {
                loop {
                    match ev_rx.recv().await.expect("player event channel closed") {
                        PlayerEvent::Track {
                            play_seq: seq,
                            event: TrackEvent::FileLoaded,
                        } if seq == play_seq => loaded = true,
                        PlayerEvent::Track {
                            play_seq: seq,
                            event: TrackEvent::TimePos(position),
                        } if seq == play_seq => furthest = furthest.max(position),
                        PlayerEvent::Track {
                            play_seq: seq,
                            event: TrackEvent::TrackEnded,
                        } if seq == play_seq => break,
                        PlayerEvent::Track {
                            play_seq: seq,
                            event: TrackEvent::LoadFailed(error),
                        } if seq == play_seq => panic!("{}: {error}", track.video_id),
                        _ => {}
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{} did not finish", track.video_id));
            assert!(loaded && furthest > 30.0, "{} never played", track.video_id);
            println!("{}: played to end, furthest={furthest:.1}s", track.video_id);
        }
        cmd_tx.send(PlayerCmd::Shutdown).unwrap();
        player.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "queries long online songs and reports their AAC stream sizes"]
    async fn long_online_audio_range_size_probe() {
        use crate::api::models::SearchKind;
        use crate::api::rustypipe::RustyPipeApi;
        use crate::api::{MusicApi, StreamFormat};

        let dir = tempfile::tempdir().unwrap();
        let api = RustyPipeApi::new(dir.path().join("rustypipe")).unwrap();
        let client = reqwest::Client::builder()
            .user_agent(concat!("nakuru-music/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap();
        for query in ["1 hour relaxing piano music", "2 hour classical music"] {
            let results = api.search(query, SearchKind::Songs).await.unwrap();
            let mut candidates: Vec<_> = results
                .tracks
                .iter()
                .filter(|track| track.duration_secs.unwrap_or(0) >= 1800)
                .collect();
            candidates.sort_by_key(|track| std::cmp::Reverse(track.duration_secs));
            for track in candidates.into_iter().take(3) {
                let url = api
                    .stream_url(&track.video_id, StreamFormat::Mp4Aac)
                    .await
                    .unwrap();
                let response = request_range(&client, &url, 0).await.unwrap();
                let bytes = response_total(&response, 0).unwrap().unwrap();
                println!(
                    "{}: duration={}s, AAC={} MiB",
                    track.video_id,
                    track.duration_secs.unwrap(),
                    bytes.div_ceil(1024 * 1024)
                );
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "plays and seeks a real multi-hour AAC stream on an audio device"]
    async fn real_long_audio_starts_and_seeks_with_bounded_rss() {
        use crate::api::models::SearchKind;
        use crate::api::rustypipe::RustyPipeApi;
        use crate::api::{MusicApi, StreamFormat};

        let dir = tempfile::tempdir().unwrap();
        let api = RustyPipeApi::new(dir.path().join("rustypipe")).unwrap();
        let results = api
            .search("1 hour relaxing piano music", SearchKind::Songs)
            .await
            .unwrap();
        let track = results
            .tracks
            .iter()
            .find(|track| track.duration_secs.unwrap_or(0) >= 10_800)
            .expect("no multi-hour song found");
        let url = api
            .stream_url(&track.video_id, StreamFormat::Mp4Aac)
            .await
            .unwrap();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let player = tokio::spawn(run(0, cmd_rx, ev_tx));
        assert!(matches!(ev_rx.recv().await, Some(PlayerEvent::Ready)));
        cmd_tx.send(PlayerCmd::Load { url, play_seq: 1 }).unwrap();
        tokio::time::timeout(Duration::from_secs(90), async {
            loop {
                match ev_rx.recv().await.expect("player event channel closed") {
                    PlayerEvent::Track {
                        play_seq: 1,
                        event: TrackEvent::FileLoaded,
                    } => break,
                    PlayerEvent::Track {
                        play_seq: 1,
                        event: TrackEvent::LoadFailed(error),
                    } => panic!("long audio load failed: {error}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("long audio load timed out");
        let before_seek = rss_kib();
        println!("long audio started before complete download");
        let seek_started = std::time::Instant::now();
        cmd_tx.send(PlayerCmd::SeekAbs(7_200.0)).unwrap();
        let reached = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match ev_rx.recv().await.expect("player event channel closed") {
                    PlayerEvent::Track {
                        play_seq: 1,
                        event: TrackEvent::TimePos(position),
                    } if position >= 7_195.0 => break position,
                    PlayerEvent::Track {
                        play_seq: 1,
                        event: TrackEvent::LoadFailed(error),
                    } => panic!("long audio seek failed: {error}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("long audio seek timed out");
        let seek_elapsed = seek_started.elapsed();
        cmd_tx.send(PlayerCmd::SeekAbs(60.0)).unwrap();
        let returned = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match ev_rx.recv().await.expect("player event channel closed") {
                    PlayerEvent::Track {
                        play_seq: 1,
                        event: TrackEvent::TimePos(position),
                    } if (55.0..90.0).contains(&position) => break position,
                    PlayerEvent::Track {
                        play_seq: 1,
                        event: TrackEvent::LoadFailed(error),
                    } => panic!("long audio return seek failed: {error}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("long audio return seek timed out");
        let after_seek = rss_kib();
        cmd_tx.send(PlayerCmd::Shutdown).unwrap();
        player.await.unwrap();
        println!(
            "{}: duration={}s, reached={reached:.1}s, returned={returned:.1}s, seek={seek_elapsed:?}, RSS loaded={before_seek} after seeks={after_seek} KiB",
            track.video_id,
            track.duration_secs.unwrap()
        );
        assert!(after_seek.saturating_sub(before_seek) <= 20 * 1024);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires YTBM_TEST_MPV, network access and a real audio output device"]
    async fn real_stream_steady_rss_is_below_mpv() {
        use crate::api::models::SearchKind;
        use crate::api::rustypipe::RustyPipeApi;
        use crate::api::{MusicApi, StreamFormat};

        let mpv = std::env::var("YTBM_TEST_MPV").expect("set YTBM_TEST_MPV");
        let dir = tempfile::tempdir().unwrap();
        let api = RustyPipeApi::new(dir.path().join("rustypipe")).unwrap();
        let songs = api
            .search("Never Gonna Give You Up", SearchKind::Songs)
            .await
            .unwrap();
        let url = api
            .stream_url(
                &songs.tracks.first().expect("no search result").video_id,
                StreamFormat::Mp4Aac,
            )
            .await
            .unwrap();

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let player = tokio::spawn(run(0, cmd_rx, ev_tx));
        assert!(matches!(ev_rx.recv().await, Some(PlayerEvent::Ready)));
        cmd_tx
            .send(PlayerCmd::Load {
                url: url.clone(),
                play_seq: 1,
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                match ev_rx.recv().await.expect("native player stopped") {
                    PlayerEvent::Track {
                        play_seq: 1,
                        event: TrackEvent::FileLoaded,
                    } => break,
                    PlayerEvent::Track {
                        play_seq: 1,
                        event: TrackEvent::LoadFailed(error),
                    } => panic!("native: {error}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("native load timed out");
        tokio::time::sleep(Duration::from_secs(5)).await;
        let native_rss = rss_kib();
        cmd_tx.send(PlayerCmd::Shutdown).unwrap();
        player.await.unwrap();

        let mut child = tokio::process::Command::new(mpv)
            .args([
                "--no-config",
                "--no-video",
                "--cache=no",
                "--no-terminal",
                "--volume=0",
            ])
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("mpv did not start");
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(
            child.try_wait().unwrap().is_none(),
            "mpv exited before the RSS sample"
        );
        let mpv_pid = child.id().unwrap();
        let output = std::process::Command::new("ps")
            .args(["-axo", "pid=,ppid=,rss="])
            .output()
            .unwrap();
        assert!(output.status.success());
        let processes: Vec<(u32, u32, usize)> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter_map(|line| {
                let mut columns = line.split_whitespace();
                Some((
                    columns.next()?.parse().ok()?,
                    columns.next()?.parse().ok()?,
                    columns.next()?.parse().ok()?,
                ))
            })
            .collect();
        let mut descendants = vec![mpv_pid];
        let mut mpv_rss = 0;
        while let Some(parent) = descendants.pop() {
            mpv_rss += processes
                .iter()
                .find(|(pid, _, _)| *pid == parent)
                .map_or(0, |(_, _, rss)| *rss);
            descendants.extend(
                processes
                    .iter()
                    .filter(|(_, ppid, _)| *ppid == parent)
                    .map(|(pid, _, _)| *pid),
            );
        }
        assert!(mpv_rss > 0, "mpv process tree was not visible to ps");
        child.kill().await.unwrap();
        child.wait().await.unwrap();

        println!(
            "same online AAC after 5s: native RSS={native_rss} KiB; mpv tree RSS={mpv_rss} KiB"
        );
        assert!(native_rss <= mpv_rss, "native steady RSS exceeded mpv");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires network access and a real audio output device"]
    async fn real_device_stream_pause_seek_and_finish() {
        use crate::api::models::SearchKind;
        use crate::api::rustypipe::RustyPipeApi;
        use crate::api::{MusicApi, StreamFormat};

        let dir = tempfile::tempdir().unwrap();
        let api = RustyPipeApi::new(dir.path().join("rustypipe")).unwrap();
        let songs = api
            .search("Never Gonna Give You Up", SearchKind::Songs)
            .await
            .unwrap();
        let video_id = &songs.tracks.first().expect("no search result").video_id;
        let url = api
            .stream_url(video_id, StreamFormat::Mp4Aac)
            .await
            .unwrap();

        #[cfg(unix)]
        println!("RSS after stream resolution: {} KiB", rss_kib());

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let player = tokio::spawn(run(0, cmd_rx, ev_tx));
        assert!(matches!(ev_rx.recv().await, Some(PlayerEvent::Ready)));
        cmd_tx.send(PlayerCmd::Load { url, play_seq: 1 }).unwrap();

        // The MP4 decoder may not know the duration until later segments are
        // downloaded; the UI already has this value from search metadata.
        let mut duration = songs.tracks[0].duration_secs.unwrap_or(0) as f64;
        tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                match ev_rx.recv().await.expect("player event channel closed") {
                    PlayerEvent::Track {
                        play_seq: 1,
                        event: TrackEvent::Duration(value),
                    } => duration = value,
                    PlayerEvent::Track {
                        play_seq: 1,
                        event: TrackEvent::FileLoaded,
                    } => break,
                    PlayerEvent::Track {
                        play_seq: 1,
                        event: TrackEvent::LoadFailed(error),
                    } => panic!("real stream failed: {error}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("real stream did not start");
        assert!(duration > 30.0, "unexpected duration: {duration}");
        #[cfg(unix)]
        println!("RSS after real FileLoaded: {} KiB", rss_kib());

        cmd_tx.send(PlayerCmd::TogglePause).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    ev_rx.recv().await,
                    Some(PlayerEvent::Track {
                        play_seq: 1,
                        event: TrackEvent::Paused(true)
                    })
                ) {
                    break;
                }
            }
        })
        .await
        .expect("pause did not arrive");
        cmd_tx.send(PlayerCmd::TogglePause).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    ev_rx.recv().await,
                    Some(PlayerEvent::Track {
                        play_seq: 1,
                        event: TrackEvent::Paused(false)
                    })
                ) {
                    break;
                }
            }
        })
        .await
        .expect("resume did not arrive");

        cmd_tx.send(PlayerCmd::SeekAbs(duration - 2.0)).unwrap();
        let mut reached_end = false;
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match ev_rx.recv().await.expect("player event channel closed") {
                    PlayerEvent::Track {
                        play_seq: 1,
                        event: TrackEvent::TimePos(position),
                    } if position >= duration - 3.0 => reached_end = true,
                    PlayerEvent::Track {
                        play_seq: 1,
                        event: TrackEvent::TrackEnded,
                    } => break,
                    PlayerEvent::Track {
                        play_seq: 1,
                        event: TrackEvent::LoadFailed(error),
                    } => panic!("real stream failed: {error}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("real stream did not finish after seek");
        assert!(reached_end, "seek never reached the track end");
        cmd_tx.send(PlayerCmd::Shutdown).unwrap();
        player.await.unwrap();
    }
}
