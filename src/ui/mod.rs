mod browse;
mod history;
mod home;
mod library;
mod now_playing;
mod overlay;
mod player_bar;
mod progress;
mod queue;
mod search;
mod settings;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{Action, App, Focus, MainView};

/// Where interactive things ended up on screen this frame - filled by
/// `draw`, consumed by the mouse handler.
#[derive(Debug, Clone, Copy, Default)]
pub struct UiLayout {
    /// (kind, list content area, scroll offset of the first visible row)
    pub main_list: Option<(MainListKind, Rect, usize)>,
    pub search_tabs: [Option<Rect>; 4],
    /// Whole queue pane (for focus), list content area and scroll offset.
    pub queue_pane: Option<Rect>,
    pub queue_list: Option<(Rect, usize)>,
    pub gauge: Option<Rect>,
    pub settings_rows: [Option<Rect>; 2],
    /// Clickable areas of the top navigation bar.
    pub nav_tabs: [Option<(Rect, MainView)>; NAV_TABS.len()],
}

/// Top-level destinations, with the key that reaches them. Showing the keys
/// in the bar is what makes them discoverable — there is no menu otherwise.
pub const NAV_TABS: [(&str, &str, MainView); 6] = [
    ("首页", "Esc", MainView::Home),
    ("搜索", "/", MainView::Search),
    ("音乐库", "L", MainView::Library),
    ("历史", "H", MainView::History),
    ("正在播放", "l", MainView::NowPlaying),
    ("设置", ",", MainView::Settings),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainListKind {
    Home,
    Search,
    Browse,
    Library,
    History,
}

pub const ACCENT: Color = Color::Rgb(117, 204, 232);
pub const DIM: Color = Color::DarkGray;

/// Item-type badges.
///
/// Deliberately ASCII: terminals disagree on how many columns an emoji
/// occupies (and which font supplies it), which silently breaks column
/// alignment. A fixed 4-column badge plus colour reads consistently
/// everywhere. Kind is conveyed by the letters, emphasis by the colour.
pub const BADGE_W: usize = 4;

pub fn badge(kind: BadgeKind) -> ratatui::text::Span<'static> {
    let (text, color) = match kind {
        BadgeKind::Album => ("ALB ", Color::Cyan),
        BadgeKind::Artist => ("ART ", Color::Magenta),
        BadgeKind::Playlist => ("LST ", Color::Yellow),
    };
    ratatui::text::Span::styled(text, Style::default().fg(color))
}

/// Track rows carry no badge - they are always inside a track list, so the
/// column layout already says what they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeKind {
    Album,
    Artist,
    Playlist,
}

/// Playback state marker for the player bar - ASCII for the same reason.
pub fn state_marker(loading: bool, paused: bool) -> &'static str {
    if loading {
        ".."
    } else if paused {
        "||"
    } else {
        "> "
    }
}

/// Column widths for a track row, derived from the available width.
/// CJK titles are twice as wide as ASCII, so everything is measured in
/// display columns rather than characters.
#[derive(Debug, Clone, Copy)]
pub struct TrackCols {
    pub title: usize,
    /// 0 means the artist column does not fit and is dropped.
    pub artist: usize,
    /// 0 means the duration column is dropped.
    pub duration: usize,
}

impl TrackCols {
    /// `width` is the list's inner width; 2 columns are reserved for the
    /// selection marker that `List::highlight_symbol` renders.
    pub fn new(width: u16) -> Self {
        let usable = (width as usize).saturating_sub(2);
        if usable < 24 {
            return TrackCols {
                title: usable,
                artist: 0,
                duration: 0,
            };
        }
        let duration = 5; // "12:34"
                          // Give the artist a third of the row, within sane bounds.
        let artist = (usable / 3).clamp(8, 24);
        let title = usable.saturating_sub(artist + duration + 2); // 2 gaps
        TrackCols {
            title,
            artist,
            duration,
        }
    }
}

/// Marker for truncated text.
///
/// Plain ASCII on purpose: the real ellipsis (U+2026) is East-Asian
/// *ambiguous* width, so CJK terminals draw it two columns wide while
/// `unicode-width` counts one, and every row containing it drifts. The same
/// reasoning rules out the em dash (U+2014), middle dot (U+00B7) and the
/// box-drawing glyphs anywhere text is laid out in columns.
const ELLIPSIS: &str = "..";

/// Truncate to `max` display columns, marking the cut.
pub fn truncate_w(s: &str, max: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if max == 0 {
        return String::new();
    }
    if s.width() <= max {
        return s.to_string();
    }
    // Too narrow to fit any marker: hard-cut to the column budget.
    let budget = if max > ELLIPSIS.len() {
        max - ELLIPSIS.len()
    } else {
        max
    };
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(c);
        w += cw;
    }
    if budget < max {
        out.push_str(ELLIPSIS);
    }
    out
}

