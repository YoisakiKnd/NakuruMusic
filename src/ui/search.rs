use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Tabs};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::api::models::SearchKind;
use crate::app::App;

use super::{
    badge, fit_w, track_item, truncate_w, visible_start, BadgeKind, TrackCols, ACCENT, BADGE_W, DIM,
};

/// The one-line input editor (search query / login cookie), rendered by
/// mod.rs whenever input is active.
pub fn draw_input(f: &mut Frame, mode: crate::app::InputMode, buf: &str, area: Rect) {
    let prefix = match mode {
        crate::app::InputMode::Search => "/ ",
        crate::app::InputMode::Login => "Cookie> ",
    };
    let help = match mode {
        crate::app::InputMode::Search => "Enter 搜索  Esc 取消",
        crate::app::InputMode::Login => "Enter 提交  Esc 取消",
    };
    let help_width = help.width() as u16;
    let show_help = area.width >= help_width + 12;
    let edit_area = Rect {
        width: area
            .width
            .saturating_sub(if show_help { help_width + 1 } else { 0 }),
        ..area
    };
    if show_help {
        f.render_widget(
            Paragraph::new(help).style(Style::default().fg(DIM)),
            Rect {
                x: edit_area.right() + 1,
                width: help_width,
                ..area
            },
        );
    }
    if mode == crate::app::InputMode::Login {
        // Cookie values must never reach the terminal buffer or scrollback.
        let hint = format!(
            "已输入 {} 字符（内容隐藏，Enter 提交）",
            buf.chars().count()
        );
        let shown = truncate_w(
            &hint,
            (edit_area.width as usize).saturating_sub(prefix.len()),
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    prefix,
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::raw(shown),
            ])),
            edit_area,
        );
        return;
    }
    // Pasted cookies.txt content may be huge/multiline - show a sanitized tail.
    let shown: String = buf.replace('\n', "⏎");
    let max = (edit_area.width as usize).saturating_sub(prefix.len() + 2);
    let tail: String = if shown.width() > max {
        let mut acc = String::new();
        for c in shown.chars().rev() {
            acc.insert(0, c);
            if acc.width() >= max {
                break;
            }
        }
        format!("..{acc}")
    } else {
        shown
    };
    let line = Line::from(vec![
        Span::styled(
            prefix,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw(tail.clone()),
    ]);
    f.render_widget(Paragraph::new(line), edit_area);
    let cursor_x = edit_area.x + prefix.len() as u16 + tail.width() as u16;
    f.set_cursor_position((
        cursor_x.min(edit_area.right().saturating_sub(1)),
        edit_area.y,
    ));
}

/// Returns the actual tab hit areas and the list hit area for mouse support.
pub fn draw(f: &mut Frame, app: &App, area: Rect) -> ([Option<Rect>; 4], Option<(Rect, usize)>) {
    let [head_a, tabs_a, list_a] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(area);

    // Last submitted query (the live editor is drawn separately).
    let head = if app.search.query.is_empty() {
        Line::from(Span::styled(
            "按 / 输入搜索关键词",
            Style::default().fg(DIM),
        ))
    } else {
        Line::from(vec![
            Span::styled("搜索: ", Style::default().fg(DIM)),
            Span::styled(
                app.search.query.clone(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
        ])
    };
    f.render_widget(Paragraph::new(head), head_a);

    let titles: Vec<Line> = SearchKind::ALL
        .iter()
        .enumerate()
        .map(|(i, k)| Line::from(format!("{} {}", i + 1, k.label())))
        .collect();
    let tabs = Tabs::new(titles)
        .select(app.search.kind_idx)
        .divider("|")
        .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
        .style(Style::default().fg(DIM));
    f.render_widget(tabs, tabs_a);
    let mut tab_x = tabs_a.x;
    let tab_hits = std::array::from_fn(|i| {
        let width = (format!("{} {}", i + 1, SearchKind::ALL[i].label()).width()
            + 2
            + usize::from(i + 1 < SearchKind::ALL.len())) as u16;
        let visible = width.min(tabs_a.right().saturating_sub(tab_x));
        let rect = (visible > 0).then(|| Rect::new(tab_x, tabs_a.y, visible, 1));
        tab_x = tab_x.saturating_add(width);
        rect
    });

    if app.search.is_loading() {
        f.render_widget(
            Paragraph::new("搜索中..").style(Style::default().fg(DIM)),
            list_a,
        );
        return (tab_hits, None);
    }

    let len = app.search.list_len();
    if len == 0 {
        let text = if app.search.errors[app.search.kind_idx].is_some() {
            "搜索失败\n\ng 重试   / 重新搜索   1–4 切换分类"
        } else if app.search.query.is_empty() {
            ""
        } else {
            "无结果\n\n/ 重新搜索   1–4 切换分类"
        };
        f.render_widget(Paragraph::new(text).style(Style::default().fg(DIM)), list_a);
        return (tab_hits, None);
    }
    let start = visible_start(len, app.search.selected, list_a.height);
    let cols = TrackCols::new(list_a.width);
    let items: Vec<ListItem> = match app.search.kind() {
        SearchKind::Songs => app
            .search
            .results
            .tracks
            .iter()
            .skip(start)
            .take(list_a.height as usize)
            .map(|t| track_item(t, cols))
            .collect(),
        SearchKind::Albums => app
            .search
            .results
            .albums
            .iter()
            .skip(start)
            .take(list_a.height as usize)
            .map(|a| {
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
            .collect(),
        SearchKind::Artists => app
            .search
            .results
            .artists
            .iter()
            .skip(start)
            .take(list_a.height as usize)
            .map(|a| {
                let w = (list_a.width as usize).saturating_sub(BADGE_W + 2);
                ListItem::new(Line::from(vec![
                    badge(BadgeKind::Artist),
                    Span::raw(truncate_w(&a.name, w)),
                ]))
            })
            .collect(),
        SearchKind::Playlists => app
            .search
            .results
            .playlists
            .iter()
            .skip(start)
            .take(list_a.height as usize)
            .map(|p| {
                let count = p.track_count.map(|n| format!("{n} 首")).unwrap_or_default();
                ListItem::new(Line::from(vec![
                    badge(BadgeKind::Playlist),
                    Span::raw(fit_w(&p.title, cols.title.saturating_sub(BADGE_W))),
                    Span::styled(
                        format!(" {count:>width$}", width = cols.artist),
                        Style::default().fg(DIM),
                    ),
                ]))
            })
            .collect(),
    };

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )
        .highlight_symbol("> ");
    let mut state = ListState::default();
    state.select(Some(app.search.selected.min(len - 1) - start));
    f.render_stateful_widget(list, list_a, &mut state);
    (tab_hits, Some((list_a, start)))
}
