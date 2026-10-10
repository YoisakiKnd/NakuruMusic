//! Native browser cookie extraction - reads browser cookie databases directly.
//!
//! Two storage schemes are handled:
//!
//! - **Firefox**: `cookies.sqlite`, values stored in plaintext.
//! - **Chromium** (Chrome/Edge/Brave/Vivaldi/..): `Cookies` SQLite DB with
//!   AES-256-GCM encrypted values; the key lives in `Local State`, itself
//!   wrapped with Windows DPAPI.
//!
//! Chrome 127+ additionally binds that key to the Chrome binary
//! ("App-Bound Encryption"), which no external process can unwrap - not this
//! code and not yt-dlp either. Those profiles are detected and reported with
//! an actionable message rather than a decryption error.
//!
//! Security: cookie values are never logged and never rendered; the temporary
//! DB copy is deleted as soon as it has been read.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use tracing::info;

/// Which on-disk cookie format a profile uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CookieStore {
    Firefox,
    Chromium,
}

/// One importable browser profile.
#[derive(Debug, Clone)]
pub struct BrowserProfile {
    /// Label shown in the UI.
    pub display: String,
    pub store: CookieStore,
    /// Path to the cookie database.
    pub db: PathBuf,
    /// Chromium only: `Local State`, holding the DPAPI-wrapped AES key.
    pub local_state: Option<PathBuf>,
}

/// Where a browser keeps its user data.
#[derive(Clone, Copy)]
enum Base {
    #[cfg(windows)]
    LocalAppData,
    #[cfg(windows)]
    RoamingAppData,
    /// `<scoop>/persist/<rel>`
    #[cfg(windows)]
    ScoopPersist,
    /// `$HOME/<rel>`
    #[cfg(not(windows))]
    Home,
}

struct Candidate {
    display: &'static str,
    store: CookieStore,
    /// Firefox: a directory holding profile dirs. Chromium: a `User Data` dir.
    paths: &'static [(Base, &'static str)],
}

#[cfg(windows)]
const CANDIDATES: &[Candidate] = &[
    Candidate {
        display: "Firefox",
        store: CookieStore::Firefox,
        paths: &[
            (Base::RoamingAppData, "Mozilla/Firefox/Profiles"),
            (Base::ScoopPersist, "firefox/profile"),
        ],
    },
    Candidate {
        display: "Chrome",
        store: CookieStore::Chromium,
        paths: &[(Base::LocalAppData, "Google/Chrome/User Data")],
    },
    Candidate {
        display: "Edge",
        store: CookieStore::Chromium,
        paths: &[(Base::LocalAppData, "Microsoft/Edge/User Data")],
    },
    Candidate {
        display: "Brave",
        store: CookieStore::Chromium,
        paths: &[
            (Base::LocalAppData, "BraveSoftware/Brave-Browser/User Data"),
            (Base::ScoopPersist, "brave/User Data"),
        ],
    },
    Candidate {
        display: "Vivaldi",
        store: CookieStore::Chromium,
        paths: &[
            (Base::LocalAppData, "Vivaldi/User Data"),
            (Base::ScoopPersist, "vivaldi/User Data"),
        ],
    },
    Candidate {
        display: "Chromium",
        store: CookieStore::Chromium,
        paths: &[
            (Base::LocalAppData, "Chromium/User Data"),
            (Base::ScoopPersist, "chromium/User Data"),
        ],
    },
    Candidate {
        display: "Opera",
        store: CookieStore::Chromium,
        paths: &[(Base::RoamingAppData, "Opera Software/Opera Stable")],
    },
    Candidate {
        display: "Thorium",
        store: CookieStore::Chromium,
        paths: &[
            (Base::ScoopPersist, "thorium/USER_DATA"),
            (Base::LocalAppData, "Thorium/User Data"),
        ],
    },
];

#[cfg(target_os = "macos")]
const CANDIDATES: &[Candidate] = &[
    Candidate {
        display: "Waterfox",
        store: CookieStore::Firefox,
        paths: &[(Base::Home, "Library/Application Support/Waterfox/Profiles")],
    },
    Candidate {
        display: "Firefox",
        store: CookieStore::Firefox,
        paths: &[(Base::Home, "Library/Application Support/Firefox/Profiles")],
    },
    Candidate {
        display: "Chrome",
        store: CookieStore::Chromium,
        paths: &[(Base::Home, "Library/Application Support/Google/Chrome")],
    },
];

