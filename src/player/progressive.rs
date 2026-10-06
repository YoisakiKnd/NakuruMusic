//! Seekable reader over an anonymous file that is still being downloaded.
//! The writer publishes only bytes already written to the kernel. Readers
//! wait for missing bytes and wake on completion or cancellation.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex};
use tokio::sync::Notify;

struct State {
    /// Downloaded byte spans, sorted and merged. The file itself may be
    /// sparse after the decoder seeks ahead of the sequential download.
    available: Vec<(u64, u64)>,
    requested: Option<u64>,
    total: Option<u64>,
    done: bool,
    failed: bool,
}

pub(super) struct ProgressiveFile {
    file: File,
    writer_file: File,
    state: Mutex<State>,
    changed: Condvar,
    request_signal: Notify,
}

impl ProgressiveFile {
    pub(super) fn new(total: Option<u64>) -> io::Result<Arc<Self>> {
        // File::try_clone shares a cursor. On Windows, seek_read moves that
        // cursor, so the writer must be opened independently or later audio
        // chunks may overwrite earlier bytes while the decoder reads.
        #[cfg(windows)]
        let (file, writer_file) = {
            let named = tempfile::NamedTempFile::new()?;
            (named.reopen()?, named.reopen()?)
        };
        #[cfg(not(windows))]
        let (file, writer_file) = {
            let file = tempfile::tempfile()?;
            let writer_file = file.try_clone()?;
            (file, writer_file)
        };
        Ok(Arc::new(Self {
            file,
            writer_file,
            state: Mutex::new(State {
                available: Vec::new(),
                requested: None,
                total,
                done: false,
                failed: false,
            }),
            changed: Condvar::new(),
            request_signal: Notify::new(),
        }))
    }

    pub(super) fn writer(&self) -> io::Result<File> {
        self.writer_file.try_clone()
    }

    pub(super) fn reader(self: &Arc<Self>) -> ProgressiveReader {
        ProgressiveReader {
            shared: self.clone(),
            pos: 0,
        }
    }

    pub(super) fn publish(&self, written: u64) {
        self.publish_range(0, written);
    }

    pub(super) fn publish_range(&self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        let mut state = self.state.lock().unwrap();
        state.available.push((start, end));
        state.available.sort_unstable_by_key(|range| range.0);
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(state.available.len());
        for (start, end) in state.available.drain(..) {
            if let Some(last) = merged.last_mut() {
                if start <= last.1 {
                    last.1 = last.1.max(end);
                    continue;
                }
            }
            merged.push((start, end));
        }
        state.available = merged;
        self.changed.notify_all();
    }

    pub(super) fn take_requested(&self) -> Option<u64> {
        self.state.lock().unwrap().requested.take()
    }

    pub(super) async fn request_notified(&self) {
        self.request_signal.notified().await;
    }

    pub(super) fn finish(&self, failed: bool) {
        let mut state = self.state.lock().unwrap();
        if !failed && state.total.is_none() {
            state.total = Some(state.available.last().map_or(0, |range| range.1));
        }
        state.done = true;
        state.failed = failed;
        self.changed.notify_all();
    }
}

/// Marks the reader failed if the asynchronous writer is dropped by Stop.
pub(super) struct WriterGuard {
    shared: Arc<ProgressiveFile>,
    finished: bool,
}

impl WriterGuard {
    pub(super) fn new(shared: Arc<ProgressiveFile>) -> Self {
        Self {
            shared,
            finished: false,
        }
    }

    pub(super) fn complete(&mut self) {
        self.shared.finish(false);
        self.finished = true;
    }
}

impl Drop for WriterGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.shared.finish(true);
        }
    }
}

pub(super) struct ProgressiveReader {
    shared: Arc<ProgressiveFile>,
    pos: u64,
}

impl ProgressiveReader {
    pub(super) fn known_len(&self) -> Option<u64> {
        self.shared.state.lock().unwrap().total
    }
}

impl Read for ProgressiveReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut state = self.shared.state.lock().unwrap();
        loop {
            if let Some((_, end)) = state
                .available
                .iter()
                .find(|(start, end)| *start <= self.pos && self.pos < *end)
            {
                let available = (*end - self.pos).min(buf.len() as u64) as usize;
                drop(state);
                let count = read_at(&self.shared.file, &mut buf[..available], self.pos)?;
                if count == 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "缓存读取失败"));
                }
                self.pos += count as u64;
                return Ok(count);
            }
            if state.total.is_some_and(|total| self.pos >= total) {
                return Ok(0);
            }
            if state.done {
                return if state.failed {
                    Err(io::Error::new(io::ErrorKind::UnexpectedEof, "音频下载中断"))
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "音频缓存不完整",
                    ))
                };
            }
            state.requested = Some(self.pos);
            self.shared.request_signal.notify_one();
            state = self.shared.changed.wait(state).unwrap();
        }
    }
}

