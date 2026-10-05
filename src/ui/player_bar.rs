use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::player::queue::RepeatMode;

use super::{progress, state_marker, DIM};

fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// Returns the progress-gauge area for click-to-seek.
pub fn draw(f: &mut Frame, app: &App, area: Rect) -> Option<Rect> {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let pb = &app.pb;

    // Right-side mode flags, always visible.
    let mut flags: Vec<&str> = Vec::new();
    match app.queue.repeat {
        RepeatMode::All => flags.push("RPT"),
        RepeatMode::One => flags.push("RPT1"),
        RepeatMode::Off => {}
    }
    if app.radio_on {
        flags.push("RADIO");
    }
    let flags_text = if flags.is_empty() {
        String::new()
    } else {
        format!("  {}", flags.join(" "))
    };
    let vol_display = if pb.muted {
        " vol MUTE".to_string()
    } else {
        format!(" vol {:>3}%", pb.volume)
    };
    let vol_text = format!("{vol_display}{flags_text}");

    let Some(title) = &pb.current_title else {
        let idle = if pb.alive {
            "-  暂无播放"
        } else {
            "!  播放器未运行 (R 重启)"
        };
        let [idle_a, vol_a] = Layout::horizontal([
            Constraint::Min(10),
            Constraint::Length(vol_text.chars().count() as u16 + 2),
        ])
        .areas(inner);
        f.render_widget(Paragraph::new(idle).style(Style::default().fg(DIM)), idle_a);
        f.render_widget(Paragraph::new(vol_text), vol_a);
        return None;
    };

    if !pb.alive {
        f.render_widget(
            Paragraph::new(format!("!  播放器未运行 (R 重启)  {title}"))
                .style(Style::default().fg(DIM)),
            inner,
        );
        return None;
    }

    let marker = state_marker(pb.loading, pb.paused);
    let title_text = if pb.loading {
        let hint = if pb.loading_secs >= 15.0 {
            " (网络较慢或解析失败,稍候)"
        } else {
            ""
        };
        format!("{marker} 解析中 {:.0}s{hint}  {title}", pb.loading_secs)
    } else {
        format!("{marker} {title}")
    };
    let time_text = format!(" {} / {} ", fmt_time(pb.time_pos), fmt_time(pb.duration));

    let title_w = (inner.width / 2).min(46);
    let [title_a, time_a, gauge_a, vol_a] = Layout::horizontal([
        Constraint::Length(title_w),
        Constraint::Length(time_text.chars().count() as u16),
        Constraint::Min(8),
        Constraint::Length(vol_text.chars().count() as u16 + 2),
    ])
    .areas(inner);

    f.render_widget(
        Paragraph::new(title_text).style(Style::default().add_modifier(Modifier::BOLD)),
        title_a,
    );
    f.render_widget(Paragraph::new(time_text), time_a);

    let ratio = if pb.duration > 0.0 {
        (pb.time_pos / pb.duration).clamp(0.0, 1.0)
    } else {
        0.0
    };
    progress::draw(f, gauge_a, ratio);

    f.render_widget(Paragraph::new(vol_text), vol_a);
    Some(gauge_a)
}
