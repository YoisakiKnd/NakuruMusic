use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::app::App;

use super::{pane_border, track_spans, visible_start, TrackCols, ACCENT, DIM};

pub fn draw(f: &mut Frame, app: &App, area: Rect) -> Option<(Rect, usize)> {
    if app.history.entries().is_empty() {
        f.render_widget(
            Paragraph::new("暂无本地播放历史\n\n播放歌曲后会自动记录")
                .style(Style::default().fg(DIM)),
            area,
        );
        return None;
    }
    let block = ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_style(pane_border(true))
        .title(" 播放历史 ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let cols = TrackCols::new(inner.width);
    let selected = app.history_selected.min(app.history.entries().len() - 1);
    let start = visible_start(app.history.entries().len(), selected, inner.height);
    let items = app
        .history
        .entries()
        .iter()
        .skip(start)
        .take(inner.height as usize)
        .map(|entry| ListItem::new(Line::from(track_spans(&entry.track, cols))))
        .collect::<Vec<_>>();
    let list = List::new(items)
        .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let mut state = ListState::default();
    state.select(Some(selected - start));
    f.render_stateful_widget(list, inner, &mut state);
    Some((inner, start))
}
