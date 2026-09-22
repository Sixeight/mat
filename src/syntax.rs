use std::str::FromStr;
use std::sync::OnceLock;

use ratatui::{
    style::{Color as TerminalColor, Style},
    text::Span,
};
use syntect::easy::HighlightLines;

use syntect::highlighting::{
    Color, FontStyle, ScopeSelectors, StyleModifier, Theme, ThemeItem, ThemeSet, ThemeSettings,
};
use syntect::parsing::{SyntaxReference, SyntaxSet};

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();

pub fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(two_face::syntax::extra_no_newlines)
}

pub fn find_syntax<'a>(path: &str, syntax_set: &'a SyntaxSet) -> &'a SyntaxReference {
    let extension = path.rsplit('.').next().unwrap_or("");
    syntax_set
        .find_syntax_by_extension(extension)
        .or_else(|| {
            let basename = path.rsplit('/').next().unwrap_or(path);
            syntax_set.find_syntax_by_extension(basename)
        })
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text())
}

fn color(r: u8, g: u8, b: u8) -> Color {
    Color { r, g, b, a: 0xff }
}

fn theme_item(scope: &str, foreground: Color, font_style: FontStyle) -> ThemeItem {
    ThemeItem {
        scope: ScopeSelectors::from_str(scope).expect("invalid built-in scope selector"),
        style: StyleModifier {
            foreground: Some(foreground),
            background: None,
            font_style: Some(font_style),
        },
    }
}

pub fn default_theme() -> Theme {
    Theme {
        name: Some("Rui Default".to_string()),
        author: Some("rui".to_string()),
        settings: ThemeSettings {
            foreground: Some(color(214, 220, 229)),
            background: Some(color(12, 17, 23)),
            caret: Some(color(214, 220, 229)),
            line_highlight: Some(color(22, 27, 34)),
            selection: Some(color(38, 79, 120)),
            gutter: Some(color(12, 17, 23)),
            gutter_foreground: Some(color(109, 119, 134)),
            ..ThemeSettings::default()
        },
        scopes: vec![
            theme_item("comment", color(121, 192, 255), FontStyle::ITALIC),
            theme_item("string", color(165, 214, 255), FontStyle::empty()),
            theme_item("constant.numeric", color(255, 166, 87), FontStyle::empty()),
            theme_item("keyword", color(255, 123, 114), FontStyle::BOLD),
            theme_item("storage", color(255, 123, 114), FontStyle::BOLD),
            theme_item("entity.name", color(210, 168, 255), FontStyle::empty()),
            theme_item("variable", color(214, 220, 229), FontStyle::empty()),
            theme_item("support.type", color(121, 192, 255), FontStyle::empty()),
            theme_item("support.class", color(121, 192, 255), FontStyle::empty()),
            theme_item("meta.path", color(210, 168, 255), FontStyle::empty()),
        ],
    }
}

fn load_theme_from_path(path: &str) -> Option<Theme> {
    ThemeSet::get_theme(path).ok()
}

pub fn resolve_theme(path: Option<&str>) -> Theme {
    path.and_then(load_theme_from_path)
        .unwrap_or_else(default_theme)
}

pub fn boost_fg(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let max = r.max(g).max(b) as f32;
    if max < 1.0 {
        return (140, 140, 140);
    }
    // Keep syntax colors natural — only lift very dark colors to ensure readability
    const MIN_BRIGHTNESS: f32 = 200.0;
    if max >= MIN_BRIGHTNESS {
        return (r, g, b);
    }
    let scale = MIN_BRIGHTNESS / max;
    (
        (r as f32 * scale).min(255.0) as u8,
        (g as f32 * scale).min(255.0) as u8,
        (b as f32 * scale).min(255.0) as u8,
    )
}

pub fn dim_color(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    // Slightly desaturate and reduce brightness for context lines
    let gray = r as f32 * 0.3 + g as f32 * 0.59 + b as f32 * 0.11;
    let mix = 0.1; // 10% toward gray
    let factor = 0.95; // very mild brightness reduction
    let r2 = ((r as f32 * (1.0 - mix) + gray * mix) * factor).min(255.0) as u8;
    let g2 = ((g as f32 * (1.0 - mix) + gray * mix) * factor).min(255.0) as u8;
    let b2 = ((b as f32 * (1.0 - mix) + gray * mix) * factor).min(255.0) as u8;
    (r2, g2, b2)
}

pub fn highlight_content_inner(
    content: &str,
    syntax: &SyntaxReference,
    ss: &SyntaxSet,
    theme: &Theme,
    bg: Option<TerminalColor>,
    dim: bool,
) -> Vec<Span<'static>> {
    let mut h = HighlightLines::new(syntax, theme);
    match h.highlight_line(content, ss) {
        Ok(ranges) => ranges
            .into_iter()
            .map(|(style, text)| {
                let (r, g, b) = if dim {
                    dim_color(style.foreground.r, style.foreground.g, style.foreground.b)
                } else if bg.is_some() {
                    boost_fg(style.foreground.r, style.foreground.g, style.foreground.b)
                } else {
                    (style.foreground.r, style.foreground.g, style.foreground.b)
                };
                let fg = TerminalColor::Rgb(r, g, b);
                let mut s = Style::default().fg(fg);
                if let Some(bg) = bg {
                    s = s.bg(bg);
                }
                Span::styled(text.to_string(), s)
            })
            .collect(),
        Err(_) => {
            let mut s = Style::default();
            if let Some(bg) = bg {
                s = s.bg(bg);
            }
            vec![Span::styled(content.to_string(), s)]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{default_theme, resolve_theme};
    use tempfile::NamedTempFile;

    #[test]
    fn resolve_theme_uses_default_when_path_is_missing() {
        let theme = resolve_theme(Some("/tmp/does-not-exist.tmTheme"));
        assert_eq!(theme.name.as_deref(), Some("Rui Default"));
    }

    #[test]
    fn resolve_theme_loads_theme_from_configured_path() {
        let mut file = NamedTempFile::new().unwrap();
        std::io::Write::write_all(
            &mut file,
            br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>name</key>
  <string>Temp Theme</string>
  <key>settings</key>
  <array>
    <dict>
      <key>settings</key>
      <dict>
        <key>foreground</key>
        <string>#F8F8F2</string>
        <key>background</key>
        <string>#101418</string>
      </dict>
    </dict>
  </array>
</dict>
</plist>"#,
        )
        .unwrap();

        let theme = resolve_theme(Some(file.path().to_str().unwrap()));
        assert_eq!(theme.name.as_deref(), Some("Temp Theme"));
    }

    #[test]
    fn default_theme_has_stable_name() {
        assert_eq!(default_theme().name.as_deref(), Some("Rui Default"));
    }
}
