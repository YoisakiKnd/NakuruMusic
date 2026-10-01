use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::api::models::Track;
use crate::app::{App, BrowsePage};

use super::{
    badge, fit_w, track_item, truncate_w, visible_start, BadgeKind, TrackCols, ACCENT, BADGE_W, DIM,
};

/// Returns the list hit area + scroll offset for mouse support.
pub fn draw(f: &mut Frame, app: &App, area: Rect) -> Option<(Rect, usize)> {
    let Some(page) = app.browse_stack.last() else {
        f.render_widget(
            Paragraph::new("（空）").style(Style::default().fg(DIM)),
            area,
        );
        return None;
    };

    let [head_a, list_a] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(area);

    let mut hit = None;
    match page {
        BrowsePage::Album {
            title,
            data,
            selected,
        } => {
            // Prefer the authoritative name from the API response.
            let title = data.as_ref().map_or(title.as_str(), |d| d.title.as_str());
            let sub = data
                .as_ref()
                .map(|d| {
                    let year = d.year.map(|y| format!(" - {y}")).unwrap_or_default();
                    format!("{}{year} - {} 首", d.artists, d.tracks.len())
                })
                .unwrap_or_else(|| "加载中..".into());
            draw_head(f, head_a, &format!("专辑  {title}"), &sub);
            if let Some(d) = data {
                hit = draw_track_list(f, list_a, &d.tracks, *selected);
            }
        }
        BrowsePage::Playlist {
            title,
            data,
            selected,
        } => {
            let title = data.as_ref().map_or(title.as_str(), |d| d.title.as_str());
            let sub = data
                .as_ref()
                .map(|d| format!("{} 首", d.tracks.len()))
                .unwrap_or_else(|| "加载中..".into());
            draw_head(f, head_a, &format!("歌单  {title}"), &sub);
            if let Some(d) = data {
                hit = draw_track_list(f, list_a, &d.tracks, *selected);
            }
        }
        BrowsePage::Artist {
            name,
            data,
            selected,
        } => {
            let name = data.as_ref().map_or(name.as_str(), |d| d.name.as_str());
            let sub = data
                .as_ref()
                .map(|d| {
                    format!(
                        "热门 {} 首 - 专辑 {} 张",
                        d.top_tracks.len(),
                        d.albums.len()
                    )
                })
                .unwrap_or_else(|| "加载中..".into());
            draw_head(f, head_a, &format!("歌手  {name}"), &sub);
            if let Some(d) = data {
                let cols = TrackCols::new(list_a.width);
                let items =
                    d.top_tracks
                        .iter()
                        .map(|t| track_item(t, cols))
                        .chain(d.albums.iter().map(|a| {
                            let year = a.year.map(|y| format!("{y}")).unwrap_or_default();
                            ListItem::new(Line::from(vec![
                                badge(BadgeKind::Album),
                                Span::raw(fit_w(&a.title, cols.title.saturating_sub(BADGE_W))),
                                Span::styled(
                                    format!(" {:width$}", "", width = cols.artist),
                                    Style::default().fg(DIM),
                                ),
                                Span::styled(format!(" {year:>5}"), Style::default().fg(DIM)),
                            ]))
                        }));
                hit = render_list(
                    f,
                    list_a,
                    d.top_tracks.len() + d.albums.len(),
                    items,
                    *selected,
                );
            }
        }
        BrowsePage::Tracks {
            title,
            tracks,
            selected,
        } => {
            draw_head(f, head_a, title, &format!("{} 首", tracks.len()));
            hit = draw_track_list(f, list_a, tracks, *selected);
        }
        BrowsePage::Albums {
            title,
            items,
            selected,
        } => {
            draw_head(f, head_a, title, &format!("{} 张专辑", items.len()));
            let cols = TrackCols::new(list_a.width);
            let rows = items.iter().map(|a| {
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
            });
            hit = render_list(f, list_a, items.len(), rows, *selected);
        }
        BrowsePage::Artists {
            title,
            items,
            selected,
        } => {
            draw_head(f, head_a, title, &format!("{} 位歌手", items.len()));
            let w = (list_a.width as usize).saturating_sub(BADGE_W + 2);
            let rows = items.iter().map(|a| {
                ListItem::new(Line::from(vec![
                    badge(BadgeKind::Artist),
                    Span::raw(truncate_w(&a.name, w)),
                ]))
            });
            hit = render_list(f, list_a, items.len(), rows, *selected);
        }
        BrowsePage::Playlists {
            title,
            items,
            selected,
        } => {
            draw_head(f, head_a, title, &format!("{} 个歌单", items.len()));
            let cols = TrackCols::new(list_a.width);
            let rows = items.iter().map(|p| {
                let count = p.track_count.map(|n| format!("{n} 首")).unwrap_or_default();
                ListItem::new(Line::from(vec![
                    badge(BadgeKind::Playlist),
                    Span::raw(fit_w(&p.title, cols.title.saturating_sub(BADGE_W))),
                    Span::styled(
                        format!(" {count:>width$}", width = cols.artist),
                        Style::default().fg(DIM),
                    ),
                ]))
            });
            hit = render_list(f, list_a, items.len(), rows, *selected);
        }
    }
    hit
}

fn draw_head(f: &mut Frame, area: Rect, title: &str, sub: &str) {
    let lines = vec![
        Line::from(Span::styled(
            title.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(sub.to_string(), Style::default().fg(DIM))),
    ];
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_track_list(
    f: &mut Frame,
    area: Rect,
    tracks: &[Track],
    selected: usize,
) -> Option<(Rect, usize)> {
    let cols = TrackCols::new(area.width);
    let items = tracks.iter().map(|t| track_item(t, cols));
    render_list(f, area, tracks.len(), items, selected)
}

fn render_list(
    f: &mut Frame,
    area: Rect,
    len: usize,
    items: impl Iterator<Item = ListItem<'static>>,
    selected: usize,
) -> Option<(Rect, usize)> {
    if len == 0 {
        f.render_widget(
            Paragraph::new("（无内容）").style(Style::default().fg(DIM)),
            area,
        );
        return None;
    }
    let selected = selected.min(len - 1);
    let start = visible_start(len, selected, area.height);
    let visible: Vec<_> = items.skip(start).take(area.height as usize).collect();
    let list = List::new(visible)
        .highlight_style(
            Style::default()
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )
        .highlight_symbol("> ");
    let mut state = ListState::default();
    state.select(Some(selected - start));
    f.render_stateful_widget(list, area, &mut state);
    Some((area, start))
}