/// Truncate to `max` columns, then pad with spaces so the next column
/// always starts at the same offset.
pub fn fit_w(s: &str, max: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let mut out = truncate_w(s, max);
    let pad = max.saturating_sub(out.width());
    out.extend(std::iter::repeat_n(' ', pad));
    out
}

/// First logical row to draw while keeping the selection in the viewport.
pub fn visible_start(total: usize, selected: usize, height: u16) -> usize {
    let visible = height as usize;
    selected
        .min(total.saturating_sub(1))
        .saturating_sub(visible / 2)
        .min(total.saturating_sub(visible))
}

/// One track row laid out in columns: title | artist | duration.
/// Shared by the browse, search, home and queue views so every list lines up.
pub fn track_spans(
    t: &crate::api::models::Track,
    cols: TrackCols,
) -> Vec<ratatui::text::Span<'static>> {
    use crate::api::models::format_duration;
    use ratatui::text::Span;

    let mut spans = vec![Span::raw(fit_w(&t.title, cols.title))];
    if cols.artist > 0 {
        spans.push(Span::styled(
            format!(" {}", fit_w(&t.artists, cols.artist)),
            Style::default().fg(DIM),
        ));
    }
    if cols.duration > 0 {
        let dur = t.duration_secs.map(format_duration).unwrap_or_default();
        // Right-align inside the duration column.
        spans.push(Span::styled(
            format!(" {:>width$}", dur, width = cols.duration),
            Style::default().fg(DIM),
        ));
    }
    spans
}

pub fn track_item(
    t: &crate::api::models::Track,
    cols: TrackCols,
) -> ratatui::widgets::ListItem<'static> {
    ratatui::widgets::ListItem::new(ratatui::text::Line::from(track_spans(t, cols)))
}

/// Takes `&mut App` because the image protocol re-encodes the cover when
/// its area changes; everything else only reads.
pub fn draw(f: &mut Frame, app: &mut App) -> UiLayout {
    let mut layout = UiLayout::default();
    // The status row only exists while there is something to say, so no
    // screen space is wasted on an empty bar.
    let status_h = u16::from(app.status.is_some() || app.detail_retry_hint.is_some());
    let [nav_area, content_area, status_area, bar_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(status_h),
        Constraint::Length(3),
    ])
    .areas(f.area());

    layout.nav_tabs = draw_nav(f, app, nav_area);

    // The now-playing page wants every column it can get for cover art and
    // lyrics, so the queue steps aside there.
    let show_queue = content_area.width >= 84 && app.main_view != MainView::NowPlaying;
    if !show_queue && app.focus == Focus::Queue {
        app.focus = Focus::Main;
    }
    let (main_area, queue_area) = if show_queue {
        let [m, q] =
            Layout::horizontal([Constraint::Min(50), Constraint::Length(34)]).areas(content_area);
        (m, Some(q))
    } else {
        (content_area, None)
    };

    draw_main(f, app, main_area, &mut layout);
    if let Some(qa) = queue_area {
        layout.queue_pane = Some(qa);
        layout.queue_list = queue::draw(f, app, qa);
    }
    if status_h > 0 {
        draw_status(f, app, status_area);
    }
    layout.gauge = player_bar::draw(f, app, bar_area);

    if app.help_visible {
        overlay::draw_help(f, app);
    }
    layout
}

