use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// An ASCII progress bar with a cyan-to-lilac foreground gradient.
pub fn draw(f: &mut Frame, area: Rect, ratio: f64) {
    let filled = ((ratio.clamp(0.0, 1.0) * f64::from(area.width)).round() as u16).min(area.width);
    let mut spans = Vec::with_capacity(area.width as usize);
    for x in 0..area.width {
        let color = if x < filled {
            let t = if filled <= 1 {
                0.0
            } else {
                f64::from(x) / f64::from(filled - 1)
            };
            Color::Rgb(lerp(117, 194, t), lerp(204, 165, t), lerp(232, 238, t))
        } else {
            Color::DarkGray
        };
        spans.push(Span::styled(
            if x < filled { "=" } else { "-" },
            Style::default().fg(color),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn lerp(a: u8, b: u8, t: f64) -> u8 {
    (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as u8
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn progress_uses_ascii_and_changes_color_across_the_bar() {
        let mut terminal = Terminal::new(TestBackend::new(10, 1)).unwrap();
        terminal.draw(|f| super::draw(f, f.area(), 0.5)).unwrap();
        let buf = terminal.backend().buffer();
        let symbols: String = (0..10).map(|x| buf[(x, 0)].symbol()).collect();
        assert_eq!(symbols, "=====-----");
        assert_ne!(buf[(0, 0)].fg, buf[(4, 0)].fg);
        assert_eq!(buf[(5, 0)].fg, ratatui::style::Color::DarkGray);
    }
}
