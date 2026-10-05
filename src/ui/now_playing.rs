//! Full-page player: album art, track metadata, progress and lyrics.
//!
//! The cover is drawn with whatever graphics protocol the terminal
//! supports (kitty / iTerm2 / sixel), falling back to half-block characters,
//! which work anywhere. When no cover is available the lyrics simply take
//! the whole page.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;
use ratatui_image::{Resize, StatefulImage};

use crate::api::models::format_duration;
use crate::app::App;
use crate::lyrics::{current_line, LyricsData};

use super::{truncate_w, ACCENT, DIM};

/// Cover art is square; a terminal cell is roughly twice as tall as wide,
/// so a square of N columns needs about N/2 rows.
const COVER_COLS: u16 = 34;

pub fn draw(f: &mut Frame, app: &mut App, area: Rect) {
    if app.now_playing.is_none() {
        f.render_widget(
            Paragraph::new("没有正在播放的曲目\n\n按 Esc 返回，/ 搜索")
                .alignment(Alignment::Center)
                .style(Style::default().fg(DIM)),
            area,
        );
        return;
    }

    // Side-by-side when there is room, stacked otherwise.
    let wide = area.width >= COVER_COLS + 40;
    let has_cover = app.cover.protocol.is_some() || app.cover.loading;

    if wide && has_cover {
        let [left, right] =
            Layout::horizontal([Constraint::Length(COVER_COLS), Constraint::Min(30)]).areas(area);
        // Reserve a pixel-square block, capped so metadata still fits.
        let cols = COVER_COLS.min(left.width);
        let rows = app.cover_rows(cols).min(left.height.saturating_sub(4));
        let [cover_a, meta_a] =
            Layout::vertical([Constraint::Length(rows), Constraint::Min(4)]).areas(left);
        draw_cover(f, app, cover_a, cols);
        draw_meta(f, app, meta_a);
        draw_lyrics(f, app, right);
    } else {
        // Stacked: the cover may not take more than half the page.
        let cols = COVER_COLS.min(area.width);
        let rows = if has_cover {
            app.cover_rows(cols).min(area.height / 2)
        } else {
            0
        };
        let [cover_a, meta_a, lyrics_a] = Layout::vertical([
            Constraint::Length(rows),
            Constraint::Length(5),
            Constraint::Min(3),
        ])
        .areas(area);
        if has_cover {
            draw_cover(f, app, cover_a, cols);
        }
        draw_meta(f, app, meta_a);
        draw_lyrics(f, app, lyrics_a);
    }
}

fn draw_cover(f: &mut Frame, app: &mut App, area: Rect, cols: u16) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    // Keep the block square in *pixels*: the rows were derived from the
    // font ratio, so clamp the width to match rather than letting the image
    // stretch across the pane.
    let area = Rect {
        width: cols.min(area.width),
        ..area
    };
    match app.cover.protocol.as_mut() {
        Some(protocol) => {
            // Fit keeps the aspect ratio; the widget re-encodes on resize.
            let widget = StatefulImage::<ratatui_image::protocol::StatefulProtocol>::new()
                .resize(Resize::Fit(None));
            f.render_stateful_widget(widget, area, protocol);
        }
        None => {
            f.render_widget(
                Paragraph::new("\n封面加载中..")
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(DIM)),
                area,
            );
        }
    }
}

