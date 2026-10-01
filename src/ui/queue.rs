use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::app::{App, Focus};

use super::{fit_w, pane_border, visible_start, ACCENT, DIM};

/// Returns the list hit area + scroll offset for mouse support.
pub fn draw(f: &mut Frame, app: &App, area: Rect) -> Option<(Rect, usize)> {
    let focused = app.focus == Focus::Queue;
    let title = format!(" 队列 {} ", app.queue.len());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(pane_border(focused))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.queue.is_empty() {
        f.render_widget(
            Paragraph::new("队列为空\n\n搜索后 Enter 播放\na 加入队列")
                .style(Style::default().fg(DIM)),
            inner,
        );
        return None;
    }

    let current = app.queue.current_index();
    // Marker takes 2 columns; split the rest between title and artist.
    let usable = (inner.width as usize).saturating_sub(2);
    let artist_w = if usable >= 22 {
        (usable / 3).clamp(6, 14)
    } else {
        0
    };
    let title_w = usable.saturating_sub(if artist_w > 0 { artist_w + 1 } else { 0 });

    let selected = if focused {
        app.queue_selected
    } else {
        current.unwrap_or(app.queue_selected)
    }
    .min(app.queue.len() - 1);
    let start = visible_start(app.queue.len(), selected, inner.height);
    let items: Vec<ListItem> = app
        .queue
        .items()
        .iter()
        .enumerate()
        .skip(start)
        .take(inner.height as usize)
        .map(|(i, t)| {
            let is_current = Some(i) == current;
            let marker = if is_current { "> " } else { "  " };
            let style = if is_current {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else if current.is_some_and(|c| i < c) {
                Style::default().fg(DIM) // history
            } else {
                Style::default()
            };
            let mut spans = vec![Span::styled(
                format!("{marker}{}", fit_w(&t.title, title_w)),
                style,
            )];
            if artist_w > 0 {
                spans.push(Span::styled(
                    format!(" {}", fit_w(&t.artists, artist_w)),
                    Style::default().fg(DIM),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    if focused {
        state.select(Some(selected - start));
    } else {
        // Keep the playing track visible when the pane is not focused.
        state.select(current.map(|_| selected - start));
    }
    f.render_stateful_widget(list, inner, &mut state);
    Some((inner, start))
}
