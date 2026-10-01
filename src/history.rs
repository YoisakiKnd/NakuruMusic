use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::api::models::Track;

const MAX_ENTRIES: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub track: Track,
    pub played_at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlaybackHistory {
    entries: Vec<HistoryEntry>,
}

impl PlaybackHistory {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取播放历史失败: {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("解析播放历史失败: {}", path.display()))
    }

    pub fn record(&mut self, track: Track) {
        self.entries
            .retain(|entry| entry.track.video_id != track.video_id);
        self.entries.insert(
            0,
            HistoryEntry {
                track,
                played_at: unix_seconds(),
            },
        );
        self.entries.truncate(MAX_ENTRIES);
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        crate::atomic_file::write(path, text.as_bytes())
            .with_context(|| format!("保存播放历史失败: {}", path.display()))?;
        Ok(())
    }

    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str) -> Track {
        Track {
            video_id: id.into(),
            title: id.into(),
            artists: "artist".into(),
            album: None,
            duration_secs: None,
            cover_url: None,
        }
    }

    #[test]
    fn record_moves_duplicates_to_front() {
        let mut history = PlaybackHistory::default();
        history.record(track("a"));
        history.record(track("b"));
        history.record(track("a"));
        assert_eq!(history.entries().len(), 2);
        assert_eq!(history.entries()[0].track.video_id, "a");
    }
}
