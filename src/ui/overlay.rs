use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{Action, App};

use super::{fit_w, truncate_w, ACCENT, DIM};

const KEYS: &[(&str, &str)] = &[
    ("/", "搜索"),
    ("L", "音乐库 / 登录 (浏览器一键导入)"),
    ("H", "本地播放历史"),
    (",", "设置 / 切换播放后端"),
    ("1-4 / [ ]", "搜索结果分类切换"),
    ("j/k Up/Down", "移动选择"),
    ("g/G PgUp/Dn", "跳顶/跳底/翻页"),
    ("Enter", "播放 / 打开"),
    ("a / A", "加入队列 / 下一首播放"),
    ("P", "整页加入队列并播放"),
    ("Tab", "主面板 / 队列"),
    ("x", "队列移除 / 音乐库登出"),
    ("J/K", "队列中下移/上移"),
    ("Space", "暂停 / 继续"),
    ("n / p", "下一首 / 上一首"),
    ("Left / Right", "快退 / 快进 5s"),
    ("- / =", "音量 -/+ 5%"),
    ("m", "静音"),
    ("r", "循环模式 关/全部/单曲"),
    ("t", "电台自动续播开关"),
    ("s", "打乱待播队列"),
    ("l", "正在播放（封面 + 歌词）"),
    ("R", "重启播放器"),
    ("Esc", "返回 / 关闭"),
    ("q", "退出"),
];

pub fn draw_help(f: &mut Frame, app: &App) {
    let keys: Vec<(String, &str)> = KEYS
        .iter()
        .map(|(key, desc)| {
            let actual = match *key {
                "/" => app.key_label(Action::Search),
                "L" => app.key_label(Action::Library),
                "H" => app.key_label(Action::History),
                "," => app.key_label(Action::Settings),
                "Tab" => app.key_label(Action::FocusToggle),
                "Space" => app.key_label(Action::PlayPause),
                "n / p" => format!(
                    "{} / {}",
                    app.key_label(Action::NextTrack),
                    app.key_label(Action::PrevTrack)
                ),
                "Left / Right" => format!(
                    "{} / {}",
                    app.key_label(Action::SeekBack),
                    app.key_label(Action::SeekFwd)
                ),
                "- / =" => format!(
                    "{} / {}",
                    app.key_label(Action::VolDown),
                    app.key_label(Action::VolUp)
                ),
                "m" => app.key_label(Action::Mute),
                "r" => app.key_label(Action::RepeatCycle),
                "t" => app.key_label(Action::RadioToggle),
                "s" => app.key_label(Action::Shuffle),
                "l" => app.key_label(Action::LyricsToggle),
                "R" => app.key_label(Action::RestartPlayer),
                "q" => app.key_label(Action::Quit),
                _ => (*key).into(),
            };
            (actual, *desc)
        })
        .collect();
    let area = centered(
        f.area(),
        52,
        (keys.len() as u16 + 2).min(f.area().height.saturating_sub(2)),
    );
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(" 快捷键 (Up/Down 滚动, Esc 关闭) ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let lines: Vec<Line> = keys
        .iter()
        .map(|(key, desc)| {
            Line::from(vec![
                Span::styled(
                    format!("  {}", fit_w(key, 15)),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    truncate_w(desc, inner.width.saturating_sub(17) as usize),
                    Style::default(),
                ),
            ])
        })
        .collect();
    let max_scroll = lines.len().saturating_sub(inner.height as usize);
    let scroll = (app.help_scroll as usize).min(max_scroll) as u16;
    f.render_widget(
        Paragraph::new(lines)
            .scroll((scroll, 0))
            .style(Style::default().fg(DIM)),
        inner,
    );
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let [mid_v] = Layout::vertical([Constraint::Length(h.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    let [mid] = Layout::horizontal([Constraint::Length(w.min(area.width))])
        .flex(Flex::Center)
        .areas(mid_v);
    mid
}
