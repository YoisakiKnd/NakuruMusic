use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::app::App;

use super::{badge, fit_w, track_spans, visible_start, BadgeKind, TrackCols, ACCENT, BADGE_W, DIM};

/// Charts + new releases as one selectable list with section headers.
/// Returns the list hit area + scroll offset for mouse support.
pub fn draw(f: &mut Frame, app: &App, area: Rect) -> Option<(Rect, usize)> {
    let home = &app.home;
    if home.loading && home.row_count() == 0 {
        centered_hint(f, area, "加载推荐内容中..");
        return None;
    }
    if home.row_count() == 0 {
        centered_hint(
            f,
            area,
            "推荐内容加载失败\n\ng 重试   / 搜索   L 音乐库   ? 帮助",
        );
        return None;
    }

    // The rank prefix eats 3 columns before the shared track layout starts.
    let cols = TrackCols::new(area.width.saturating_sub(3));
    // Map the logical selection onto the physical list (skip headers).
    let sel_item = if home.selected < home.tracks.len() {
        1 + home.selected
    } else {
        2 + home.tracks.len() + (home.selected - home.tracks.len())
    };
    let physical_len = home.row_count() + 2;
    let start = visible_start(physical_len, sel_item, area.height);
    let items: Vec<ListItem> = (start..(start + area.height as usize).min(physical_len))
        .map(|row| {
            if row == 0 {
                return section_header("热门歌曲", area.width);
            }
            if row == home.tracks.len() + 1 {
                return section_header("新专辑", area.width);
            }
            if row <= home.tracks.len() {
                let i = row - 1;
                let mut line = vec![Span::styled(
                    format!("{:>2} ", i + 1),
                    Style::default().fg(DIM),
                )];
                line.extend(track_spans(&home.tracks[i], cols));
                return ListItem::new(Line::from(line));
            }
            let a = &home.albums[row - home.tracks.len() - 2];
            let year = a.year.map(|y| format!("{y}")).unwrap_or_default();
            ListItem::new(Line::from(vec![
                badge(BadgeKind::Album),
                Span::raw(fit_w(&a.title, cols.title.saturating_sub(BADGE_W))),
                Span::styled(
                    format!(" {}", fit_w(&a.artists, cols.artist)),
                    Style::default().fg(DIM),
                ),
                Span::styled(format!(" {year:>5}"), Style::default().fg(DIM)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )
        .highlight_symbol("> ");
    let mut state = ListState::default();
    state.select(Some(sel_item - start));
    f.render_stateful_widget(list, area, &mut state);
    Some((area, start))
}

/// A bold label followed by a rule, so sections read as sections without
/// relying on emoji that terminals size differently.
fn section_header(title: &str, width: u16) -> ListItem<'static> {
    use unicode_width::UnicodeWidthStr;
    let rule_w = (width as usize).saturating_sub(title.width() + 3);
    ListItem::new(Line::from(vec![
        Span::styled(
            title.to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {}", "-".repeat(rule_w)), Style::default().fg(DIM)),
    ]))
}

fn centered_hint(f: &mut Frame, area: Rect, text: &str) {
    f.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .style(Style::default().fg(DIM)),
        area,
    );
}