impl Seek for ProgressiveReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let base: i128 = match position {
            SeekFrom::Start(pos) => {
                self.pos = pos;
                return Ok(pos);
            }
            SeekFrom::Current(_) => self.pos.into(),
            SeekFrom::End(_) => {
                let mut state = self.shared.state.lock().unwrap();
                while state.total.is_none() && !state.done {
                    state = self.shared.changed.wait(state).unwrap();
                }
                if state.failed && state.total.is_none() {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "音频下载中断"));
                }
                state
                    .total
                    .unwrap_or_else(|| state.available.last().map_or(0, |range| range.1))
                    .into()
            }
        };
        let delta: i128 = match position {
            SeekFrom::Current(delta) | SeekFrom::End(delta) => delta.into(),
            SeekFrom::Start(_) => unreachable!(),
        };
        let target = base + delta;
        if !(0..=u64::MAX as i128).contains(&target) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "无效的音频跳转位置",
            ));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], pos: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, pos)
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], pos: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, pos)
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, Write};
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn reader_waits_for_published_bytes_and_wakes_on_cancel() {
        let shared = ProgressiveFile::new(Some(10)).unwrap();
        let mut writer = shared.writer().unwrap();
        let mut guard = WriterGuard::new(shared.clone());
        writer.write_all(b"hello").unwrap();
        shared.publish(5);
        let mut reader = shared.reader();
        let mut first = [0; 5];
        reader.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"hello");
        assert_eq!(reader.seek(SeekFrom::End(0)).unwrap(), 10);
        reader.seek(SeekFrom::Start(5)).unwrap();
        let task = std::thread::spawn(move || {
            let mut next = [0; 5];
            reader.read_exact(&mut next).map(|_| next)
        });
        writer.write_all(b"world").unwrap();
        shared.publish(10);
        guard.complete();
        assert_eq!(&task.join().unwrap().unwrap(), b"world");

        let failed = ProgressiveFile::new(None).unwrap();
        let mut waiting = failed.reader();
        let guard = WriterGuard::new(failed);
        drop(guard);
        assert!(waiting.read(&mut [0; 1]).is_err());

        let unknown = ProgressiveFile::new(None).unwrap();
        let mut writer = unknown.writer().unwrap();
        writer.write_all(b"abc").unwrap();
        unknown.publish(3);
        let mut guard = WriterGuard::new(unknown.clone());
        guard.complete();
        assert_eq!(unknown.reader().known_len(), Some(3));
    }

    #[test]
    fn sparse_read_requests_far_segment_before_middle_bytes_exist() {
        let shared = ProgressiveFile::new(Some(12)).unwrap();
        let mut writer = shared.writer().unwrap();
        writer.write_all(b"head").unwrap();
        shared.publish_range(0, 4);

        let mut reader = shared.reader();
        reader.seek(SeekFrom::Start(8)).unwrap();
        let waiting = std::thread::spawn(move || {
            let mut tail = [0; 4];
            reader.read_exact(&mut tail).map(|_| tail)
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if shared.take_requested() == Some(8) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "reader did not request sparse range"
            );
            std::thread::yield_now();
        }
        writer.seek(SeekFrom::Start(8)).unwrap();
        writer.write_all(b"tail").unwrap();
        shared.publish_range(8, 12);
        assert_eq!(&waiting.join().unwrap().unwrap(), b"tail");
        assert_eq!(shared.reader().known_len(), Some(12));
    }

    #[test]
    fn decoder_reads_cannot_move_download_writer_cursor() {
        let shared = ProgressiveFile::new(Some(6)).unwrap();
        let mut writer = shared.writer().unwrap();
        writer.write_all(b"abcd").unwrap();
        shared.publish(4);

        let mut decoder = shared.reader();
        let mut first = [0; 1];
        decoder.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"a");

        writer.write_all(b"ef").unwrap();
        shared.publish(6);
        let mut complete = [0; 6];
        shared.reader().read_exact(&mut complete).unwrap();
        assert_eq!(&complete, b"abcdef");
    }
}