#[cfg(all(unix, not(target_os = "macos")))]
const CANDIDATES: &[Candidate] = &[
    Candidate {
        display: "Firefox",
        store: CookieStore::Firefox,
        paths: &[
            (Base::Home, ".mozilla/firefox"),
            (Base::Home, ".config/mozilla/firefox"),
            (Base::Home, "snap/firefox/common/.mozilla/firefox"),
        ],
    },
    Candidate {
        display: "Chrome",
        store: CookieStore::Chromium,
        paths: &[(Base::Home, ".config/google-chrome")],
    },
    Candidate {
        display: "Chromium",
        store: CookieStore::Chromium,
        paths: &[(Base::Home, ".config/chromium")],
    },
];

fn resolve(base: Base, rel: &str) -> Option<PathBuf> {
    let root = match base {
        #[cfg(windows)]
        Base::LocalAppData => PathBuf::from(std::env::var_os("LOCALAPPDATA")?),
        #[cfg(windows)]
        Base::RoamingAppData => PathBuf::from(std::env::var_os("APPDATA")?),
        #[cfg(windows)]
        Base::ScoopPersist => crate::config::scoop_root()?.join("persist"),
        #[cfg(not(windows))]
        Base::Home => PathBuf::from(std::env::var_os("HOME")?),
    };
    Some(root.join(rel))
}

/// Probe for readable browser profiles. Cheap filesystem checks only.
pub fn detect() -> Vec<BrowserProfile> {
    let mut out: Vec<BrowserProfile> = Vec::new();
    for c in CANDIDATES {
        for (base, rel) in c.paths {
            let Some(dir) = resolve(*base, rel) else {
                continue;
            };
            if !dir.exists() {
                continue;
            }
            match c.store {
                CookieStore::Firefox => collect_firefox(c.display, &dir, &mut out),
                CookieStore::Chromium => collect_chromium(c.display, &dir, &mut out),
            }
        }
    }
    disambiguate(&mut out);
    out
}

/// The same browser can be installed twice (e.g. scoop *and* per-user), which
/// would otherwise produce two rows with identical labels and no way to tell
/// them apart. Append a path hint to any label that is not unique.
fn disambiguate(profiles: &mut [BrowserProfile]) {
    let dupes: Vec<String> = profiles
        .iter()
        .filter(|p| {
            profiles
                .iter()
                .filter(|q| q.display == p.display)
                .take(2)
                .count()
                > 1
        })
        .map(|p| p.display.clone())
        .collect();
    for p in profiles.iter_mut() {
        if !dupes.contains(&p.display) {
            continue;
        }
        // The profile dir's grandparent is what actually differs (the install
        // root); showing its tail is enough to tell the two apart.
        if let Some(hint) =
            p.db.ancestors()
                .nth(3)
                .and_then(|a| a.file_name())
                .map(|n| n.to_string_lossy().into_owned())
        {
            p.display = format!("{} @{hint}", p.display);
        }
    }
}

/// A Firefox `Profiles` dir holds one directory per profile; a scoop-style
/// path may already be the profile itself.
fn collect_firefox(display: &str, dir: &Path, out: &mut Vec<BrowserProfile>) {
    let mut push = |db: PathBuf, label: String| {
        if db.is_file() && !out.iter().any(|p| p.db == db) {
            out.push(BrowserProfile {
                display: label,
                store: CookieStore::Firefox,
                db,
                local_state: None,
            });
        }
    };

    let direct = dir.join("cookies.sqlite");
    if direct.is_file() {
        push(direct, display.to_string());
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut profiles: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("cookies.sqlite").is_file())
        .collect();
    // Stable ordering, and the ".default" profiles are the interesting ones.
    profiles.sort();
    for p in profiles {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Profile dirs are named "<salt>.<label>" - the salt is noise.
        let label = name
            .split_once('.')
            .map_or(name.clone(), |(_, l)| l.to_string());
        push(p.join("cookies.sqlite"), format!("{display} ({label})"));
    }
}

/// A Chromium `User Data` dir holds `Default`, `Profile 1`, .. subdirectories.
/// Newer versions moved the DB into a `Network` subfolder.
fn collect_chromium(display: &str, user_data: &Path, out: &mut Vec<BrowserProfile>) {
    let local_state = user_data.join("Local State");
    let Ok(entries) = std::fs::read_dir(user_data) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    dirs.sort();
    for d in dirs {
        if !d.is_dir() {
            continue;
        }
        let db = ["Network/Cookies", "Cookies"]
            .iter()
            .map(|rel| d.join(rel))
            .find(|p| p.is_file());
        let Some(db) = db else { continue };
        if out.iter().any(|p| p.db == db) {
            continue;
        }
        let name = d
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        out.push(BrowserProfile {
            display: format!("{display} ({name})"),
            store: CookieStore::Chromium,
            db,
            local_state: Some(local_state.clone()),
        });
    }
}

