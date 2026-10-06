# NakuruMusic

[![CI](https://github.com/YoisakiKnd/NakuruMusic/actions/workflows/ci.yml/badge.svg)](https://github.com/YoisakiKnd/NakuruMusic/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/YoisakiKnd/NakuruMusic)](https://github.com/YoisakiKnd/NakuruMusic/releases/latest)
[![License](https://img.shields.io/badge/license-GPL--3.0-blue)](LICENSE)

轻量级 YouTube Music 终端客户端（TUI）。默认使用内置音频后端，也可在设置页切换到 mpv。界面采用以藍月なくる为灵感的浅蓝主题与渐变进度条；本项目为非官方作品。

```
 首页 Esc  搜索 /  音乐库 L  历史 H  正在播放 l  设置 ,   ? 帮助  q 退出
┌ 首页 ───────────────────────────────────────┬ 队列 12 ─────────┐
│ 热门歌曲 ----------------------------------- │ > 晴天    周杰伦  │
│ >  1 Golden              HUNTR/X       3:20 │   七里香  周杰伦  │
│    2 Soda Pop            Saja Boys     2:46 │   稻香    周杰伦  │
│ 新专辑 ------------------------------------- │                  │
│    ALB The Life of a Sh.. Taylor Swift 2025 │                  │
├─────────────────────────────────────────────┴──────────────────┤
│ >  晴天 - 周杰伦    1:23 ----●--------- 4:29  vol 70%  RPT RADIO│
└────────────────────────────────────────────────────────────────┘
```

顶部导航栏显示当前位置与到达各页面的按键，可直接点击切换。

## 下载

到 [Releases](https://github.com/YoisakiKnd/NakuruMusic/releases/latest) 下载对应平台的
压缩包（Windows / Linux / macOS Intel / macOS Apple Silicon），解压即用。

## 特性

- **无广告播放**：程序自己解析音频直链（进程内完成，无需 yt-dlp / deno / node），
  流中天然不含广告（广告由官方客户端注入）
- **浏览器一键登录**：自动检测已装浏览器，选中即导入登录身份，无需复制粘贴；登录后可访问 喜欢的音乐 / 我的歌单 / 收藏专辑 / 关注歌手 / 播放历史，凭据只存本机
- **首页推荐**：打开即见排行榜热门歌曲与新发行专辑
- **SponsorBlock**：自动跳过 MV 音源中的非音乐片段（口播/片头/片尾），社区数据驱动
- **播放详情页**（按 `l`）：专辑封面 + 曲目信息 + 同步歌词逐行滚动。封面自动选用
  终端支持的最佳图形协议（kitty / iTerm2 / Sixel），都不支持时降级为半块字符——
  任何终端都能看到图
- **同步歌词**：LRCLIB 逐行滚动高亮，无同步歌词时回退 YT Music 文本歌词
- **电台续播**：队列快播完时自动追加相似歌曲（可开关）
- **完整队列**：插播/下一首/移除/排序/打乱，循环三模式（关/全部/单曲）
- **本地播放会话**：自动保存队列、循环模式和播放位置，重启后可继续上次播放；不保存带凭据的直链
- **本地播放历史**：保存最近 100 首歌曲，支持从历史页面直接重新播放，不依赖登录或网络
- **鼠标支持**：点击选中、双击播放、滚轮滚动、点击进度条跳转、点击分类页签

## 安装

下载对应平台压缩包，解压后运行。首次启动会生成配置文件，默认使用内置播放器，
无需 mpv 或 yt-dlp。

Windows 可使用 Scoop：

```powershell
scoop bucket add yoisaki https://github.com/YoisakiKnd/scoop-bucket
scoop install yoisaki/nakuru-music
nakuru-music
```

macOS 可使用 Homebrew tap：

```sh
brew tap YoisakiKnd/tap
brew install nakuru-music
nakuru-music
```

按 `,` 打开设置页，选中 mpv 并按 Enter，可在运行时切换。mpv 需预先安装：

```powershell
scoop install mpv
```

内置 MP4/AAC 播放器先预缓冲约 1 MiB 压缩音频，
再从匿名临时文件边下载边解码；单曲临时文件上限为 512 MiB。
网络慢时播放会等待后续数据；快进到尚未下载的位置会取消当前分段并优先请求目标分段，仍需等待网络响应。切歌或退出后
临时文件会关闭并删除。对带完整片段索引的 MP4，三小时线上 AAC 已实测快速启动、跳到两小时处及跳回开头；
已在多首真实歌曲上完成整曲下载和 AAC 解码，也在 macOS 真实设备完成播放控制、两首线上歌曲连续自然结束及两小时本地 AAC 连续播放测试。
部分地址仍会偶发 HTTP 403，程序会限次重新解析地址；跨平台验收和更多长音频封装测试仍在进行。

> 播放地址由程序自己解析，不需要 yt-dlp，也不需要 yt-dlp 所要求的
> JS 运行时（deno / node）。
>
> yt-dlp 是**可选**的：仅在 mpv 模式下程序自身解析播放地址失败时作为兜底。
> 浏览器登录导入不依赖 yt-dlp；若 Chromium 因加密限制无法导入，改用
> Firefox 或手动粘贴 Cookie。
>
> 程序会自动在 PATH、scoop、Program Files 中寻找 mpv，装完不必重开终端。

从源码构建（需要 [Rust 工具链](https://rustup.rs/)）：

```powershell
cargo build --release
# 产物: target\release\nakuru-music.exe，单文件可任意拷贝
```

## 开发

```powershell
cargo test                 # 纯逻辑 + 渲染测试，不联网
cargo test -- --ignored    # 冒烟测试，会真实请求 YouTube/LRCLIB/SponsorBlock
cargo clippy --all-targets -- -D warnings
```

CI（`.github/workflows/ci.yml`）在三大平台跑 fmt + clippy + test + release 构建；
推送 `v*` 标签会触发 `release.yml` 交叉构建四个平台产物并自动创建 GitHub Release。
项目约定见 [CLAUDE.md](CLAUDE.md)。

实机测试时可设置 `NAKURU_MUSIC_DATA_ROOT` 为绝对路径，将配置、缓存和会话文件写入该目录，
便于使用独立测试资料且不改动日常使用的配置。
旧版的 `YTBM_DATA_ROOT` 仍可使用；升级后会继续读取旧版数据目录，不迁移或删除登录与历史数据。

## 登录（访问个人音乐库）

按 `L` 打开登录页，程序会自动列出本机已安装的浏览器：

```
[导入] 从 Firefox / Waterfox 导入登录
[导入] 从 Vivaldi (…\persist\vivaldi\User Data) 导入登录
[网页] 先在浏览器登录 music.youtube.com（打开网页）
[手动] 手动粘贴 Cookie 或 cookies.txt 路径
```

**一键登录**：确保该浏览器里已登录 YouTube，选中它按 Enter 即可——程序会直接读取
浏览器本机的 Cookie 数据库，全程离线、无需复制粘贴，也不依赖 yt-dlp。浏览器还没登录的话，
先选「打开网页」登录，回来再导入。

失败时的常见原因：**Chrome 127+ 启用了 App-Bound Encryption，把密钥绑定到
Chrome 自身进程，外部程序无法解密**。Firefox 和 Waterfox 的 Cookie 库是明文的，可以正常导入。
读不出来就用「手动粘贴」兜底（浏览器扩展「Get cookies.txt LOCALLY」
导出后粘贴文件路径即可）。

登录信息保存在本机数据目录，重启免登录；播放会话和本地播放历史也保存在同一数据目录：

- `playback-session.json`：队列、当前曲目、播放位置和播放模式
- `playback-history.json`：最近 100 首本地播放记录

上述文件不保存带凭据的播放直链。音乐库页按 `x` 登出。
注意 YouTube 会轮换 Cookie，若长时间后接口报错，重新导入一次即可。

## 快捷键

| 键 | 功能 |
|---|---|
| `/` | 搜索 |
| `L` | 音乐库（登录入口） |
| `H` | 本地播放历史 |
| `,` | 设置 / 切换播放器 |
| `1-4` / `[` `]` | 搜索分类切换（歌曲/专辑/歌手/歌单） |
| `j/k` `↑/↓` `PgUp/PgDn` `g/G` | 列表导航 |
| `Enter` | 播放（当前列表从这首起顺序播放）/ 打开；历史页重新播放选中歌曲 |
| `a` / `A` | 加入队列 / 作为下一首 |
| `P` | 整页（专辑/歌单/列表）从头播放 |
| `Tab` | 主面板 ⇄ 队列 |
| `x` | 队列：移除所选 · 音乐库：登出 |
| `J/K` | 队列内下移 / 上移 |
| `Space` / `m` | 暂停 / 静音 |
| `n` / `p` | 下一首 / 上一首 |
| `←` / `→` | 快退 / 快进 5s |
| `-` / `=` | 音量 ∓5% |
| `r` / `t` / `s` | 循环模式 / 电台续播 / 打乱 |
| `l` | 播放详情页（封面 + 歌词） |
| `?` | 帮助 |
| `q` | 退出 |

鼠标：单击选中 · 双击播放/打开 · 滚轮滚动 · 点击进度条跳转 · 点击搜索页签切分类。
（终端内选择文本请用 Shift+拖动）

## 配置

首次运行自动生成 `%APPDATA%\NakuruMusic\config\config.toml`；旧版安装沿用 `%APPDATA%\ytbm-tui`：

```toml
[playback]
engine = "native"    # 默认内置播放器；也可在设置页切换 mpv
mpv_path = "mpv"     # 自动发现失败时可写绝对路径
volume = 70
radio_auto = true

[sponsorblock]
enabled = true
categories = ["sponsor", "selfpromo", "interaction", "music_offtopic"]

[lyrics]
enabled = true

[keys]           # 全局键位可重映射（列表内导航键固定）
# next = "b"     # 动作名: quit/search/library/help/focus/play_pause/mute/next/
# vol_up = "]"   #   prev/seek_back/seek_fwd/vol_down/vol_up/repeat/radio/
                 #   shuffle/lyrics/restart_player/history/settings
```

日志：`%APPDATA%\NakuruMusic\data\nakuru-music.log`（旧安装仍使用旧数据目录）。

## 故障排查

- **播放一直"解析中"**：检查网络是否可访问 YouTube；内置模式可能正在等待预缓冲或音频元数据
- **播放中断或无法播放**：失败时保留当前曲目，按 `R` 重试，或按 `n` 手动播放下一首；若某首歌曲持续失败，记录曲目 ID 与日志，参见 [更新计划](UPDATE_PLAN.md) 的线上整曲验收状态
- **yt-dlp 警告缺少 JS 运行时**：安装 deno，或已有 node 时在 `yt-dlp.conf`
  中加一行 `--js-runtimes node`
- **提示未检测到 mpv**：`scoop install mpv`；程序会自动搜 scoop/winget 安装位置
- **mpv 崩溃**：界面会提示，按 `R` 重启播放器进程，队列不丢失
- **浏览器导入失败**：Chrome 系需完全关闭浏览器后重试；Firefox 无此限制；
  或改用「手动粘贴 Cookie」
- **登录后接口报错**：Cookie 已被 YouTube 轮换失效，重新导入一次即可

## 合规说明

本项目为个人学习/研究用途，使用非官方公开接口；不绕过任何 DRM，
内置播放模式仅使用播放期间存在的匿名临时文件，不内置或分发 YouTube 内容。登录 Cookie 仅保存在本机、
仅用于向 YouTube 发起请求。使用产生的流量及 YouTube 服务条款相关风险由使用者自行承担。

## License

GPL-3.0-only（依赖 [rustypipe](https://codeberg.org/ThetaDev/rustypipe)，其为 GPL-3.0 协议）