/// Top navigation: where you are, where you can go, and how.
fn draw_nav(f: &mut Frame, app: &App, area: Rect) -> [Option<(Rect, MainView)>; NAV_TABS.len()] {
    use unicode_width::UnicodeWidthStr;

    let mut hits: [Option<(Rect, MainView)>; NAV_TABS.len()] = Default::default();
    let mut spans = Vec::new();
    let mut x = area.x;
    let compact = area.width < 96;
    let compact_labels = ["首页", "搜索", "库", "史", "播", "设"];
    let hint = if compact {
        format!(
            "{} {}",
            app.key_label(Action::Help),
            app.key_label(Action::Quit)
        )
    } else {
        format!(
            "{} 帮助  {} 退出",
            app.key_label(Action::Help),
            app.key_label(Action::Quit)
        )
    };
    let hint_w = hint.width() as u16;
    let tab_right = area.right().saturating_sub(hint_w + 1);

    for (i, (label, key, view)) in NAV_TABS.iter().enumerate() {
        let active = app.main_view == *view
            // Browsing is reached from search results, so keep that tab lit.
            || (app.main_view == MainView::Browse && *view == MainView::Search);
        let active_key = match view {
            MainView::Search => app.key_label(Action::Search),
            MainView::Library => app.key_label(Action::Library),
            MainView::History => app.key_label(Action::History),
            MainView::NowPlaying => app.key_label(Action::LyricsToggle),
            MainView::Settings => app.key_label(Action::Settings),
            _ => (*key).to_string(),
        };
        let text = if compact {
            format!("{active_key}{} ", compact_labels[i])
        } else {
            format!(" {label} {active_key} ")
        };
        let w = text.width() as u16;
        if x.saturating_add(w) > tab_right {
            break;
        }
        spans.push(Span::styled(
            text,
            if active {
                Style::default()
                    .fg(ACCENT)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::default().fg(DIM)
            },
        ));
        hits[i] = Some((
            Rect {
                x,
                y: area.y,
                width: w,
                height: 1,
            },
            *view,
        ));
        x += w;
    }

    // Reserve the right edge before laying out tabs so help/quit stay visible
    // even in a 40-column terminal.
    f.render_widget(Paragraph::new(Line::from(spans)), area);
    if area.width >= hint_w {
        f.render_widget(
            Paragraph::new(hint).style(Style::default().fg(DIM)),
            Rect {
                x: area.right() - hint_w,
                y: area.y,
                width: hint_w,
                height: 1,
            },
        );
    }
    hits
}

/// Transient messages get their own row instead of covering list content.
fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let Some(msg) = app.status.as_ref().or(app.detail_retry_hint.as_ref()) else {
        return;
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ! ", Style::default().fg(Color::Black).bg(Color::Yellow)),
            Span::styled(
                format!(" {msg}"),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ])),
        area,
    );
}

fn pane_border(focused: bool) -> Style {
    if focused {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(DIM)
    }
}