/// Read the YouTube cookies of `profile` and return a `name=value; ..` header.
///
/// Called from a background task - it does blocking file I/O.
pub fn read_cookies(profile: &BrowserProfile) -> Result<String> {
    let scratch = CookieScratchDir::new()?;
    let pairs = match profile.store {
        CookieStore::Firefox => firefox::read(&profile.db, scratch.path())?,
        CookieStore::Chromium => {
            let local_state = profile
                .local_state
                .as_deref()
                .context("找不到该浏览器的 Local State 文件")?;
            chromium::read(&profile.db, local_state, scratch.path())?
        }
    };

    let header = super::browser_login::cookie_header_from_pairs(pairs)?;
    if !super::browser_login::has_auth_credential(&header) {
        bail!(
            "{} 尚未登录 YouTube（没有找到登录凭据）。\n请先在该浏览器登录 music.youtube.com，再回来导入。",
            profile.display
        );
    }
    // Log counts only - never cookie material.
    info!(
        "read {} youtube cookies from {:?} profile",
        header.split(';').count(),
        profile.store
    );
    Ok(header)
}

/// Private, short-lived workspace for a browser database snapshot.
struct CookieScratchDir(PathBuf);

/// Remove snapshots left behind by crashes. Every import normally removes its
/// own directory immediately; only directories older than a day are touched,
/// so another running instance's current import remains untouched.
pub fn cleanup_stale_scratch() {
    let removed = cleanup_stale_scratch_in(&std::env::temp_dir(), Duration::from_secs(24 * 3600));
    if removed > 0 {
        info!("removed {removed} stale cookie scratch directories");
    }
}

