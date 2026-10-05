use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::App;
use crate::config::PlaybackEngine;

use super::{ACCENT, DIM};

pub fn draw(f: &mut Frame, app: &App, area: Rect) -> [Option<Rect>; 2] {
    let current = app.config.playback.engine;
    let options = [
        (PlaybackEngine::Native, "内置播放器", "无需外部程序"),
        (PlaybackEngine::Mpv, "mpv", "需要本机安装 mpv"),
    ];
    let mut lines = vec![Line::raw("播放后端"), Line::raw("")];
    for (i, (engine, label, detail)) in options.iter().enumerate() {
        let selected = i == app.settings_selected;
        let active = *engine == current;
        let marker = if selected { ">" } else { " " };
        let state = if active { " [当前]" } else { "" };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {marker} {}. {label}{state}", i + 1),
                if selected {
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            ),
            Span::styled(format!("  {detail}"), Style::default().fg(DIM)),
        ]));
    }
    lines.extend([
        Line::raw(""),
        Line::styled(
            "Up/Down 选择  Enter 应用  Esc 返回",
            Style::default().fg(DIM),
        ),
    ]);
    if app.engine_switch_pending {
        lines.push(Line::styled(
            "正在检查播放器..",
            Style::default().fg(ACCENT),
        ));
    }
    f.render_widget(Paragraph::new(lines), area);
    std::array::from_fn(|i| {
        (area.height > i as u16 + 2).then_some(Rect::new(
            area.x,
            area.y + i as u16 + 2,
            area.width,
            1,
        ))
    })
}
