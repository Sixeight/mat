use std::io::{self, Write};

use ratatui::{
    style::{Color, Modifier, Style},
    text::Line,
};

/// Remove terminal controls while preserving Markdown line breaks and tabs.
pub fn sanitize_text(text: &str) -> String {
    enum State {
        Text,
        Escape,
        EscapeIntermediate,
        Csi,
        ControlString { escaped: bool },
    }

    let mut state = State::Text;
    let mut output = String::with_capacity(text.len());
    for ch in text.chars() {
        state = match state {
            State::Text => match ch {
                '\x1b' => State::Escape,
                '\u{9b}' => State::Csi,
                '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => {
                    State::ControlString { escaped: false }
                }
                '\n' | '\t' => {
                    output.push(ch);
                    State::Text
                }
                ch if ch.is_control() => State::Text,
                _ => {
                    output.push(ch);
                    State::Text
                }
            },
            State::Escape => match ch {
                '[' => State::Csi,
                ']' | 'P' | 'X' | '^' | '_' => State::ControlString { escaped: false },
                '\x20'..='\x2f' => State::EscapeIntermediate,
                _ => State::Text,
            },
            State::EscapeIntermediate => match ch {
                '\x20'..='\x2f' => State::EscapeIntermediate,
                _ => State::Text,
            },
            State::Csi => match ch {
                '\x40'..='\x7e' => State::Text,
                _ => State::Csi,
            },
            State::ControlString { escaped } => {
                if ch == '\x07' || ch == '\u{9c}' || (escaped && ch == '\\') {
                    State::Text
                } else {
                    State::ControlString {
                        escaped: ch == '\x1b',
                    }
                }
            }
        };
    }
    output
}

/// Write one styled line without changing the terminal's cursor or screen mode.
pub fn write_line(output: &mut impl Write, line: &Line<'_>, color: bool) -> io::Result<()> {
    for span in &line.spans {
        let content = sanitize_text(&span.content);
        if content.is_empty() {
            continue;
        }
        if color {
            write_style(output, line.style.patch(span.style))?;
        }
        output.write_all(content.as_bytes())?;
    }
    if color {
        output.write_all(b"\x1b[0m")?;
    }
    output.write_all(b"\n")
}

fn write_style(output: &mut impl Write, style: Style) -> io::Result<()> {
    output.write_all(b"\x1b[0")?;
    if let Some(fg) = style.fg {
        write_color(output, fg, false)?;
    }
    if let Some(bg) = style.bg {
        write_color(output, bg, true)?;
    }
    for (modifier, code) in [
        (Modifier::BOLD, 1),
        (Modifier::DIM, 2),
        (Modifier::ITALIC, 3),
        (Modifier::UNDERLINED, 4),
        (Modifier::SLOW_BLINK, 5),
        (Modifier::RAPID_BLINK, 6),
        (Modifier::REVERSED, 7),
        (Modifier::HIDDEN, 8),
        (Modifier::CROSSED_OUT, 9),
    ] {
        if style.add_modifier.contains(modifier) {
            write!(output, ";{code}")?;
        }
    }
    output.write_all(b"m")
}

fn write_color(output: &mut impl Write, color: Color, background: bool) -> io::Result<()> {
    let base = if background { 48 } else { 38 };
    let code = match color {
        Color::Reset => {
            if background {
                49
            } else {
                39
            }
        }
        Color::Rgb(r, g, b) => return write!(output, ";{base};2;{r};{g};{b}"),
        Color::Indexed(index) => return write!(output, ";{base};5;{index}"),
        Color::Black => 30,
        Color::Red => 31,
        Color::Green => 32,
        Color::Yellow => 33,
        Color::Blue => 34,
        Color::Magenta => 35,
        Color::Cyan => 36,
        Color::Gray => 37,
        Color::DarkGray => 90,
        Color::LightRed => 91,
        Color::LightGreen => 92,
        Color::LightYellow => 93,
        Color::LightBlue => 94,
        Color::LightMagenta => 95,
        Color::LightCyan => 96,
        Color::White => 97,
    };
    let code = if background && color != Color::Reset {
        code + 10
    } else {
        code
    };
    write!(output, ";{code}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Span;

    #[test]
    fn line_style_is_inherited_and_span_modifiers_can_remove_it() {
        let line = Line::from(vec![
            Span::raw("inherited"),
            Span::styled(
                "overridden",
                Style::default()
                    .fg(Color::Red)
                    .remove_modifier(Modifier::BOLD),
            ),
        ])
        .style(
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        );
        let mut output = Vec::new();
        write_line(&mut output, &line, true).expect("write line");
        assert_eq!(
            String::from_utf8(output).expect("UTF-8"),
            "\x1b[0;32;1minherited\x1b[0;31moverridden\x1b[0m\n"
        );
    }

    #[test]
    fn plain_output_contains_neither_styles_nor_input_controls() {
        let line = Line::styled("a\x1b[2Jb\x07", Style::default().fg(Color::Red));
        let mut output = Vec::new();
        write_line(&mut output, &line, false).expect("write line");
        assert_eq!(output, b"ab\n");
    }

    #[test]
    fn sanitizer_removes_control_payloads_and_keeps_markdown_whitespace() {
        let input = "日本語\n\tA\x1b]52;c;secret\x07B\x1b_Gpayload\x1b\\C\u{9b}2JD\r\x08E";
        assert_eq!(sanitize_text(input), "日本語\n\tABCDE");
    }

    #[test]
    fn sanitizer_discards_unterminated_control_strings() {
        assert_eq!(sanitize_text("safe\x1b]52;c;unfinished"), "safe");
    }

    #[test]
    fn rgb_and_indexed_backgrounds_are_emitted_and_reset() {
        let line = Line::styled(
            "styled",
            Style::default()
                .fg(Color::Rgb(1, 2, 3))
                .bg(Color::Indexed(42)),
        );
        let mut output = Vec::new();
        write_line(&mut output, &line, true).expect("write line");
        assert_eq!(output, b"\x1b[0;38;2;1;2;3;48;5;42mstyled\x1b[0m\n");
    }
}