fn draw_meta(f: &mut Frame, app: &App, area: Rect) {
    let Some(t) = app.now_playing.as_ref() else {
        return;
    };
    let w = area.width as usize;
    let mut lines = vec![
        Line::from(Span::styled(
            truncate_w(&t.title, w),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::raw(truncate_w(&t.artists, w))),
    ];
    if let Some(album) = &t.album {
        lines.push(Line::from(Span::styled(
            truncate_w(album, w),
            Style::default().fg(DIM),
        )));
    }
    // Position within the queue, so the page says where you are.
    if let (Some(i), n) = (app.queue.current_index(), app.queue.len()) {
        lines.push(Line::from(Span::styled(
            format!("队列 {} / {}", i + 1, n),
            Style::default().fg(DIM),
        )));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_lyrics(f: &mut Frame, app: &App, area: Rect) {
    let [progress_a, lyrics_a] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(area);
    draw_progress(f, app, progress_a);

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(DIM));
    let inner = block.inner(lyrics_a);
    f.render_widget(block, lyrics_a);

    match &app.lyrics.data {
        LyricsData::None => {
            let text = if app.lyrics.loading {
                "歌词加载中.."
            } else {
                "暂无歌词"
            };
            f.render_widget(
                Paragraph::new(format!("\n{text}"))
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(DIM)),
                inner,
            );
        }
        LyricsData::Plain(text) => {
            f.render_widget(
                Paragraph::new(text.as_str())
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: false })
                    .scroll((app.lyrics.scroll, 0)),
                inner,
            );
        }
        LyricsData::Synced(lines) => {
            let cur = current_line(lines, app.pb.time_pos);
            let h = inner.height as usize;
            // Keep the active line near the middle of the pane.
            let start = cur.unwrap_or(0).saturating_sub(h / 2);
            let visible: Vec<Line> = lines
                .iter()
                .enumerate()
                .skip(start)
                .take(h)
                .map(|(i, (_, text))| {
                    let display = if text.is_empty() {
                        "~ ~ ~"
                    } else {
                        text.as_str()
                    };
                    let style = if Some(i) == cur {
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(DIM)
                    };
                    Line::from(display.to_string()).style(style)
                })
                .collect();
            f.render_widget(Paragraph::new(visible).alignment(Alignment::Center), inner);
        }
    }
}

fn draw_progress(f: &mut Frame, app: &App, area: Rect) {
    let pb = &app.pb;
    let elapsed = format_duration(pb.time_pos.max(0.0) as u32);
    let total = if pb.duration > 0.0 {
        format_duration(pb.duration as u32)
    } else {
        "--:--".to_string()
    };
    let label = format!(" {elapsed} / {total} ");
    let [gauge_a, time_a] = Layout::horizontal([
        Constraint::Min(10),
        Constraint::Length(label.chars().count() as u16),
    ])
    .areas(Rect { height: 1, ..area });

    let ratio = if pb.duration > 0.0 {
        (pb.time_pos / pb.duration).clamp(0.0, 1.0)
    } else {
        0.0
    };
    super::progress::draw(f, gauge_a, ratio);
    f.render_widget(
        Paragraph::new(label).style(Style::default().fg(DIM)),
        time_a,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use ratatui_image::picker::Picker;
    use ratatui_image::{Resize, StatefulImage};

    /// Half-blocks are the universal fallback - prove a real image actually
    /// paints coloured cells into the buffer at that tier, since that is
    /// what most terminals (including plain Windows Terminal) will use.
    #[test]
    fn halfblock_cover_paints_colored_cells() {
        // A red/blue checkerboard so the output is unmistakably the image.
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(64, 64, |x, y| {
            if (x / 8 + y / 8) % 2 == 0 {
                image::Rgb([220, 30, 30])
            } else {
                image::Rgb([30, 30, 220])
            }
        }));

        let picker = Picker::halfblocks();
        let mut protocol = picker.new_resize_protocol(img);

        let mut terminal = Terminal::new(TestBackend::new(20, 10)).unwrap();
        terminal
            .draw(|f| {
                let widget = StatefulImage::<ratatui_image::protocol::StatefulProtocol>::new()
                    .resize(Resize::Fit(None));
                f.render_stateful_widget(widget, f.area(), &mut protocol);
            })
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let colored = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .filter(|(x, y)| {
                let cell = &buf[(*x, *y)];
                cell.fg != ratatui::style::Color::Reset || cell.bg != ratatui::style::Color::Reset
            })
            .count();
        println!("colored cells: {colored} / {}", buf.area.area());
        assert!(
            colored > 20,
            "cover did not render - only {colored} cells were painted"
        );
    }
}
