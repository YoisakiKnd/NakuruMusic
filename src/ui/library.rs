use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, LoginMethod, LIBRARY_MENU};

use super::{ACCENT, DIM};

/// Returns the menu hit area + offset for mouse support.
pub fn draw(f: &mut Frame, app: &App, area: Rect) -> Option<(Rect, usize)> {
    if !app.api.is_logged_in() {
        return draw_login(f, app, area);
    }

    let items: Vec<ListItem> = LIBRARY_MENU
        .iter()
        .map(|label| ListItem::new(format!("  {label}")))
        .collect();
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )
        .highlight_symbol("> ");
    let mut state = ListState::default();
    state.select(Some(app.library.selected));

    let [list_a, hint_a] = Layout::vertical([
        Constraint::Length(LIBRARY_MENU.len() as u16 + 1),
        Constraint::Min(1),
    ])
    .areas(area);
    f.render_stateful_widget(list, list_a, &mut state);

    let hint = if app.library.loading {
        "加载中.."
    } else if app.library.failed {
        "加载失败    Enter 重试当前项目   x 退出登录   Esc 返回"
    } else {
        "已登录    Enter 打开   x 退出登录   Esc 返回"
    };
    f.render_widget(Paragraph::new(hint).style(Style::default().fg(DIM)), hint_a);
    Some((list_a, state.offset()))
}

fn draw_login(f: &mut Frame, app: &App, area: Rect) -> Option<(Rect, usize)> {
    let methods = &app.login.methods;
    let [head_a, list_a, hint_a] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(methods.len() as u16 + 1),
        Constraint::Min(1),
    ])
    .areas(area);

    f.render_widget(
        Paragraph::new(vec![
            Line::styled(
                "登录 YouTube Music",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                "登录后可访问：喜欢的音乐 - 我的歌单 - 收藏专辑 - 关注歌手 - 播放历史",
                Style::default().fg(DIM),
            ),
        ]),
        head_a,
    );

    let items: Vec<ListItem> = methods
        .iter()
        .map(|m| {
            let tag = match m {
                LoginMethod::Browser { .. } => "导入",
                LoginMethod::OpenBrowser => "网页",
                LoginMethod::Manual => "手动",
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("[{tag}] "), Style::default().fg(super::ACCENT)),
                Span::raw(m.label()),
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
    state.select(Some(app.login.selected));
    f.render_stateful_widget(list, list_a, &mut state);

    let hint: Vec<Line> = if app.login.busy {
        vec![Line::styled(
            "正在处理登录状态..",
            Style::default().fg(ACCENT),
        )]
    } else {
        vec![
            Line::from(""),
            Line::from("Enter 选择   Esc 返回"),
            Line::from(""),
            Line::from("从浏览器导入会直接读取该浏览器里已登录的 YouTube 身份信息，"),
            Line::from("无需手动复制。信息只保存在本机数据目录，仅用于向 YouTube 请求。"),
            Line::from("若浏览器尚未登录，先选「打开网页」登录后再回来导入。"),
            Line::from("Chrome 系浏览器若读取失败，请先完全关闭浏览器再试。"),
        ]
    };
    f.render_widget(
        Paragraph::new(hint)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(DIM)),
        hint_a,
    );

    Some((list_a, state.offset()))
}
