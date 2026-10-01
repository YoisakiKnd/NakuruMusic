use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::api::models::Track;
use crate::player::queue::{Queue, RepeatMode};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSession {
    pub tracks: Vec<Track>,
    pub current: Option<usize>,
    pub position: f64,
    pub repeat: RepeatMode,
    pub radio_on: bool,
}

impl SavedSession {
    pub fn from_queue(queue: &Queue, position: f64, radio_on: bool) -> Option<Self> {
        if queue.is_empty() {
            return None;
        }
        Some(Self {
            tracks: queue.items().to_vec(),
            current: queue.current_index(),
            position: position.max(0.0),
            repeat: queue.repeat,
            radio_on,
        })
    }

    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取播放会话失败: {}", path.display()))?;
        let session = serde_json::from_str(&text)
            .with_context(|| format!("解析播放会话失败: {}", path.display()))?;
        Ok(Some(session))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        crate::atomic_file::write(path, text.as_bytes())
            .with_context(|| format!("保存播放会话失败: {}", path.display()))?;
        Ok(())
    }
}