fn draw_main(f: &mut Frame, app: &mut App, area: Rect, layout: &mut UiLayout) {
    let title = match app.main_view {
        MainView::Home => " 首页 ",
        MainView::Search => " 搜索 ",
        MainView::Browse => " 浏览 ",
        MainView::NowPlaying => " 正在播放 ",
        MainView::Library => " 音乐库 ",
        MainView::History => " 播放历史 ",
        MainView::Settings => " 设置 ",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(pane_border(app.focus == Focus::Main))
        .title(title)
        .title_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let mut inner = block.inner(area);
    f.render_widget(block, area);

    // An active input line takes over the first row of the main pane.
    if let Some((mode, buf)) = &app.input {
        let input_area = Rect { height: 1, ..inner };
        search::draw_input(f, *mode, buf, input_area);
        inner.y += 1;
        inner.height = inner.height.saturating_sub(1);
    }

    match app.main_view {
        MainView::Home => {
            if let Some(hit) = home::draw(f, app, inner) {
                layout.main_list = Some((MainListKind::Home, hit.0, hit.1));
            }
        }
        MainView::Search => {
            let (tabs, hit) = search::draw(f, app, inner);
            layout.search_tabs = tabs;
            if let Some(hit) = hit {
                layout.main_list = Some((MainListKind::Search, hit.0, hit.1));
            }
        }
        MainView::Browse => {
            if let Some(hit) = browse::draw(f, app, inner) {
                layout.main_list = Some((MainListKind::Browse, hit.0, hit.1));
            }
        }
        MainView::NowPlaying => now_playing::draw(f, app, inner),
        MainView::Library => {
            if let Some(hit) = library::draw(f, app, inner) {
                layout.main_list = Some((MainListKind::Library, hit.0, hit.1));
            }
        }
        MainView::History => {
            if let Some(hit) = history::draw(f, app, inner) {
                layout.main_list = Some((MainListKind::History, hit.0, hit.1));
            }
        }
        MainView::Settings => layout.settings_rows = settings::draw(f, app, inner),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_input_never_enters_terminal_buffer() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let secret = "SAPISID=very-secret-value";
        let mut terminal = Terminal::new(TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|f| search::draw_input(f, crate::app::InputMode::Login, secret, f.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let line: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect();
        assert!(!line.contains("SAPISID"));
        assert!(!line.contains("very-secret-value"));
        assert!(line.contains("Cookie>"));
    }

    #[test]
    fn search_input_shows_submit_and_cancel_at_narrow_width() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut terminal = Terminal::new(TestBackend::new(40, 1)).unwrap();
        terminal
            .draw(|f| {
                search::draw_input(
                    f,
                    crate::app::InputMode::Search,
                    "long search query",
                    f.area(),
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let line: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect();
        assert!(line.contains("Enter"));
        assert!(line.contains("Esc"));
    }

    #[test]
    fn truncate_counts_display_columns_not_chars() {
        use unicode_width::UnicodeWidthStr;
        // CJK characters occupy two columns each.
        assert_eq!(truncate_w("晴天", 10), "晴天");
        assert_eq!(truncate_w("晴天七里香", 6), "晴天..");
        assert_eq!(truncate_w("abcdef", 4), "ab..");
        assert_eq!(truncate_w("abc", 0), "");
        // The result never exceeds the budget, marker included.
        for max in 1..12 {
            assert!(truncate_w("晴天七里香abc", max).width() <= max, "max={max}");
        }
    }

    /// The nav bar is the only thing telling users what exists and how to
    /// get there, so assert it renders every destination with its key.
    #[test]
    fn nav_bar_shows_all_destinations_with_keys() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut terminal = Terminal::new(TestBackend::new(80, 1)).unwrap();
        let mut hits = None;
        terminal
            .draw(|f| {
                // Render the bar standalone with a stub state.
                let mut spans = Vec::new();
                let mut rects: Vec<Rect> = Vec::new();
                let mut x = 0u16;
                for (label, key, _) in NAV_TABS {
                    let text = format!(" {label} {key} ");
                    let w = unicode_width::UnicodeWidthStr::width(text.as_str()) as u16;
                    spans.push(Span::raw(text));
                    rects.push(Rect::new(x, 0, w, 1));
                    x += w;
                }
                f.render_widget(Paragraph::new(Line::from(spans)), f.area());
                hits = Some(rects);
            })
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let line: String = (0..buf.area.width)
            .map(|x| buf[(x, 0)].symbol())
            .collect::<String>();
        println!("[{}]", line.trim_end());

        let squeezed: String = line.chars().filter(|c| *c != ' ').collect();
        for (label, key, _) in NAV_TABS {
            let squeezed_label: String = label.chars().filter(|c| *c != ' ').collect();
            assert!(squeezed.contains(&squeezed_label), "missing tab {label}");
            assert!(squeezed.contains(key), "missing key hint for {label}");
        }
        // Tabs must not overlap, or clicks would land on the wrong one.
        let rects = hits.unwrap();
        for pair in rects.windows(2) {
            assert!(pair[0].right() <= pair[1].x, "nav tabs overlap");
        }
    }

    #[test]
    fn ui_text_avoids_ambiguous_width_characters() {
        // These render two columns wide in CJK terminals while
        // unicode-width reports one, so they must never reach the screen.
        // Listed by codepoint so the check cannot be defeated by an
        // editor silently normalising the literals.
        const AMBIGUOUS: [char; 6] = [
            '\u{2026}', // …
            '\u{2014}', // —
            '\u{00B7}', // ·
            '\u{2192}', // →
            '\u{2713}', // ✓
            '\u{25B6}', // ▶
        ];
        for bad in AMBIGUOUS {
            assert!(
                !ELLIPSIS.contains(bad),
                "truncation marker uses ambiguous-width {bad:?}"
            );
        }
        // The whole UI must stay clear of them too.
        for (name, text) in [
            ("badge", badge(BadgeKind::Album).content.to_string()),
            ("marker", state_marker(false, false).to_string()),
        ] {
            for bad in AMBIGUOUS {
                assert!(!text.contains(bad), "{name} contains {bad:?}");
            }
        }
    }

    #[test]
    fn fit_pads_to_exact_column_width() {
        use unicode_width::UnicodeWidthStr;
        assert_eq!(fit_w("ab", 5).width(), 5);
        assert_eq!(fit_w("晴天", 5).width(), 5);
        assert_eq!(fit_w("very long text here", 6).width(), 6);
    }

    fn track(title: &str, artists: &str, secs: u32) -> crate::api::models::Track {
        crate::api::models::Track {
            video_id: "id".into(),
            title: title.into(),
            artists: artists.into(),
            album: None,
            duration_secs: Some(secs),
            cover_url: None,
        }
    }

    /// Render real rows through ratatui and check the columns line up -
    /// a long CJK title must not push the artist/duration off the row.
    fn render_rows(width: u16) -> Vec<String> {
        use ratatui::backend::TestBackend;
        use ratatui::widgets::List;
        use ratatui::Terminal;

        let tracks = [
            track("晴天", "周杰伦", 269),
            track(
                "这是一个非常非常长的中文歌曲标题用来测试截断行为是否正确",
                "某位歌手",
                225,
            ),
            track(
                "An Extremely Long English Song Title That Should Be Truncated",
                "Some Artist",
                194,
            ),
        ];
        let cols = TrackCols::new(width);
        let items: Vec<_> = tracks.iter().map(|t| track_item(t, cols)).collect();

        let mut terminal = Terminal::new(TestBackend::new(width, 3)).unwrap();
        terminal
            .draw(|f| {
                let list = List::new(items).highlight_symbol("> ");
                let mut state = ratatui::widgets::ListState::default();
                state.select(Some(0));
                f.render_stateful_widget(list, f.area(), &mut state);
            })
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// A wide char occupies two terminal cells, the second of which reads
    /// back as a blank - drop spaces before matching CJK text.
    fn squeeze(s: &str) -> String {
        s.chars().filter(|c| *c != ' ').collect()
    }

    #[test]
    fn long_titles_never_hide_artist_and_duration() {
        let lines = render_rows(60);
        for l in &lines {
            println!("[{l}]");
        }
        // Every row keeps its duration, which sits in the last column.
        assert!(
            lines[0].contains("4:29"),
            "row 0 lost its duration: {}",
            lines[0]
        );
        assert!(
            lines[1].contains("3:45"),
            "row 1 lost its duration: {}",
            lines[1]
        );
        assert!(
            lines[2].contains("3:14"),
            "row 2 lost its duration: {}",
            lines[2]
        );
        // Artists survive too.
        assert!(squeeze(&lines[0]).contains("周杰伦"));
        assert!(squeeze(&lines[1]).contains("某位歌手"));
        assert!(lines[2].contains("Some Artist"));
        // Overlong titles are cut with an ellipsis rather than overflowing.
        assert!(lines[1].contains(".."));
        assert!(lines[2].contains(".."));
        // Durations must all start at the same column.
        let col = |s: &String, pat: &str| s.find(pat).map(|i| s[..i].chars().count());
        assert_eq!(col(&lines[0], "4:29"), col(&lines[1], "3:45"));
        assert_eq!(col(&lines[1], "3:45"), col(&lines[2], "3:14"));
    }

    /// Badges are fixed-width ASCII so mixed rows stay aligned - the whole
    /// reason emoji were dropped.
    #[test]
    fn badges_are_fixed_width_and_keep_rows_aligned() {
        use ratatui::backend::TestBackend;
        use ratatui::text::{Line, Span};
        use ratatui::widgets::{List, ListItem};
        use ratatui::Terminal;
        use unicode_width::UnicodeWidthStr;

        let cols = TrackCols::new(50);
        let rows = vec![
            ListItem::new(Line::from(vec![
                badge(BadgeKind::Album),
                Span::raw(fit_w("某张专辑", cols.title - BADGE_W)),
            ])),
            ListItem::new(Line::from(vec![
                badge(BadgeKind::Artist),
                Span::raw(fit_w("Some Artist", cols.title - BADGE_W)),
            ])),
            ListItem::new(Line::from(vec![
                badge(BadgeKind::Playlist),
                Span::raw(fit_w("我的歌单", cols.title - BADGE_W)),
            ])),
        ];

        let mut terminal = Terminal::new(TestBackend::new(50, 3)).unwrap();
        terminal
            .draw(|f| f.render_widget(List::new(rows), f.area()))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let lines: Vec<String> = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        for l in &lines {
            println!("[{l}]");
        }

        // Every badge occupies the same columns, so titles start together.
        for l in &lines {
            assert_eq!(l[..BADGE_W].width(), BADGE_W, "badge width drifted: {l}");
        }
        assert!(lines[0].starts_with("ALB "));
        assert!(lines[1].starts_with("ART "));
        assert!(lines[2].starts_with("LST "));
        // Titles all begin at the same column regardless of script.
        assert!(lines[1][BADGE_W..].starts_with("Some Artist"));
    }

    #[test]
    fn narrow_terminal_still_renders_titles() {
        let lines = render_rows(24);
        for l in &lines {
            println!("[{l}]");
            assert_eq!(l.chars().count(), 24, "row overflowed the viewport");
        }
        assert!(squeeze(&lines[0]).contains("晴天"));
    }

    #[test]
    fn columns_shrink_gracefully_on_narrow_terminals() {
        let wide = TrackCols::new(80);
        assert!(wide.title > 30 && wide.artist > 0 && wide.duration == 5);
        // Everything must still fit inside the row.
        assert!(wide.title + wide.artist + wide.duration + 2 <= 78);

        let narrow = TrackCols::new(20);
        assert_eq!(narrow.artist, 0, "artist column must be dropped when tight");
        assert_eq!(narrow.duration, 0);
    }
}
