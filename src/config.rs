use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub playback: Playback,
    pub sponsorblock: SponsorBlock,
    pub lyrics: Lyrics,
    /// Optional global-key overrides, e.g. `next = "b"`; see [`parse_key`].
    pub keys: HashMap<String, String>,
    /// yt-dlp path resolved at startup - not read from the config file.
    #[serde(skip)]
    pub ytdlp_path: Option<String>,
}

/// Parse a config key string into a crossterm KeyCode.
/// Accepts a single character or the named keys below.
pub fn parse_key(s: &str) -> Option<ratatui::crossterm::event::KeyCode> {
    use ratatui::crossterm::event::KeyCode;
    let lower = s.trim().to_ascii_lowercase();
    Some(match lower.as_str() {
        "space" => KeyCode::Char(' '),
        "tab" => KeyCode::Tab,
        "enter" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        _ => {
            let mut chars = s.trim().chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None; // multi-char & not a named key
            }
            KeyCode::Char(c)
        }
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Playback {
    pub engine: PlaybackEngine,
    pub mpv_path: String,
    pub volume: i64,
    pub radio_auto: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackEngine {
    Mpv,
    #[default]
    Native,
}

impl Default for Playback {
    fn default() -> Self {
        Self {
            engine: PlaybackEngine::Native,
            mpv_path: "mpv".into(),
            volume: 70,
            radio_auto: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SponsorBlock {
    pub enabled: bool,
    pub categories: Vec<String>,
}

impl Default for SponsorBlock {
    fn default() -> Self {
        Self {
            enabled: true,
            categories: vec![
                "sponsor".into(),
                "selfpromo".into(),
                "interaction".into(),
                "music_offtopic".into(),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Lyrics {
    pub enabled: bool,
}

impl Default for Lyrics {
    fn default() -> Self {
        Self { enabled: true }
    }
}

pub struct Dirs {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

/// Locate an external tool without relying on the session PATH (freshly
/// installed scoop/winget tools are only on the PATH of *new* terminals).
/// Returns the first candidate that answers `--version`.
pub fn resolve_tool(configured: &str, exe_name: &str, scoop_pkg: &str) -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    if configured != exe_name {
        candidates.push(configured.to_string()); // explicit config path first
    }
    candidates.push(exe_name.to_string()); // PATH
    if let Some(root) = scoop_root() {
        candidates.push(
            root.join("apps")
                .join(scoop_pkg)
                .join("current")
                .join(format!("{exe_name}.exe"))
                .to_string_lossy()
                .into_owned(),
        );
        candidates.push(
            root.join("shims")
                .join(format!("{exe_name}.exe"))
                .to_string_lossy()
                .into_owned(),
        );
    }
    if let Ok(pf) = std::env::var("ProgramFiles") {
        candidates.push(format!("{pf}\\{exe_name}\\{exe_name}.exe"));
    }
    if let Ok(lad) = std::env::var("LOCALAPPDATA") {
        // winget symlink directory (usually on PATH, but be thorough)
        candidates.push(format!("{lad}\\Microsoft\\WinGet\\Links\\{exe_name}.exe"));
    }
    candidates.into_iter().find(|c| probe_version(c))
}

pub fn scoop_root() -> Option<PathBuf> {
    if let Ok(s) = std::env::var("SCOOP") {
        let p = PathBuf::from(s);
        if p.exists() {
            return Some(p);
        }
    }
    // Derive from a "..\scoop\shims" PATH entry (covers non-default roots).
    if let Ok(path) = std::env::var("PATH") {
        for p in std::env::split_paths(&path) {
            if p.ends_with("scoop\\shims") || p.ends_with("scoop/shims") {
                if let Some(root) = p.parent() {
                    return Some(root.to_path_buf());
                }
            }
        }
    }
    let home = std::env::var("USERPROFILE").ok()?;
    let p = PathBuf::from(home).join("scoop");
    p.exists().then_some(p)
}

pub fn probe_version(exe: &str) -> bool {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if started.elapsed() < std::time::Duration::from_secs(2) => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

pub fn project_dirs() -> Result<Dirs> {
    if let Some(root) = std::env::var_os("NAKURU_MUSIC_DATA_ROOT")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("YTBM_DATA_ROOT").filter(|value| !value.is_empty()))
    {
        let root = PathBuf::from(root);
        if !root.is_absolute() {
            bail!("NAKURU_MUSIC_DATA_ROOT 必须是绝对路径");
        }
        return Ok(Dirs {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
        });
    }
    let pd = ProjectDirs::from("", "", "NakuruMusic")
        .context("cannot determine platform config directory")?;
    let new_dirs = Dirs {
        config_dir: pd.config_dir().to_path_buf(),
        data_dir: pd.data_dir().to_path_buf(),
    };
    if new_dirs.config_dir.exists() || new_dirs.data_dir.exists() {
        return Ok(new_dirs);
    }
    // Existing installs keep their config, library login and play history in
    // place. Fresh installs use NakuruMusic directories.
    if let Some(old) = ProjectDirs::from("", "", "ytbm-tui") {
        if old.config_dir().exists() || old.data_dir().exists() {
            return Ok(Dirs {
                config_dir: old.config_dir().to_path_buf(),
                data_dir: old.data_dir().to_path_buf(),
            });
        }
    }
    Ok(new_dirs)
}

/// Load config.toml from the platform config dir; write a commented default
/// file on first run so users can discover the options.
pub fn load(dirs: &Dirs) -> Result<Config> {
    let path = dirs.config_dir.join("config.toml");
    if !path.exists() {
        std::fs::create_dir_all(&dirs.config_dir)?;
        std::fs::write(&path, DEFAULT_CONFIG_TOML)?;
        return Ok(Config::default());
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(cfg)
}

/// Change just the engine setting, preserving user comments and other keys.
pub fn save_engine(config_dir: &Path, engine: PlaybackEngine) -> Result<()> {
    let path = config_dir.join("config.toml");
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DEFAULT_CONFIG_TOML.to_owned(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut doc = source.parse::<toml_edit::DocumentMut>()?;
    doc["playback"]["engine"] = toml_edit::value(match engine {
        PlaybackEngine::Native => "native",
        PlaybackEngine::Mpv => "mpv",
    });
    crate::atomic_file::write(&path, doc.to_string().as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

const DEFAULT_CONFIG_TOML: &str = r#"# NakuruMusic 配置文件

[playback]
engine = "native"   # 默认内置 AAC 播放器；可在设置页切换到 mpv
mpv_path = "mpv"     # mpv 不在 PATH 时可写绝对路径
volume = 70
radio_auto = true    # 队列快播完时自动补充电台歌曲

[sponsorblock]
enabled = true
categories = ["sponsor", "selfpromo", "interaction", "music_offtopic"]

[lyrics]
enabled = true

[keys]
# 可选全局键位覆盖。可用动作名:
#   quit, search, help, focus, play_pause, next, prev, seek_back, seek_fwd,
#   vol_down, vol_up, repeat, radio, shuffle, lyrics, restart_player, settings
# 取值: 单个字符, 或 space/tab/enter/esc/left/right/up/down
# 示例:
# next = "b"
# play_pause = "space"
"#;

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn default_engine_is_native_and_setting_preserves_custom_config() {
        assert_eq!(Config::default().playback.engine, PlaybackEngine::Native);
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs {
            config_dir: tmp.path().join("config"),
            data_dir: tmp.path().join("data"),
        };
        std::fs::create_dir_all(&dirs.config_dir).unwrap();
        let path = dirs.config_dir.join("config.toml");
        std::fs::write(&path, "# My settings\n[playback]\nengine = 'native' # chosen\nvolume = 29\n[extra]\nvalue = 1\n").unwrap();
        save_engine(&dirs.config_dir, PlaybackEngine::Mpv).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# My settings"));
        assert!(text.contains("[extra]"));
        assert_eq!(load(&dirs).unwrap().playback.engine, PlaybackEngine::Mpv);
        assert_eq!(load(&dirs).unwrap().playback.volume, 29);
    }

    #[test]
    fn hung_tool_probe_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("hung-tool");
        std::fs::write(&script, "#!/bin/sh\nexec sleep 10\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let started = std::time::Instant::now();
        assert!(!probe_version(script.to_str().unwrap()));
        assert!(started.elapsed() < std::time::Duration::from_secs(4));
    }
}
