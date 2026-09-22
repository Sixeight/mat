use ratatui::text::{Line, Span};

pub fn wrap_line(line: Line<'static>, max_width: usize) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthChar;

    const TAB_STOP: usize = 4;

    if max_width == 0 {
        return vec![line];
    }
    if line.width() <= max_width && !line.spans.iter().any(|span| span.content.contains('\t')) {
        return vec![line];
    }

    let mut result: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_width: usize = 0;

    for span in line.spans {
        let style = span.style;
        let mut buf = String::new();
        for ch in span.content.chars() {
            let (rendered, repeat) = if ch == '\t' {
                (' ', TAB_STOP - current_width % TAB_STOP)
            } else {
                (ch, 1)
            };
            for _ in 0..repeat {
                let ch_width = rendered.width().unwrap_or(0);
                // Flush before this char when it would overflow the current line.
                // Important: `buf` may be empty at a span boundary while `current_spans`
                // still holds earlier content (common with CJK + styled markdown).
                if ch_width > 0 && current_width + ch_width > max_width && current_width > 0 {
                    if !buf.is_empty() {
                        current_spans.push(Span::styled(std::mem::take(&mut buf), style));
                    }
                    result.push(Line::from(std::mem::take(&mut current_spans)));
                    current_width = 0;
                }
                buf.push(rendered);
                current_width += ch_width;
            }
        }
        if !buf.is_empty() {
            current_spans.push(Span::styled(buf, style));
        }
    }
    if !current_spans.is_empty() {
        result.push(Line::from(current_spans));
    }
    if result.is_empty() {
        result.push(Line::from(""));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};

    #[test]
    fn wrap_line_breaks_at_span_boundary_for_wide_chars() {
        // "abcd" (4) + fullwidth "あい" (4) with a style boundary in between.
        // Old bug: refused to wrap when buf was empty at the span start, overflowing.
        let line = Line::from(vec![
            Span::raw("abcd"),
            Span::styled("あい", Style::default().fg(Color::Red)),
        ]);
        let wrapped = wrap_line(line, 6);
        assert!(
            wrapped.iter().all(|l| l.width() <= 6),
            "wrapped lines exceeded width: {:?}",
            wrapped.iter().map(|l| l.width()).collect::<Vec<_>>()
        );
        assert!(wrapped.len() >= 2);
    }

    #[test]
    fn wrap_line_keeps_ascii_within_max_width() {
        let line = Line::from("abcdefghijklmnopqrstuvwxyz");
        let wrapped = wrap_line(line, 10);
        assert_eq!(wrapped.len(), 3);
        assert!(wrapped.iter().all(|l| l.width() <= 10));
    }

    #[test]
    fn wrap_line_expands_tabs_to_four_column_stops() {
        let wrapped = wrap_line(Line::from("a\tb"), 10);
        let text = wrapped
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(text, "a   b");
        assert!(!text.contains('\t'));
    }
}