fn cleanup_stale_scratch_in(root: &Path, max_age: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !scratch_name(name) {
            continue;
        }
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        if !meta.file_type().is_dir() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if SystemTime::now()
            .duration_since(modified)
            .is_ok_and(|age| age > max_age)
            && std::fs::remove_dir_all(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

fn scratch_name(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix("nakuru-cookie-")
        .or_else(|| name.strip_prefix("ytbm-cookie-"))
    else {
        return false;
    };
    let Some((pid, nonce)) = rest.split_once('-') else {
        return false;
    };
    !pid.is_empty()
        && pid.bytes().all(|c| c.is_ascii_digit())
        && nonce.len() == 16
        && nonce.bytes().all(|c| c.is_ascii_hexdigit())
}

impl CookieScratchDir {
    fn new() -> Result<Self> {
        for _ in 0..8 {
            let path = std::env::temp_dir().join(format!(
                "nakuru-cookie-{}-{:016x}",
                std::process::id(),
                rand::random::<u64>()
            ));
            #[cfg(unix)]
            let builder = {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = std::fs::DirBuilder::new();
                builder.mode(0o700);
                builder
            };
            #[cfg(not(unix))]
            let builder = std::fs::DirBuilder::new();
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        anyhow::bail!("无法创建临时 Cookie 工作目录")
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for CookieScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Copy a SQLite DB (plus its WAL sidecars) somewhere we can open it.
///
/// Browsers keep the database open and Windows denies a shared read lock, so
/// reading in place fails while the browser runs. The `-wal` file must come
/// along or recently written cookies would be missing.
fn copy_db(src: &Path, work_dir: &Path, tag: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(work_dir)?;
    let dst = work_dir.join(format!("{tag}-{}.sqlite", std::process::id()));
    std::fs::copy(src, &dst).with_context(|| {
        format!(
            "无法读取 Cookie 数据库（{}）。\n请先完全关闭该浏览器再重试。",
            src.display()
        )
    })?;
    for ext in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{ext}", src.display()));
        if side.exists() {
            let _ = std::fs::copy(&side, format!("{}{ext}", dst.display()));
        }
    }
    Ok(dst)
}

/// Delete a copied DB and its sidecars.
fn remove_db(path: &Path) {
    let _ = std::fs::remove_file(path);
    for ext in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
    }
}

/// Keep only cookies belonging to YouTube.
fn is_youtube_host(host: &str) -> bool {
    let h = host.trim_start_matches('.');
    h == "youtube.com" || h.ends_with(".youtube.com")
}

mod firefox {
    use super::{copy_db, is_youtube_host, remove_db};
    use anyhow::{Context, Result};
    use std::path::Path;

    /// Firefox stores cookie values in plaintext, so this is a plain query.
    pub fn read(db: &Path, work_dir: &Path) -> Result<Vec<(String, String)>> {
        let copy = copy_db(db, work_dir, "ff-cookies")?;
        let result = read_from(&copy);
        remove_db(&copy);
        result
    }

    fn read_from(db: &Path) -> Result<Vec<(String, String)>> {
        let conn =
            rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .context("打开 Firefox Cookie 数据库失败")?;
        let mut stmt = conn
            .prepare("SELECT host, name, value FROM moz_cookies")
            .context("Firefox Cookie 数据库结构不符合预期")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (host, name, value) = row?;
            if is_youtube_host(&host) && !name.is_empty() {
                out.push((name, value));
            }
        }
        Ok(out)
    }
}

mod chromium {
    use super::{copy_db, is_youtube_host, remove_db};
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    use anyhow::{bail, Context, Result};
    use base64::Engine;
    use std::path::Path;

    pub fn read(db: &Path, local_state: &Path, work_dir: &Path) -> Result<Vec<(String, String)>> {
        let key = master_key(local_state)?;
        let copy = copy_db(db, work_dir, "cr-cookies")?;
        let result = read_from(&copy, &key);
        remove_db(&copy);
        result
    }

    fn read_from(db: &Path, key: &[u8]) -> Result<Vec<(String, String)>> {
        let conn =
            rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .context("打开 Chromium Cookie 数据库失败")?;
        let mut stmt = conn
            .prepare("SELECT host_key, name, value, encrypted_value FROM cookies")
            .context("Chromium Cookie 数据库结构不符合预期")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Vec<u8>>(3)?,
            ))
        })?;

        let mut out = Vec::new();
        let mut app_bound = 0usize;
        let mut undecryptable = 0usize;
        let mut tags: Vec<String> = Vec::new();
        for row in rows {
            let (host, name, plain, encrypted) = row?;
            if !is_youtube_host(&host) || name.is_empty() {
                continue;
            }
            // Very old profiles kept the value in the clear.
            if encrypted.is_empty() {
                if !plain.is_empty() {
                    out.push((name, plain));
                }
                continue;
            }
            match decrypt_value(&encrypted, key) {
                Ok(v) => out.push((name, v)),
                Err(Decrypt::AppBound) => app_bound += 1,
                // One bad row should not sink the whole import.
                Err(Decrypt::Other) => {
                    undecryptable += 1;
                    // The 3-byte version tag is not cookie material, and it is
                    // the one thing that explains *why* a profile failed.
                    let tag = String::from_utf8_lossy(&encrypted[..encrypted.len().min(3)])
                        .chars()
                        .filter(|c| c.is_ascii_graphic())
                        .collect::<String>();
                    if !tags.contains(&tag) {
                        tags.push(tag);
                    }
                }
            }
        }
        if undecryptable > 0 {
            tracing::debug!("undecryptable cookie version tags: {tags:?}");
        }

        // Distinguish "logged out" from "we could not read it" - otherwise a
        // decryption problem masquerades as an empty profile and the user
        // gets sent off to re-login for no reason.
        if out.is_empty() {
            if app_bound > 0 {
                bail!(
                    "该浏览器启用了 App-Bound Encryption（Chrome 127+），\n\
                     密钥绑定在浏览器进程上，任何外部程序都无法解密（yt-dlp 同样不行）。\n\
                     请改用 Firefox 导入，或选择「手动粘贴 Cookie」。"
                );
            }
            if undecryptable > 0 {
                bail!(
                    "读到了 {undecryptable} 条 YouTube Cookie，但都无法解密（格式 {tags:?}）。\n\
                     请改用 Firefox 导入，或选择「手动粘贴 Cookie」。"
                );
            }
        }
        Ok(out)
    }

    enum Decrypt {
        /// v20 - App-Bound Encryption, unwrappable only by Chrome itself.
        AppBound,
        Other,
    }

    /// Chrome 127+ prefixes the plaintext with a SHA-256 of the cookie's host,
    /// binding the value to its domain.
    const DOMAIN_HASH_LEN: usize = 32;

    /// `v10`/`v11`: `prefix(3) || nonce(12) || ciphertext || tag(16)`.
    fn decrypt_value(blob: &[u8], key: &[u8]) -> Result<String, Decrypt> {
        if blob.starts_with(b"v20") {
            return Err(Decrypt::AppBound);
        }
        if !(blob.starts_with(b"v10") || blob.starts_with(b"v11")) || blob.len() < 3 + 12 + 16 {
            return Err(Decrypt::Other);
        }
        let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| Decrypt::Other)?;
        let nonce = Nonce::try_from(&blob[3..15]).map_err(|_| Decrypt::Other)?;
        let plain = cipher
            .decrypt(&nonce, &blob[15..])
            .map_err(|_| Decrypt::Other)?;

        // Older builds store the value directly; newer ones put the domain
        // hash in front of it. Authentication already passed at this point, so
        // whichever of the two parses as UTF-8 is the real value.
        if let Ok(s) = std::str::from_utf8(&plain) {
            return Ok(s.to_owned());
        }
        if plain.len() > DOMAIN_HASH_LEN {
            if let Ok(s) = std::str::from_utf8(&plain[DOMAIN_HASH_LEN..]) {
                return Ok(s.to_owned());
            }
        }
        Err(Decrypt::Other)
    }

    /// Pull the AES key out of `Local State` and unwrap it with DPAPI.
    fn master_key(local_state: &Path) -> Result<Vec<u8>> {
        let text = std::fs::read_to_string(local_state)
            .with_context(|| format!("读取 {} 失败", local_state.display()))?;
        let json: serde_json::Value =
            serde_json::from_str(&text).context("Local State 不是合法的 JSON")?;
        let encoded = json
            .get("os_crypt")
            .and_then(|c| c.get("encrypted_key"))
            .and_then(serde_json::Value::as_str)
            .context("Local State 里没有 os_crypt.encrypted_key")?;
        let has_abe = json
            .get("os_crypt")
            .and_then(|c| c.get("app_bound_encrypted_key"))
            .is_some();
        if has_abe {
            tracing::debug!("Chromium Local State contains an app-bound encryption key");
        }
        let raw = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .context("os_crypt.encrypted_key 不是合法的 base64")?;
        // The blob is tagged "DPAPI" before the actual ciphertext.
        let wrapped = raw
            .strip_prefix(b"DPAPI".as_slice())
            .context("加密密钥格式不符合预期")?;
        unprotect(wrapped)
    }

    #[cfg(windows)]
    fn unprotect(blob: &[u8]) -> Result<Vec<u8>> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

        let input = CRYPT_INTEGER_BLOB {
            cbData: blob.len() as u32,
            pbData: blob.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        // SAFETY: `input` points at a live slice for the duration of the call;
        // on success Windows allocates `output.pbData`, which we copy out of
        // and then hand back to LocalFree.
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                &mut output,
            )
        };
        if ok == 0 {
            bail!(
                "解密浏览器密钥失败（DPAPI）。\n\
                 该密钥只能由当前 Windows 用户解密——请确认这是同一个用户账户。"
            );
        }
        // SAFETY: CryptUnprotectData succeeded, so pbData holds cbData bytes.
        let key =
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
        // SAFETY: pbData was allocated by CryptUnprotectData.
        unsafe { LocalFree(output.pbData as *mut std::ffi::c_void) };
        Ok(key)
    }

    #[cfg(not(windows))]
    fn unprotect(_blob: &[u8]) -> Result<Vec<u8>> {
        bail!(
            "该平台上的 Chromium 系浏览器需要系统钥匙串才能解密 Cookie，暂不支持。\n\
             请改用 Firefox 导入，或选择「手动粘贴 Cookie」。"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_host_matching() {
        assert!(is_youtube_host(".youtube.com"));
        assert!(is_youtube_host("youtube.com"));
        assert!(is_youtube_host("music.youtube.com"));
        assert!(is_youtube_host(".music.youtube.com"));
        // Look-alikes must never match.
        assert!(!is_youtube_host("google.com"));
        assert!(!is_youtube_host("notyoutube.com"));
        assert!(!is_youtube_host("youtube.com.evil.net"));
    }

    #[test]
    fn stale_cleanup_only_removes_matching_directories() {
        let root = tempfile::tempdir().unwrap();
        let stale = root.path().join("ytbm-cookie-123-0123456789abcdef");
        let new_stale = root.path().join("nakuru-cookie-123-0123456789abcdef");
        let unrelated = root.path().join("ytbm-cookie-unrelated");
        std::fs::create_dir(&stale).unwrap();
        std::fs::create_dir(&new_stale).unwrap();
        std::fs::create_dir(&unrelated).unwrap();
        assert_eq!(
            cleanup_stale_scratch_in(root.path(), Duration::from_secs(3600)),
            0
        );
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(cleanup_stale_scratch_in(root.path(), Duration::ZERO), 2);
        assert!(!stale.exists());
        assert!(!new_stale.exists());
        assert!(unrelated.exists());
    }
}
