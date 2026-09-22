use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use pulldown_cmark::{
    BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use syntect::highlighting::Theme;
use syntect::parsing::{SyntaxReference, SyntaxSet};

use std::collections::VecDeque;
use std::ops::{Range, RangeInclusive};

use super::syntax::highlight_content_inner;
use super::wrap_line;

pub const IMAGE_HEIGHT: u16 = 20;
pub const VIDEO_HEIGHT: u16 = 4;
const BLOB_URL_PREFIX: &str = "https://github.com/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    Image,
    Video,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobUrlInfo {
    pub owner: String,
    pub repo: String,
    pub git_ref: String,
    pub path: String,
    pub start_line: u64,
    pub end_line: Option<u64>,
}

pub fn parse_blob_url(url: &str) -> Option<BlobUrlInfo> {
    let url = url.strip_prefix("https://github.com/")?;
    let (path_part, fragment) = url.split_once('#')?;
    let mut parts = path_part.splitn(4, '/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    let blob = parts.next()?;
    if blob != "blob" {
        return None;
    }
    let rest = parts.next()?;
    let (git_ref, file_path) = rest.split_once('/')?;

    let range = fragment.strip_prefix('L')?;
    let (start_line, end_line) = if let Some((start, end)) = range.split_once("-L") {
        (start.parse().ok()?, Some(end.parse().ok()?))
    } else {
        (range.parse().ok()?, None)
    };

    Some(BlobUrlInfo {
        owner: owner.to_string(),
        repo: repo.to_string(),
        git_ref: git_ref.to_string(),
        path: file_path.to_string(),
        start_line,
        end_line,
    })
}

struct BlobUrlInText<'a> {
    url: &'a str,
    info: BlobUrlInfo,
}

fn find_blob_url_in_text(text: &str) -> Option<BlobUrlInText<'_>> {
    let start = text.find(BLOB_URL_PREFIX)?;
    let rest = &text[start..];
    // Find the end of the URL (whitespace or end of string)
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let url = &rest[..end];
    let info = parse_blob_url(url)?;
    Some(BlobUrlInText { url, info })
}

fn is_github_attachment_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains("://github.com/user-attachments/assets/")
        || lower.contains("://user-images.githubusercontent.com/")
        || lower.contains("://private-user-images.githubusercontent.com/")
}

fn media_kind_for_url(url: &str) -> Option<MediaKind> {
    let trimmed = url.trim_end_matches(['.', ',', ';', '!', '?', '"', '\'', ')']);
    let trimmed = trimmed.split_once('#').map_or(trimmed, |(base, _)| base);
    let trimmed = trimmed.split_once('?').map_or(trimmed, |(base, _)| base);
    let trimmed = trimmed.to_ascii_lowercase();

    if ["png", "jpg", "jpeg", "gif", "webp"]
        .iter()
        .any(|ext| trimmed.ends_with(&format!(".{ext}")))
    {
        return Some(MediaKind::Image);
    }
    if ["mp4", "mov", "webm"]
        .iter()
        .any(|ext| trimmed.ends_with(&format!(".{ext}")))
    {
        return Some(MediaKind::Video);
    }
    // GitHub drag-and-drop uploads have no extension; try as image and
    // reclassify to video from Content-Type when fetching.
    if is_github_attachment_url(&trimmed) {
        return Some(MediaKind::Image);
    }
    None
}

fn html_attr_value<'a>(tag: &'a str, attr: &str) -> Option<&'a str> {
    let lower = tag.to_ascii_lowercase();
    let attr_lower = attr.to_ascii_lowercase();
    let key = format!("{attr_lower}=");
    let start = lower.find(&key)? + key.len();
    let bytes = tag.as_bytes();
    let quote = *bytes.get(start)?;
    if quote == b'"' || quote == b'\'' {
        let from = start + 1;
        let end = tag[from..].find(quote as char)? + from;
        Some(&tag[from..end])
    } else {
        let rest = &tag[start..];
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '>')
            .unwrap_or(rest.len());
        Some(rest[..end].trim_end_matches('/'))
    }
}

/// Extract a single media block from a raw HTML tag (`<img>` / `<video>` / `<source>`).
fn media_from_html_tag(html: &str) -> Option<(MediaKind, String, String)> {
    let trimmed = html.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    let kind = if lower.starts_with("<video") || lower.starts_with("<source") {
        MediaKind::Video
    } else if lower.starts_with("<img") {
        MediaKind::Image
    } else {
        return None;
    };
    let url = html_attr_value(trimmed, "src")?.to_string();
    if url.is_empty() {
        return None;
    }
    let alt = html_attr_value(trimmed, "alt")
        .unwrap_or_default()
        .to_string();
    Some((kind, url, alt))
}

fn split_raw_media_segments(text: &str) -> Vec<(&str, Option<(&str, MediaKind)>)> {
    let mut segments = Vec::new();
    let mut cursor = 0;

    for token in text.match_indices("https://") {
        let start = token.0;
        let rest = &text[start..];
        let end = rest
            .find(|c: char| c.is_whitespace() || ['(', ')', '<', '>'].contains(&c))
            .map_or(text.len(), |idx| start + idx);
        let candidate = &text[start..end];
        let trimmed = candidate.trim_end_matches(['.', ',', ';', '!', '?', '"', '\'', ')']);
        let Some(kind) = media_kind_for_url(trimmed) else {
            continue;
        };

        if start > cursor {
            segments.push((&text[cursor..start], None));
        }
        segments.push((&text[start..end], Some((trimmed, kind))));
        cursor = end;
    }

    if cursor < text.len() {
        segments.push((&text[cursor..], None));
    }

    if segments.is_empty() {
        segments.push((text, None));
    }

    segments
}

// GitHub Dark–aligned markdown chrome (primer / prettylights)
pub const CODE_BLOCK_FG: Color = Color::Rgb(201, 209, 217);
pub const CODE_BLOCK_BG: Color = Color::Rgb(22, 27, 34);
pub const CODE_BLOCK_BORDER_FG: Color = Color::Rgb(48, 54, 61);
pub const CODE_BLOCK_LANG_FG: Color = Color::Rgb(110, 118, 129);
pub const INLINE_CODE_FG: Color = Color::Rgb(255, 166, 87);
pub const INLINE_CODE_BG: Color = Color::Rgb(33, 38, 45);
pub const LINK_FG: Color = Color::Rgb(88, 166, 255);
pub const BLOCKQUOTE_FG: Color = Color::Rgb(139, 148, 158);
pub const BLOCKQUOTE_BORDER_FG: Color = Color::Rgb(56, 139, 253);
pub const LIST_MARKER_FG: Color = Color::Rgb(110, 118, 129);
pub const TASK_CHECKED_FG: Color = Color::Rgb(63, 185, 80);
pub const TASK_UNCHECKED_FG: Color = Color::Rgb(110, 118, 129);
pub const HR_FG: Color = Color::Rgb(61, 68, 77);
pub const TABLE_BORDER_FG: Color = Color::Rgb(61, 68, 77);
pub const MARKDOWN_WRAP_WIDTH: usize = 160;
const ALERT_NOTE_FG: Color = Color::Rgb(31, 111, 235);
const ALERT_TIP_FG: Color = Color::Rgb(26, 127, 55);
const ALERT_IMPORTANT_FG: Color = Color::Rgb(130, 80, 223);
const ALERT_WARNING_FG: Color = Color::Rgb(191, 135, 0);
const ALERT_CAUTION_FG: Color = Color::Rgb(218, 54, 51);

fn style_with_bg(mut style: Style, bg: Option<Color>) -> Style {
    if let Some(b) = bg {
        style = style.bg(b);
    }
    style
}

fn list_bullet(depth: usize) -> &'static str {
    match depth {
        0 | 1 => "•",
        2 => "◦",
        _ => "▪",
    }
}

fn list_marker_width(depth: usize, ordered: Option<u64>) -> usize {
    let indent = 2 * depth.clamp(1, 6);
    match ordered {
        Some(n) => indent + n.to_string().len() + 2, // "N. "
        None => indent + 2,                          // "• "
    }
}

fn push_list_marker(
    spans: &mut Vec<Span<'static>>,
    depth: usize,
    ordered: Option<&mut u64>,
    bg: Option<Color>,
) {
    let indent = "  ".repeat(depth.clamp(1, 6));
    let style = style_with_bg(Style::default().fg(LIST_MARKER_FG), bg);
    if let Some(counter) = ordered {
        spans.push(Span::styled(format!("{indent}{counter}. "), style));
        *counter += 1;
    } else {
        spans.push(Span::styled(
            format!("{indent}{} ", list_bullet(depth)),
            style,
        ));
    }
}

fn push_task_marker(spans: &mut Vec<Span<'static>>, checked: bool, bg: Option<Color>) {
    let (marker, fg) = if checked {
        ("☑ ", TASK_CHECKED_FG)
    } else {
        ("☐ ", TASK_UNCHECKED_FG)
    };
    spans.push(Span::styled(
        marker.to_string(),
        style_with_bg(Style::default().fg(fg), bg),
    ));
}

fn code_fence_lang<'a>(kind: &'a CodeBlockKind<'_>) -> Option<&'a str> {
    match kind {
        CodeBlockKind::Fenced(lang) => {
            let token = lang.split_whitespace().next().unwrap_or("");
            if token.is_empty()
                || token.eq_ignore_ascii_case("suggestion")
                || token.eq_ignore_ascii_case("mermaid")
            {
                None
            } else {
                Some(token)
            }
        }
        CodeBlockKind::Indented => None,
    }
}

fn code_lang_label_spans(lang: &str) -> Vec<Span<'static>> {
    let border = Style::default().fg(CODE_BLOCK_BORDER_FG).bg(CODE_BLOCK_BG);
    let label = Style::default().fg(CODE_BLOCK_LANG_FG).bg(CODE_BLOCK_BG);
    vec![
        Span::styled("▎ ", border),
        Span::styled(lang.to_string(), label),
    ]
}

fn hr_line(bg: Option<Color>) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(32),
        style_with_bg(Style::default().fg(HR_FG), bg),
    ))
}

pub fn md_options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_GFM
}

fn alert_color(kind: &BlockQuoteKind) -> Color {
    match kind {
        BlockQuoteKind::Note => ALERT_NOTE_FG,
        BlockQuoteKind::Tip => ALERT_TIP_FG,
        BlockQuoteKind::Important => ALERT_IMPORTANT_FG,
        BlockQuoteKind::Warning => ALERT_WARNING_FG,
        BlockQuoteKind::Caution => ALERT_CAUTION_FG,
    }
}

fn alert_label(kind: &BlockQuoteKind) -> &'static str {
    match kind {
        BlockQuoteKind::Note => "Note",
        BlockQuoteKind::Tip => "Tip",
        BlockQuoteKind::Important => "Important",
        BlockQuoteKind::Warning => "Warning",
        BlockQuoteKind::Caution => "Caution",
    }
}

fn alert_icon(kind: &BlockQuoteKind) -> &'static str {
    match kind {
        BlockQuoteKind::Note => "\u{2139}\u{fe0f}",
        BlockQuoteKind::Tip => "\u{1f4a1}",
        BlockQuoteKind::Important => "\u{2757}",
        BlockQuoteKind::Warning => "\u{26a0}\u{fe0f}",
        BlockQuoteKind::Caution => "\u{1f534}",
    }
}

fn is_mermaid_block(kind: &CodeBlockKind) -> bool {
    matches!(kind, CodeBlockKind::Fenced(lang) if lang.split_whitespace().next() == Some("mermaid"))
}

/// Settings for one document. Rendering does not retain settings between calls.
#[derive(Clone, Copy)]
pub struct RenderOptions<'a> {
    /// Content width in terminal cells. Zero leaves text and diagrams unconstrained;
    /// tables use `MARKDOWN_WRAP_WIDTH` in that case.
    pub width: usize,
    pub base_fg: Color,
    pub bg: Option<Color>,
    pub syntax_ctx: Option<(&'a SyntaxSet, &'a Theme)>,
    pub repo_base_url: Option<&'a str>,
    /// Source line ranges emphasized by `render_lines` and `layout_lines`.
    pub highlighted_lines: &'a [RangeInclusive<u64>],
    pub highlight_style: Style,
}

impl Default for RenderOptions<'_> {
    fn default() -> Self {
        Self {
            width: MARKDOWN_WRAP_WIDTH,
            base_fg: Color::Rgb(200, 205, 220),
            bg: None,
            syntax_ctx: None,
            repo_base_url: None,
            highlighted_lines: &[],
            highlight_style: Style::default(),
        }
    }
}

const MERMAID_CACHE_CAPACITY: usize = 64;
const MERMAID_CACHE_ENTRY_BYTES: usize = 1024 * 1024;
type MermaidCacheEntry = ((String, usize), Result<String, String>);

/// Reusable renderer. Only successful and failed Mermaid layouts are cached.
#[derive(Default)]
pub struct Renderer {
    mermaid_cache: VecDeque<MermaidCacheEntry>,
}

impl Renderer {
    /// Render without wrapping ordinary text; tables and diagrams use `options.width`.
    pub fn render_lines(&mut self, text: &str, options: &RenderOptions<'_>) -> Vec<Line<'static>> {
        markdown_to_lines_inner(self, text, options, false)
    }

    /// Preserve images and GitHub snippets as separate blocks for the caller to resolve.
    pub fn render_blocks(&mut self, text: &str, options: &RenderOptions<'_>) -> Vec<ContentBlock> {
        markdown_to_content_blocks(self, text, options, false)
    }

    /// Render and wrap text while preserving table and Mermaid geometry.
    pub fn layout_lines(&mut self, text: &str, options: &RenderOptions<'_>) -> Vec<Line<'static>> {
        markdown_to_lines_inner(self, text, options, true)
    }

    /// Lay out text and retain media blocks in document order.
    pub fn layout_blocks(&mut self, text: &str, options: &RenderOptions<'_>) -> Vec<ContentBlock> {
        markdown_to_content_blocks(self, text, options, true)
    }

    /// Count the exact rows returned by `layout_lines` at the requested width.
    pub fn line_count(&mut self, text: &str, width: usize, options: &RenderOptions<'_>) -> usize {
        self.layout_lines(text, &RenderOptions { width, ..*options })
            .len()
    }

    /// Estimate rows without syntax highlighting or allocating the rendered document.
    /// Table cells, emoji shortcodes, and other inline formatting can change the exact count.
    pub fn estimate_line_count(
        &mut self,
        text: &str,
        width: usize,
        options: &RenderOptions<'_>,
    ) -> usize {
        estimate_markdown_line_count(self, text, width, options)
    }

    fn mermaid_to_ascii(&mut self, code: &str, width: usize) -> Result<String, String> {
        if let Some((_, result)) = self
            .mermaid_cache
            .iter()
            .find(|((source, cached_width), _)| source == code && *cached_width == width)
        {
            return result.clone();
        }
        let result =
            ma::render_with_options(code, (width > 0).then_some(width)).and_then(|output| {
                if output.is_empty() {
                    Err("empty Mermaid output".to_string())
                } else {
                    Ok(output)
                }
            });
        let result_bytes = result
            .as_ref()
            .map_or_else(|error| error.len(), |output| output.len());
        if code.len().saturating_add(result_bytes) <= MERMAID_CACHE_ENTRY_BYTES {
            if self.mermaid_cache.len() == MERMAID_CACHE_CAPACITY {
                self.mermaid_cache.pop_front();
            }
            self.mermaid_cache
                .push_back(((code.to_owned(), width), result.clone()));
        }
        result
    }
}

/// Split text on `#\d+` patterns and push spans with link style for matches.
/// Only activates when repo_base_url is non-empty.
fn push_spans_with_issue_refs(
    text: &str,
    normal_style: Style,
    link_style: Style,
    spans: &mut Vec<Span<'static>>,
    repo_base_url: Option<&str>,
) {
    if repo_base_url.is_none_or(str::is_empty) {
        spans.push(Span::styled(text.to_string(), normal_style));
        return;
    }
    let mut last = 0;
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        if bytes[i] == b'#' && i + 1 < len && bytes[i + 1].is_ascii_digit() {
            // Check word boundary before #
            if i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
                i += 1;
                continue;
            }
            let start = i;
            i += 1; // skip #
            while i < len && bytes[i].is_ascii_digit() {
                i += 1;
            }
            // Check word boundary after digits
            if i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                continue;
            }
            // Push text before the match
            if start > last {
                spans.push(Span::styled(text[last..start].to_string(), normal_style));
            }
            spans.push(Span::styled(text[start..i].to_string(), link_style));
            last = i;
        } else {
            i += 1;
        }
    }
    if last < len {
        spans.push(Span::styled(text[last..].to_string(), normal_style));
    }
}

fn mermaid_unavailable_line(error: &str, style: Style) -> Line<'static> {
    Line::from(Span::styled(
        format!("[mermaid unavailable: {error}]"),
        style,
    ))
}

#[derive(Clone, Debug, PartialEq)]
pub enum ContentBlock {
    Text(Line<'static>),
    Image {
        url: String,
        alt: String,
    },
    Video {
        url: String,
        alt: String,
    },
    CodeSnippet {
        url: String,
        path: String,
        start_line: u64,
        end_line: Option<u64>,
    },
    Suggestion {
        lines: Vec<String>,
    },
}

fn estimate_markdown_line_count(
    renderer: &mut Renderer,
    text: &str,
    wrap_width: usize,
    options: &RenderOptions<'_>,
) -> usize {
    use unicode_width::UnicodeWidthStr;

    let wrap_lines = |text_width: usize| -> usize {
        if wrap_width == 0 || text_width <= wrap_width {
            1
        } else {
            text_width.div_ceil(wrap_width)
        }
    };

    let mut count: usize = 0;
    let mut has_pending = false;
    let mut pending_width: usize = 0;
    let mut in_code_block = false;
    let mut in_mermaid = false;
    let mut mermaid_count_buf = String::new();
    let mut table_data_row_index: usize = 0;
    let mut list_depth: usize = 0;
    // Whether the last committed line had content (for adding block separators)
    let mut last_was_content = false;

    let parser = Parser::new_ext(text, md_options());
    for event in parser {
        match event {
            Event::End(TagEnd::Heading(_))
            | Event::End(TagEnd::Item)
            | Event::End(TagEnd::Paragraph) => {
                if has_pending {
                    count += wrap_lines(pending_width);
                    last_was_content = true;
                }
                has_pending = false;
                pending_width = 0;
            }
            Event::Start(Tag::Paragraph) if list_depth == 0 && last_was_content => {
                count += 1; // block separator
            }
            Event::Start(Tag::Heading { .. }) if last_was_content => {
                count += 1; // block separator
            }
            Event::Start(Tag::CodeBlock(ref kind)) => {
                if last_was_content {
                    count += 1; // block separator
                }
                in_mermaid = is_mermaid_block(kind);
                in_code_block = true;
                if code_fence_lang(kind).is_some() {
                    count += 1; // language label line
                    last_was_content = true;
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if in_mermaid {
                    match renderer.mermaid_to_ascii(&mermaid_count_buf, options.width) {
                        Ok(aa) => count += aa.lines().count(),
                        Err(_) => count += 1,
                    }
                    mermaid_count_buf.clear();
                    has_pending = false;
                    pending_width = 0;
                    last_was_content = true;
                }
                in_code_block = false;
                in_mermaid = false;
            }
            Event::Start(Tag::BlockQuote(ref kind)) => {
                if last_was_content {
                    count += 1; // block separator
                }
                if kind.is_some() {
                    count += 1; // alert label line
                    last_was_content = true;
                }
            }
            Event::End(TagEnd::BlockQuote(_)) => {}
            Event::Start(Tag::List(_)) => {
                if list_depth == 0 && last_was_content {
                    count += 1; // block separator
                } else if list_depth > 0 && has_pending {
                    count += wrap_lines(pending_width);
                    has_pending = false;
                    pending_width = 0;
                    last_was_content = true;
                }
                list_depth += 1;
            }
            Event::End(TagEnd::List(_)) => {
                list_depth = list_depth.saturating_sub(1);
            }
            Event::Start(Tag::Table(_)) if last_was_content => {
                count += 1; // block separator
            }
            Event::Start(Tag::Item) => {
                has_pending = true;
                pending_width += list_marker_width(list_depth, None);
            }
            Event::TaskListMarker(_) => {
                pending_width += 2; // checkbox
            }
            Event::Code(t) => {
                has_pending = true;
                pending_width += UnicodeWidthStr::width(t.as_ref()) + 2; // backticks
            }
            Event::Text(t) => {
                if in_mermaid {
                    mermaid_count_buf.push_str(t.as_ref());
                } else if in_code_block {
                    count += t.as_ref().lines().count().max(1);
                    has_pending = false;
                    pending_width = 0;
                    last_was_content = true;
                } else {
                    has_pending = true;
                    pending_width += UnicodeWidthStr::width(t.as_ref());
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if has_pending {
                    count += wrap_lines(pending_width);
                    last_was_content = true;
                } else {
                    count += 1;
                }
                has_pending = false;
                pending_width = 0;
            }
            Event::End(TagEnd::Image) => {
                count += 1; // placeholder line (actual height depends on load state)
                has_pending = false;
                pending_width = 0;
                last_was_content = true;
            }
            Event::End(TagEnd::TableHead) => {
                count += 1; // top border
                count += 1; // header row
                count += 1; // separator line below header
                table_data_row_index = 0;
                has_pending = false;
                pending_width = 0;
                last_was_content = true;
            }
            Event::End(TagEnd::TableRow) => {
                if table_data_row_index > 0 {
                    count += 1; // separator between data rows
                }
                count += 1; // data row
                table_data_row_index += 1;
                has_pending = false;
                pending_width = 0;
                last_was_content = true;
            }
            Event::End(TagEnd::Table) => {
                count += 1; // bottom border
                has_pending = false;
                pending_width = 0;
                last_was_content = true;
            }
            Event::FootnoteReference(ref name) => {
                has_pending = true;
                pending_width += name.len() + 2; // [name]
            }
            Event::Start(Tag::FootnoteDefinition(_)) => {
                if last_was_content {
                    count += 1; // block separator
                }
                has_pending = true;
            }
            Event::End(TagEnd::FootnoteDefinition) => {
                if has_pending {
                    count += wrap_lines(pending_width);
                    last_was_content = true;
                }
                has_pending = false;
                pending_width = 0;
            }
            Event::Rule => {
                if has_pending {
                    count += wrap_lines(pending_width);
                    has_pending = false;
                    pending_width = 0;
                }
                count += 1; // rule line
                last_was_content = true;
            }
            _ => {}
        }
    }
    if has_pending {
        count += wrap_lines(pending_width);
    }
    count.max(1)
}

pub fn replace_emoji_shortcodes(text: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if !text.contains(':') {
        return Cow::Borrowed(text);
    }
    let mut result = String::new();
    let mut last_end = 0;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b':'
            && let Some(close_offset) = text[i + 1..].find(':')
        {
            let shortcode = &text[i + 1..i + 1 + close_offset];
            if !shortcode.is_empty()
                && shortcode.len() <= 50
                && shortcode
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'+' || b == b'-')
                && let Some(emoji) = emoji_for_shortcode(shortcode)
            {
                if result.is_empty() {
                    result.reserve(text.len());
                }
                result.push_str(&text[last_end..i]);
                result.push_str(emoji);
                last_end = i + 1 + close_offset + 1;
                i = last_end;
                continue;
            }
        }
        i += 1;
    }
    if last_end == 0 {
        Cow::Borrowed(text)
    } else {
        result.push_str(&text[last_end..]);
        Cow::Owned(result)
    }
}

fn emoji_for_shortcode(shortcode: &str) -> Option<&'static str> {
    let lookup = match shortcode {
        "octocat" => "octopus",
        _ => shortcode,
    };
    emojis::get_by_shortcode(lookup).map(|emoji| emoji.as_str())
}

pub fn heading_style(level: HeadingLevel, bg: Option<Color>) -> Style {
    let style = match level {
        HeadingLevel::H1 => Style::default()
            .fg(Color::Rgb(255, 255, 255))
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        HeadingLevel::H2 => Style::default()
            .fg(Color::Rgb(230, 237, 243))
            .add_modifier(Modifier::BOLD),
        HeadingLevel::H3 => Style::default()
            .fg(Color::Rgb(201, 209, 217))
            .add_modifier(Modifier::BOLD),
        HeadingLevel::H4 => Style::default()
            .fg(Color::Rgb(139, 148, 158))
            .add_modifier(Modifier::BOLD),
        _ => Style::default()
            .fg(Color::Rgb(110, 118, 129))
            .add_modifier(Modifier::BOLD),
    };
    style_with_bg(style, bg)
}

fn cell_display_width(spans: &[Span<'static>]) -> usize {
    Line::from(spans.to_vec()).width()
}

pub fn is_rendered_table_line(line: &Line<'_>) -> bool {
    line.spans.first().is_some_and(|span| {
        span.style.fg == Some(TABLE_BORDER_FG)
            && span
                .content
                .chars()
                .next()
                .is_some_and(|first| matches!(first, '┌' | '├' | '└' | '│'))
    })
}

fn fit_table_column_widths(col_widths: &mut [usize], min_widths: &[usize], max_width: usize) {
    let border_width = col_widths.len().saturating_mul(3).saturating_add(1);
    let content_budget = max_width.saturating_sub(border_width);
    while col_widths.iter().sum::<usize>() > content_budget {
        let Some((widest, _)) = col_widths
            .iter()
            .enumerate()
            .filter(|(index, width)| **width > min_widths[*index])
            .max_by_key(|(_, width)| **width)
        else {
            break;
        };
        col_widths[widest] -= 1;
    }
}

fn render_table_to_lines(
    header_cells: &[Vec<Span<'static>>],
    data_rows: &[Vec<Vec<Span<'static>>>],
    base: Style,
    bg: Option<Color>,
    width: usize,
) -> Vec<Line<'static>> {
    let col_count = header_cells.len();
    if col_count == 0 {
        return Vec::new();
    }

    use unicode_width::UnicodeWidthChar;

    let width = if width == 0 {
        MARKDOWN_WRAP_WIDTH
    } else {
        width.min(MARKDOWN_WRAP_WIDTH)
    };
    let mut col_widths = vec![0; col_count];
    let mut min_widths = vec![0; col_count];
    for row in std::iter::once(header_cells).chain(data_rows.iter().map(Vec::as_slice)) {
        for (index, cell) in row.iter().take(col_count).enumerate() {
            col_widths[index] = col_widths[index].max(cell_display_width(cell));
            let minimum = cell
                .iter()
                .flat_map(|span| span.content.chars())
                .filter_map(UnicodeWidthChar::width)
                .max()
                .unwrap_or(0);
            min_widths[index] = min_widths[index].max(minimum);
        }
    }
    let min_table_width = col_count
        .saturating_mul(3)
        .saturating_add(1)
        .saturating_add(min_widths.iter().sum::<usize>());
    if min_table_width > width {
        return render_stacked_table(header_cells, data_rows, base, width);
    }
    fit_table_column_widths(&mut col_widths, &min_widths, width);

    let sep_style = style_with_bg(Style::default().fg(TABLE_BORDER_FG), bg);

    let build_row = |cells: &[Vec<Span<'static>>], is_header: bool| -> Vec<Line<'static>> {
        let pad_style = if is_header {
            base.add_modifier(Modifier::BOLD)
        } else {
            base
        };
        let wrapped_cells: Vec<Vec<Line<'static>>> = col_widths
            .iter()
            .enumerate()
            .map(|(index, width)| {
                cells.get(index).map_or_else(
                    || vec![Line::default()],
                    |spans| wrap_line(Line::from(spans.clone()), *width),
                )
            })
            .collect();
        let row_height = wrapped_cells.iter().map(Vec::len).max().unwrap_or(1);
        let mut rows = Vec::with_capacity(row_height);
        for row_index in 0..row_height {
            let mut spans = Vec::new();
            for (i, col_w) in col_widths.iter().enumerate() {
                spans.push(Span::styled(if i == 0 { "│ " } else { " │ " }, sep_style));
                let cell_line = wrapped_cells[i].get(row_index);
                let cell_w = cell_line.map(Line::width).unwrap_or(0);
                if let Some(cell_line) = cell_line {
                    spans.extend(cell_line.spans.iter().cloned());
                }
                let pad = col_w.saturating_sub(cell_w);
                if pad > 0 {
                    spans.push(Span::styled(" ".repeat(pad), pad_style));
                }
            }
            spans.push(Span::styled(" │", sep_style));
            rows.push(Line::from(spans));
        }
        rows
    };

    let build_separator = |left: char, mid: char, right: char| -> Line<'static> {
        let mut s = String::new();
        for (i, &w) in col_widths.iter().enumerate() {
            s.push(if i == 0 { left } else { mid });
            for _ in 0..(w + 2) {
                s.push('─');
            }
        }
        s.push(right);
        Line::from(Span::styled(s, sep_style))
    };

    let mut lines = Vec::new();
    lines.push(build_separator('┌', '┬', '┐'));
    lines.extend(build_row(header_cells, true));
    lines.push(build_separator('├', '┼', '┤'));

    for (i, row) in data_rows.iter().enumerate() {
        if i > 0 {
            lines.push(build_separator('├', '┼', '┤'));
        }
        lines.extend(build_row(row, false));
    }

    lines.push(build_separator('└', '┴', '┘'));
    lines
}

fn render_stacked_table(
    header_cells: &[Vec<Span<'static>>],
    data_rows: &[Vec<Vec<Span<'static>>>],
    base: Style,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if data_rows.is_empty() {
        for header in header_cells {
            lines.extend(wrap_line(Line::from(header.clone()), width));
        }
        return lines;
    }
    for (row_index, row) in data_rows.iter().enumerate() {
        if row_index > 0 {
            lines.push(Line::default());
        }
        for (column, header) in header_cells.iter().enumerate() {
            let mut spans = header.clone();
            spans.push(Span::styled(": ", base));
            if let Some(cell) = row.get(column) {
                spans.extend(cell.iter().cloned());
            }
            lines.extend(wrap_line(Line::from(spans), width));
        }
    }
    lines
}

fn highlighted_byte_ranges(
    text: &str,
    highlighted_lines: &[RangeInclusive<u64>],
) -> Vec<Range<usize>> {
    let mut starts = vec![0usize];
    starts.extend(
        text.bytes()
            .enumerate()
            .filter_map(|(index, byte)| (byte == b'\n').then_some(index + 1)),
    );
    highlighted_lines
        .iter()
        .filter_map(|lines| {
            let start = usize::try_from(*lines.start()).ok()?.checked_sub(1)?;
            let end = usize::try_from(*lines.end()).ok()?;
            let start_byte = *starts.get(start)?;
            let end_byte = starts.get(end).copied().unwrap_or(text.len());
            Some(start_byte..end_byte)
        })
        .collect()
}

fn markdown_to_lines_inner(
    renderer: &mut Renderer,
    text: &str,
    options: &RenderOptions<'_>,
    wrap: bool,
) -> Vec<Line<'static>> {
    let RenderOptions {
        base_fg,
        bg,
        syntax_ctx,
        highlighted_lines,
        highlight_style,
        ..
    } = *options;
    let mut protected_lines: Vec<Range<usize>> = Vec::new();
    let mermaid_width = if wrap && options.width > 0 {
        options.width.saturating_sub(2).max(1)
    } else {
        options.width
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut bold = false;
    let mut italic = false;
    let mut strikethrough = false;
    let mut in_code_block = false;
    let mut in_mermaid = false;
    let mut mermaid_buf = String::new();
    let mut code_syntax: Option<&SyntaxReference> = None;
    let mut heading_level: Option<HeadingLevel> = None;
    let mut in_link = false;
    let mut in_blockquote = false;
    let mut alert_kind: Option<BlockQuoteKind> = None;
    let mut ordered_list_stack: Vec<Option<u64>> = Vec::new();
    let mut table_header_cells: Vec<Vec<Span<'static>>> = Vec::new();
    let mut table_data_rows: Vec<Vec<Vec<Span<'static>>>> = Vec::new();
    let mut table_current_row: Vec<Vec<Span<'static>>> = Vec::new();

    let base = {
        let mut s = Style::default().fg(base_fg);
        if let Some(b) = bg {
            s = s.bg(b);
        }
        s
    };

    let flush_line = |spans: &mut Vec<Span<'static>>, lines: &mut Vec<Line<'static>>| {
        lines.push(Line::from(std::mem::take(spans)));
    };

    let mut list_depth: usize = 0;

    let add_block_separator = |lines: &mut Vec<Line<'static>>, base: Style| {
        if !lines.is_empty() && !lines.last().is_none_or(|l| l.spans.is_empty()) {
            lines.push(Line::from(Span::styled(String::new(), base)));
        }
    };

    let highlighted_bytes = highlighted_byte_ranges(text, highlighted_lines);
    let parser = Parser::new_ext(text, md_options()).into_offset_iter();
    for (event, source_range) in parser {
        let highlighted = highlighted_bytes
            .iter()
            .any(|range| range.start < source_range.end && range.end > source_range.start);
        let patch_generated_lines = highlighted && !matches!(event, Event::End(TagEnd::Table));
        let lines_before = lines.len();
        let spans_before = current_spans.len();
        match event {
            Event::Start(Tag::Strikethrough) => strikethrough = true,
            Event::End(TagEnd::Strikethrough) => strikethrough = false,
            Event::Start(Tag::Table(_)) => {
                add_block_separator(&mut lines, base);
                table_header_cells.clear();
                table_data_rows.clear();
                table_current_row.clear();
            }
            Event::End(TagEnd::Table) => {
                lines.extend(render_table_to_lines(
                    &table_header_cells,
                    &table_data_rows,
                    base,
                    bg,
                    options.width,
                ));
            }
            Event::Start(Tag::TableHead) => {
                table_current_row.clear();
                bold = true;
            }
            Event::End(TagEnd::TableHead) => {
                table_header_cells = std::mem::take(&mut table_current_row);
                bold = false;
            }
            Event::Start(Tag::TableRow) => {
                table_current_row.clear();
            }
            Event::End(TagEnd::TableRow) => {
                table_data_rows.push(std::mem::take(&mut table_current_row));
            }
            Event::Start(Tag::TableCell) => {}
            Event::End(TagEnd::TableCell) => {
                table_current_row.push(std::mem::take(&mut current_spans));
            }
            Event::Start(Tag::Heading { level, .. }) => {
                add_block_separator(&mut lines, base);
                heading_level = Some(level);
                bold = true;
            }
            Event::End(TagEnd::Heading(_)) => {
                heading_level = None;
                bold = false;
                flush_line(&mut current_spans, &mut lines);
            }
            Event::Start(Tag::Emphasis) => italic = true,
            Event::End(TagEnd::Emphasis) => italic = false,
            Event::Start(Tag::Strong) => bold = true,
            Event::End(TagEnd::Strong) => bold = false,
            Event::Start(Tag::CodeBlock(ref kind)) => {
                add_block_separator(&mut lines, base);
                in_mermaid = is_mermaid_block(kind);
                if in_mermaid {
                    mermaid_buf.clear();
                }
                in_code_block = true;
                if let Some(lang) = code_fence_lang(kind) {
                    current_spans.extend(code_lang_label_spans(lang));
                    flush_line(&mut current_spans, &mut lines);
                }
                code_syntax = syntax_ctx.and_then(|(ss, _)| {
                    if let CodeBlockKind::Fenced(lang) = kind {
                        let token = lang.split_whitespace().next().unwrap_or("");
                        if !token.is_empty() {
                            return ss.find_syntax_by_token(token);
                        }
                    }
                    None
                });
            }
            Event::End(TagEnd::CodeBlock) => {
                if in_mermaid {
                    match renderer.mermaid_to_ascii(&mermaid_buf, mermaid_width) {
                        Ok(aa) => {
                            let start = lines.len();
                            let border_style =
                                Style::default().fg(CODE_BLOCK_BORDER_FG).bg(CODE_BLOCK_BG);
                            let text_style = Style::default().fg(CODE_BLOCK_FG).bg(CODE_BLOCK_BG);
                            for aa_line in aa.lines() {
                                current_spans.push(Span::styled("▎ ", border_style));
                                current_spans.push(Span::styled(aa_line.to_string(), text_style));
                                flush_line(&mut current_spans, &mut lines);
                            }
                            protected_lines.push(start..lines.len());
                        }
                        Err(error) => {
                            let style = Style::default().fg(Color::Rgb(114, 113, 105));
                            lines.push(mermaid_unavailable_line(&error, style));
                        }
                    }
                    mermaid_buf.clear();
                }
                in_code_block = false;
                in_mermaid = false;
                code_syntax = None;
            }
            Event::Start(Tag::Link { .. }) => in_link = true,
            Event::End(TagEnd::Link) => in_link = false,
            Event::Start(Tag::BlockQuote(kind)) => {
                add_block_separator(&mut lines, base);
                in_blockquote = true;
                alert_kind = kind;
                if let Some(ref ak) = alert_kind {
                    let fg = alert_color(ak);
                    let border_style = Style::default().fg(fg);
                    let label_style = Style::default().fg(fg).add_modifier(Modifier::BOLD);
                    current_spans.push(Span::styled("▎ ", border_style));
                    current_spans.push(Span::styled(
                        format!("{} {}", alert_icon(ak), alert_label(ak)),
                        label_style,
                    ));
                    flush_line(&mut current_spans, &mut lines);
                }
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                in_blockquote = false;
                alert_kind = None;
            }
            Event::Start(Tag::List(start)) => {
                if list_depth == 0 {
                    add_block_separator(&mut lines, base);
                } else if !current_spans.is_empty() {
                    // Tight list: parent item text isn't wrapped in a paragraph,
                    // so flush it before nested markers append to the same line.
                    flush_line(&mut current_spans, &mut lines);
                }
                list_depth += 1;
                ordered_list_stack.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                list_depth = list_depth.saturating_sub(1);
                ordered_list_stack.pop();
            }
            Event::Start(Tag::Item) => {
                if in_blockquote {
                    let bq_fg = alert_kind
                        .as_ref()
                        .map_or(BLOCKQUOTE_BORDER_FG, alert_color);
                    current_spans.push(Span::styled(
                        "▎ ",
                        style_with_bg(Style::default().fg(bq_fg), bg),
                    ));
                }
                let ordered = ordered_list_stack.last_mut().and_then(|c| c.as_mut());
                push_list_marker(&mut current_spans, list_depth, ordered, bg);
            }
            Event::End(TagEnd::Item) if !current_spans.is_empty() => {
                flush_line(&mut current_spans, &mut lines);
            }
            Event::TaskListMarker(checked) => {
                push_task_marker(&mut current_spans, checked, bg);
            }
            Event::Start(Tag::Paragraph) => {
                if list_depth == 0 {
                    add_block_separator(&mut lines, base);
                }
                if in_blockquote && current_spans.is_empty() {
                    let bq_fg = alert_kind
                        .as_ref()
                        .map_or(BLOCKQUOTE_BORDER_FG, alert_color);
                    current_spans.push(Span::styled(
                        "▎ ",
                        style_with_bg(Style::default().fg(bq_fg), bg),
                    ));
                }
            }
            Event::End(TagEnd::Paragraph) => {
                flush_line(&mut current_spans, &mut lines);
            }
            Event::Code(code) => {
                let style = Style::default().fg(INLINE_CODE_FG).bg(INLINE_CODE_BG);
                current_spans.push(Span::styled(format!(" {code} "), style));
            }
            Event::Text(t) => {
                if in_mermaid {
                    mermaid_buf.push_str(t.as_ref());
                } else if in_code_block {
                    for code_line in t.as_ref().lines() {
                        let border_style =
                            Style::default().fg(CODE_BLOCK_BORDER_FG).bg(CODE_BLOCK_BG);
                        current_spans.push(Span::styled("▎ ", border_style));
                        if let (Some(syn), Some((ss, theme))) = (code_syntax, syntax_ctx) {
                            current_spans.extend(highlight_content_inner(
                                code_line,
                                syn,
                                ss,
                                theme,
                                Some(CODE_BLOCK_BG),
                                false,
                            ));
                        } else {
                            let style = Style::default().fg(CODE_BLOCK_FG).bg(CODE_BLOCK_BG);
                            current_spans.push(Span::styled(code_line.to_string(), style));
                        }
                        flush_line(&mut current_spans, &mut lines);
                    }
                } else {
                    let resolved = replace_emoji_shortcodes(t.as_ref());
                    if in_link {
                        let style = style_with_bg(
                            Style::default()
                                .fg(LINK_FG)
                                .add_modifier(Modifier::UNDERLINED),
                            bg,
                        );
                        current_spans.push(Span::styled(resolved.to_string(), style));
                    } else if in_blockquote {
                        let mut style = style_with_bg(Style::default().fg(BLOCKQUOTE_FG), bg);
                        if bold {
                            style = style.add_modifier(Modifier::BOLD);
                        }
                        if italic {
                            style = style.add_modifier(Modifier::ITALIC);
                        }
                        current_spans.push(Span::styled(resolved.to_string(), style));
                    } else {
                        let mut style = if let Some(level) = heading_level {
                            heading_style(level, bg)
                        } else {
                            base
                        };
                        if bold && heading_level.is_none() {
                            style = style.add_modifier(Modifier::BOLD);
                        }
                        if italic {
                            style = style.add_modifier(Modifier::ITALIC);
                        }
                        if strikethrough {
                            style = style.add_modifier(Modifier::CROSSED_OUT);
                        }
                        let link_style = style_with_bg(
                            Style::default()
                                .fg(LINK_FG)
                                .add_modifier(Modifier::UNDERLINED),
                            bg,
                        );
                        push_spans_with_issue_refs(
                            &resolved,
                            style,
                            link_style,
                            &mut current_spans,
                            options.repo_base_url,
                        );
                    }
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                flush_line(&mut current_spans, &mut lines);
                if in_blockquote {
                    let bq_fg = alert_kind
                        .as_ref()
                        .map_or(BLOCKQUOTE_BORDER_FG, alert_color);
                    current_spans.push(Span::styled(
                        "▎ ",
                        style_with_bg(Style::default().fg(bq_fg), bg),
                    ));
                }
            }
            Event::FootnoteReference(name) => {
                let style = style_with_bg(
                    Style::default()
                        .fg(LINK_FG)
                        .add_modifier(Modifier::UNDERLINED),
                    bg,
                );
                current_spans.push(Span::styled(format!("[{name}]"), style));
            }
            Event::Start(Tag::FootnoteDefinition(name)) => {
                add_block_separator(&mut lines, base);
                let style = style_with_bg(Style::default().fg(LINK_FG), bg);
                current_spans.push(Span::styled(format!("[{name}]: "), style));
            }
            Event::End(TagEnd::FootnoteDefinition) => {
                flush_line(&mut current_spans, &mut lines);
            }
            Event::Rule => {
                flush_line(&mut current_spans, &mut lines);
                lines.push(hr_line(bg));
            }
            _ => {}
        }
        if highlighted {
            for span in current_spans.iter_mut().skip(spans_before) {
                span.style = span.style.patch(highlight_style);
            }
        }
        if patch_generated_lines {
            for line in lines.iter_mut().skip(lines_before) {
                if line.spans.iter().all(|span| span.content.is_empty()) {
                    continue;
                }
                for span in &mut line.spans {
                    span.style = span.style.patch(highlight_style);
                }
            }
        }
    }
    if !current_spans.is_empty() {
        flush_line(&mut current_spans, &mut lines);
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(String::new(), base)));
    }
    if wrap {
        lines
            .into_iter()
            .enumerate()
            .flat_map(|(index, line)| {
                if protected_lines.iter().any(|range| range.contains(&index))
                    || is_rendered_table_line(&line)
                {
                    vec![line]
                } else {
                    wrap_line(line, options.width)
                }
            })
            .collect()
    } else {
        lines
    }
}

fn markdown_to_content_blocks(
    renderer: &mut Renderer,
    text: &str,
    options: &RenderOptions<'_>,
    wrap: bool,
) -> Vec<ContentBlock> {
    let RenderOptions {
        base_fg,
        bg,
        syntax_ctx,
        ..
    } = *options;
    let mut protected_blocks: Vec<Range<usize>> = Vec::new();
    let mermaid_width = if wrap && options.width > 0 {
        options.width.saturating_sub(2).max(1)
    } else {
        options.width
    };
    let mut blocks: Vec<ContentBlock> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut bold = false;
    let mut italic = false;
    let mut strikethrough = false;
    let mut in_code_block = false;
    let mut in_mermaid = false;
    let mut code_syntax: Option<&SyntaxReference> = None;
    let mut mermaid_buf = String::new();
    let mut heading_level: Option<HeadingLevel> = None;
    let mut in_suggestion = false;
    let mut suggestion_lines: Vec<String> = Vec::new();
    let mut in_image = false;
    let mut image_url = String::new();
    let mut image_alt = String::new();
    let mut in_link = false;
    let mut blob_link: Option<BlobUrlInfo> = None;
    let mut media_link_kind: Option<MediaKind> = None;
    let mut media_link_alt = String::new();
    let mut link_url_raw = String::new();
    let mut in_blockquote = false;
    let mut alert_kind: Option<BlockQuoteKind> = None;
    let mut ordered_list_stack: Vec<Option<u64>> = Vec::new();
    let mut table_header_cells: Vec<Vec<Span<'static>>> = Vec::new();
    let mut table_data_rows: Vec<Vec<Vec<Span<'static>>>> = Vec::new();
    let mut table_current_row: Vec<Vec<Span<'static>>> = Vec::new();

    let base = {
        let mut s = Style::default().fg(base_fg);
        if let Some(b) = bg {
            s = s.bg(b);
        }
        s
    };

    let flush_line = |spans: &mut Vec<Span<'static>>, blocks: &mut Vec<ContentBlock>| {
        blocks.push(ContentBlock::Text(Line::from(std::mem::take(spans))));
    };

    let mut list_depth: usize = 0;

    let add_block_separator_cb = |blocks: &mut Vec<ContentBlock>, base: Style| {
        let last_non_empty = blocks.last().is_some_and(|b| match b {
            ContentBlock::Text(line) => !line.spans.is_empty(),
            ContentBlock::Image { .. }
            | ContentBlock::Video { .. }
            | ContentBlock::CodeSnippet { .. }
            | ContentBlock::Suggestion { .. } => true,
        });
        if !blocks.is_empty() && last_non_empty {
            blocks.push(ContentBlock::Text(Line::from(Span::styled(
                String::new(),
                base,
            ))));
        }
    };

    let parser = Parser::new_ext(text, md_options());
    for event in parser {
        match event {
            Event::Start(Tag::Image { dest_url, .. }) => {
                if !current_spans.is_empty() {
                    flush_line(&mut current_spans, &mut blocks);
                }
                in_image = true;
                image_url = dest_url.to_string();
                image_alt.clear();
            }
            Event::End(TagEnd::Image) => {
                in_image = false;
                blocks.push(ContentBlock::Image {
                    url: std::mem::take(&mut image_url),
                    alt: std::mem::take(&mut image_alt),
                });
            }
            _ if in_image => {
                if let Event::Text(t) = &event {
                    image_alt.push_str(t.as_ref());
                }
            }
            Event::Start(Tag::Strikethrough) => strikethrough = true,
            Event::End(TagEnd::Strikethrough) => strikethrough = false,
            Event::Start(Tag::Table(_)) => {
                add_block_separator_cb(&mut blocks, base);
                table_header_cells.clear();
                table_data_rows.clear();
                table_current_row.clear();
            }
            Event::End(TagEnd::Table) => {
                for line in render_table_to_lines(
                    &table_header_cells,
                    &table_data_rows,
                    base,
                    bg,
                    options.width,
                ) {
                    blocks.push(ContentBlock::Text(line));
                }
            }
            Event::Start(Tag::TableHead) => {
                table_current_row.clear();
                bold = true;
            }
            Event::End(TagEnd::TableHead) => {
                table_header_cells = std::mem::take(&mut table_current_row);
                bold = false;
            }
            Event::Start(Tag::TableRow) => {
                table_current_row.clear();
            }
            Event::End(TagEnd::TableRow) => {
                table_data_rows.push(std::mem::take(&mut table_current_row));
            }
            Event::Start(Tag::TableCell) => {}
            Event::End(TagEnd::TableCell) => {
                table_current_row.push(std::mem::take(&mut current_spans));
            }
            Event::Start(Tag::Heading { level, .. }) => {
                add_block_separator_cb(&mut blocks, base);
                heading_level = Some(level);
                bold = true;
            }
            Event::End(TagEnd::Heading(_)) => {
                heading_level = None;
                bold = false;
                flush_line(&mut current_spans, &mut blocks);
            }
            Event::Start(Tag::Emphasis) => italic = true,
            Event::End(TagEnd::Emphasis) => italic = false,
            Event::Start(Tag::Strong) => bold = true,
            Event::End(TagEnd::Strong) => bold = false,
            Event::Start(Tag::CodeBlock(ref kind)) => {
                add_block_separator_cb(&mut blocks, base);
                in_mermaid = is_mermaid_block(kind);
                if in_mermaid {
                    mermaid_buf.clear();
                }
                in_suggestion = matches!(kind, CodeBlockKind::Fenced(lang) if lang.split_whitespace().next().map(|t| t.to_lowercase()) == Some("suggestion".to_string()));
                if in_suggestion {
                    suggestion_lines.clear();
                }
                in_code_block = true;
                if !in_suggestion && let Some(lang) = code_fence_lang(kind) {
                    current_spans.extend(code_lang_label_spans(lang));
                    flush_line(&mut current_spans, &mut blocks);
                }
                code_syntax = syntax_ctx.and_then(|(ss, _)| {
                    if let CodeBlockKind::Fenced(lang) = kind {
                        let token = lang.split_whitespace().next().unwrap_or("");
                        if !token.is_empty() {
                            return ss.find_syntax_by_token(token);
                        }
                    }
                    None
                });
            }
            Event::End(TagEnd::CodeBlock) => {
                if in_suggestion {
                    blocks.push(ContentBlock::Suggestion {
                        lines: std::mem::take(&mut suggestion_lines),
                    });
                    in_suggestion = false;
                    in_code_block = false;
                    code_syntax = None;
                    continue;
                }
                if in_mermaid {
                    match renderer.mermaid_to_ascii(&mermaid_buf, mermaid_width) {
                        Ok(aa) => {
                            let start = blocks.len();
                            let border_style =
                                Style::default().fg(CODE_BLOCK_BORDER_FG).bg(CODE_BLOCK_BG);
                            let text_style = Style::default().fg(CODE_BLOCK_FG).bg(CODE_BLOCK_BG);
                            for aa_line in aa.lines() {
                                current_spans.push(Span::styled("▎ ", border_style));
                                current_spans.push(Span::styled(aa_line.to_string(), text_style));
                                flush_line(&mut current_spans, &mut blocks);
                            }
                            protected_blocks.push(start..blocks.len());
                        }
                        Err(error) => {
                            let style = Style::default().fg(Color::Rgb(114, 113, 105));
                            blocks
                                .push(ContentBlock::Text(mermaid_unavailable_line(&error, style)));
                        }
                    }
                    mermaid_buf.clear();
                }
                in_code_block = false;
                in_mermaid = false;
                code_syntax = None;
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                in_link = true;
                link_url_raw = dest_url.to_string();
                blob_link = parse_blob_url(&link_url_raw);
                media_link_kind = if blob_link.is_none() {
                    media_kind_for_url(&link_url_raw)
                } else {
                    None
                };
                media_link_alt.clear();
                if blob_link.is_some() && !current_spans.is_empty() {
                    flush_line(&mut current_spans, &mut blocks);
                }
                if media_link_kind.is_some() && !current_spans.is_empty() {
                    flush_line(&mut current_spans, &mut blocks);
                }
            }
            Event::End(TagEnd::Link) => {
                if let Some(info) = blob_link.take() {
                    blocks.push(ContentBlock::CodeSnippet {
                        url: std::mem::take(&mut link_url_raw),
                        path: info.path,
                        start_line: info.start_line,
                        end_line: info.end_line,
                    });
                } else if let Some(kind) = media_link_kind.take() {
                    let url = std::mem::take(&mut link_url_raw);
                    let alt = std::mem::take(&mut media_link_alt);
                    match kind {
                        MediaKind::Image => blocks.push(ContentBlock::Image { url, alt }),
                        MediaKind::Video => blocks.push(ContentBlock::Video { url, alt }),
                    }
                }
                in_link = false;
            }
            Event::Start(Tag::BlockQuote(kind)) => {
                add_block_separator_cb(&mut blocks, base);
                in_blockquote = true;
                alert_kind = kind;
                if let Some(ref ak) = alert_kind {
                    let fg = alert_color(ak);
                    let border_style = Style::default().fg(fg);
                    let label_style = Style::default().fg(fg).add_modifier(Modifier::BOLD);
                    current_spans.push(Span::styled("▎ ", border_style));
                    current_spans.push(Span::styled(
                        format!("{} {}", alert_icon(ak), alert_label(ak)),
                        label_style,
                    ));
                    flush_line(&mut current_spans, &mut blocks);
                }
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                in_blockquote = false;
                alert_kind = None;
            }
            Event::Start(Tag::List(start)) => {
                if list_depth == 0 {
                    add_block_separator_cb(&mut blocks, base);
                } else if !current_spans.is_empty() {
                    flush_line(&mut current_spans, &mut blocks);
                }
                list_depth += 1;
                ordered_list_stack.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                list_depth = list_depth.saturating_sub(1);
                ordered_list_stack.pop();
            }
            Event::Start(Tag::Item) => {
                if in_blockquote {
                    let bq_fg = alert_kind
                        .as_ref()
                        .map_or(BLOCKQUOTE_BORDER_FG, alert_color);
                    current_spans.push(Span::styled(
                        "▎ ",
                        style_with_bg(Style::default().fg(bq_fg), bg),
                    ));
                }
                let ordered = ordered_list_stack.last_mut().and_then(|c| c.as_mut());
                push_list_marker(&mut current_spans, list_depth, ordered, bg);
            }
            Event::End(TagEnd::Item) if !current_spans.is_empty() => {
                flush_line(&mut current_spans, &mut blocks);
            }
            Event::TaskListMarker(checked) => {
                push_task_marker(&mut current_spans, checked, bg);
            }
            Event::Start(Tag::Paragraph) => {
                if list_depth == 0 {
                    add_block_separator_cb(&mut blocks, base);
                }
                if in_blockquote && current_spans.is_empty() {
                    let bq_fg = alert_kind
                        .as_ref()
                        .map_or(BLOCKQUOTE_BORDER_FG, alert_color);
                    current_spans.push(Span::styled(
                        "▎ ",
                        style_with_bg(Style::default().fg(bq_fg), bg),
                    ));
                }
            }
            Event::End(TagEnd::Paragraph) => {
                flush_line(&mut current_spans, &mut blocks);
            }
            Event::Code(_) if blob_link.is_some() => {}
            Event::Code(code) if media_link_kind.is_some() => {
                media_link_alt.push_str(&code);
            }
            Event::Code(code) => {
                let style = Style::default().fg(INLINE_CODE_FG).bg(INLINE_CODE_BG);
                current_spans.push(Span::styled(format!(" {code} "), style));
            }
            Event::Text(_) if blob_link.is_some() => {}
            Event::Text(t) if !in_mermaid && !in_code_block && !in_link => {
                // Check for blob URLs in plain text (autolinks may not parse fragments)
                if let Some(info) = find_blob_url_in_text(t.as_ref()) {
                    if !current_spans.is_empty() {
                        flush_line(&mut current_spans, &mut blocks);
                    }
                    blocks.push(ContentBlock::CodeSnippet {
                        url: info.url.to_string(),
                        path: info.info.path,
                        start_line: info.info.start_line,
                        end_line: info.info.end_line,
                    });
                } else {
                    for (segment, media) in split_raw_media_segments(t.as_ref()) {
                        if !segment.is_empty() {
                            let resolved = replace_emoji_shortcodes(segment);
                            if in_blockquote {
                                let mut style = Style::default().fg(BLOCKQUOTE_FG);
                                if let Some(b) = bg {
                                    style = style.bg(b);
                                }
                                if bold {
                                    style = style.add_modifier(Modifier::BOLD);
                                }
                                if italic {
                                    style = style.add_modifier(Modifier::ITALIC);
                                }
                                current_spans.push(Span::styled(resolved.to_string(), style));
                            } else {
                                let mut style = if let Some(level) = heading_level {
                                    heading_style(level, bg)
                                } else {
                                    base
                                };
                                if bold && heading_level.is_none() {
                                    style = style.add_modifier(Modifier::BOLD);
                                }
                                if italic {
                                    style = style.add_modifier(Modifier::ITALIC);
                                }
                                if strikethrough {
                                    style = style.add_modifier(Modifier::CROSSED_OUT);
                                }
                                let mut link_style = Style::default()
                                    .fg(LINK_FG)
                                    .add_modifier(Modifier::UNDERLINED);
                                if let Some(b) = bg {
                                    link_style = link_style.bg(b);
                                }
                                push_spans_with_issue_refs(
                                    &resolved,
                                    style,
                                    link_style,
                                    &mut current_spans,
                                    options.repo_base_url,
                                );
                            }
                        }

                        if let Some((url, kind)) = media {
                            if !current_spans.is_empty() {
                                flush_line(&mut current_spans, &mut blocks);
                            }
                            match kind {
                                MediaKind::Image => blocks.push(ContentBlock::Image {
                                    url: url.to_string(),
                                    alt: String::new(),
                                }),
                                MediaKind::Video => blocks.push(ContentBlock::Video {
                                    url: url.to_string(),
                                    alt: String::new(),
                                }),
                            }
                        }
                    }
                }
            }
            Event::Text(t) => {
                if in_suggestion {
                    for line in t.as_ref().lines() {
                        suggestion_lines.push(line.to_string());
                    }
                } else if in_mermaid {
                    mermaid_buf.push_str(t.as_ref());
                } else if in_code_block {
                    for code_line in t.as_ref().lines() {
                        let border_style =
                            Style::default().fg(CODE_BLOCK_BORDER_FG).bg(CODE_BLOCK_BG);
                        current_spans.push(Span::styled("▎ ", border_style));
                        if let (Some(syn), Some((ss, theme))) = (code_syntax, syntax_ctx) {
                            current_spans.extend(highlight_content_inner(
                                code_line,
                                syn,
                                ss,
                                theme,
                                Some(CODE_BLOCK_BG),
                                false,
                            ));
                        } else {
                            let style = Style::default().fg(CODE_BLOCK_FG).bg(CODE_BLOCK_BG);
                            current_spans.push(Span::styled(code_line.to_string(), style));
                        }
                        flush_line(&mut current_spans, &mut blocks);
                    }
                } else if media_link_kind.is_some() {
                    media_link_alt.push_str(t.as_ref());
                } else if in_link {
                    let resolved = replace_emoji_shortcodes(t.as_ref());
                    let mut style = Style::default()
                        .fg(LINK_FG)
                        .add_modifier(Modifier::UNDERLINED);
                    if let Some(b) = bg {
                        style = style.bg(b);
                    }
                    current_spans.push(Span::styled(resolved.to_string(), style));
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                flush_line(&mut current_spans, &mut blocks);
                if in_blockquote {
                    let bq_fg = alert_kind
                        .as_ref()
                        .map_or(BLOCKQUOTE_BORDER_FG, alert_color);
                    current_spans.push(Span::styled(
                        "▎ ",
                        style_with_bg(Style::default().fg(bq_fg), bg),
                    ));
                }
            }
            Event::FootnoteReference(name) => {
                let style = style_with_bg(
                    Style::default()
                        .fg(LINK_FG)
                        .add_modifier(Modifier::UNDERLINED),
                    bg,
                );
                current_spans.push(Span::styled(format!("[{name}]"), style));
            }
            Event::Start(Tag::FootnoteDefinition(name)) => {
                add_block_separator_cb(&mut blocks, base);
                let style = style_with_bg(Style::default().fg(LINK_FG), bg);
                current_spans.push(Span::styled(format!("[{name}]: "), style));
            }
            Event::End(TagEnd::FootnoteDefinition) => {
                flush_line(&mut current_spans, &mut blocks);
            }
            Event::Rule => {
                flush_line(&mut current_spans, &mut blocks);
                blocks.push(ContentBlock::Text(hr_line(bg)));
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                if let Some((kind, url, alt)) = media_from_html_tag(html.as_ref()) {
                    if !current_spans.is_empty() {
                        flush_line(&mut current_spans, &mut blocks);
                    }
                    match kind {
                        MediaKind::Image => blocks.push(ContentBlock::Image { url, alt }),
                        MediaKind::Video => blocks.push(ContentBlock::Video { url, alt }),
                    }
                }
            }
            _ => {}
        }
    }
    if !current_spans.is_empty() {
        flush_line(&mut current_spans, &mut blocks);
    }
    if blocks.is_empty() {
        blocks.push(ContentBlock::Text(Line::from(Span::styled(
            String::new(),
            base,
        ))));
    }
    if wrap {
        blocks
            .into_iter()
            .enumerate()
            .flat_map(|(index, block)| match block {
                ContentBlock::Text(line)
                    if !protected_blocks.iter().any(|range| range.contains(&index))
                        && !is_rendered_table_line(&line) =>
                {
                    wrap_line(line, options.width)
                        .into_iter()
                        .map(ContentBlock::Text)
                        .collect()
                }
                block => vec![block],
            })
            .collect()
    } else {
        blocks
    }
}

pub fn extract_urls(markdown: &str, repo_base_url: Option<&str>) -> Vec<(String, String)> {
    let mut urls: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut in_link = false;
    let mut link_url = String::new();
    let mut link_label = String::new();
    let mut in_code_block = false;

    let parser = Parser::new_ext(markdown, Options::ENABLE_TABLES);
    for event in parser {
        match event {
            Event::Start(Tag::CodeBlock(_)) => in_code_block = true,
            Event::End(TagEnd::CodeBlock) => in_code_block = false,
            Event::Start(Tag::Link { dest_url, .. }) => {
                in_link = true;
                link_url = dest_url.to_string();
                link_label.clear();
            }
            Event::Text(t) if in_link => {
                link_label.push_str(t.as_ref());
            }
            Event::Code(t) if in_link => {
                link_label.push_str(t.as_ref());
            }
            Event::End(TagEnd::Link) => {
                in_link = false;
                if !link_url.is_empty() && seen.insert(link_url.clone()) {
                    let label = if link_label.is_empty() {
                        link_url.clone()
                    } else {
                        link_label.clone()
                    };
                    urls.push((label, link_url.clone()));
                }
                link_url.clear();
                link_label.clear();
            }
            Event::Text(t) if !in_link && !in_code_block => {
                // Extract #\d+ issue refs
                if let Some(base) = repo_base_url
                    && !base.is_empty()
                {
                    let text = t.as_ref();
                    let bytes = text.as_bytes();
                    let len = bytes.len();
                    let mut i = 0;
                    while i < len {
                        if bytes[i] == b'#' && i + 1 < len && bytes[i + 1].is_ascii_digit() {
                            if i > 0
                                && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_')
                            {
                                i += 1;
                                continue;
                            }
                            let start = i;
                            i += 1;
                            while i < len && bytes[i].is_ascii_digit() {
                                i += 1;
                            }
                            if i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                                continue;
                            }
                            let ref_text = &text[start..i];
                            let number = &text[start + 1..i];
                            let url = format!("{base}/issues/{number}");
                            if seen.insert(url.clone()) {
                                urls.push((ref_text.to_string(), url));
                            }
                        } else {
                            i += 1;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Extract raw https:// URLs from text
    for word in
        markdown.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == '<' || c == '>')
    {
        if word.starts_with("https://") {
            let url = word.trim_end_matches(['.', ',', ';', '!', '?', '"', '\'', ')']);
            if seen.insert(url.to_string()) {
                urls.push((url.to_string(), url.to_string()));
            }
        }
    }

    urls
}

pub fn extract_commit_hashes(markdown: &str) -> Vec<String> {
    let mut hashes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut in_code_block = false;

    let parser = Parser::new_ext(markdown, Options::ENABLE_TABLES);
    for event in parser {
        match event {
            Event::Start(Tag::CodeBlock(_)) => in_code_block = true,
            Event::End(TagEnd::CodeBlock) => in_code_block = false,
            Event::Text(text) | Event::Code(text) if !in_code_block => {
                let bytes = text.as_bytes();
                let mut start = 0;
                while start < bytes.len() {
                    if !bytes[start].is_ascii_hexdigit() {
                        start += 1;
                        continue;
                    }
                    let mut end = start + 1;
                    while end < bytes.len() && bytes[end].is_ascii_hexdigit() {
                        end += 1;
                    }
                    let joined_to_word = start > 0
                        && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_')
                        || end < bytes.len()
                            && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_');
                    if !joined_to_word && (7..=40).contains(&(end - start)) {
                        let hash = text[start..end].to_ascii_lowercase();
                        if seen.insert(hash.clone()) {
                            hashes.push(hash);
                        }
                    }
                    start = end;
                }
            }
            _ => {}
        }
    }

    hashes
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn markdown_to_lines(
        text: &str,
        base_fg: Color,
        bg: Option<Color>,
        syntax_ctx: Option<(&SyntaxSet, &Theme)>,
    ) -> Vec<Line<'static>> {
        Renderer::default().render_lines(
            text,
            &RenderOptions {
                base_fg,
                bg,
                syntax_ctx,
                ..RenderOptions::default()
            },
        )
    }

    fn markdown_to_content_blocks(
        text: &str,
        base_fg: Color,
        bg: Option<Color>,
        syntax_ctx: Option<(&SyntaxSet, &Theme)>,
    ) -> Vec<ContentBlock> {
        Renderer::default().render_blocks(
            text,
            &RenderOptions {
                base_fg,
                bg,
                syntax_ctx,
                ..RenderOptions::default()
            },
        )
    }

    #[test]
    fn render_options_do_not_leak_between_documents() {
        let mut renderer = Renderer::default();
        let linked = RenderOptions {
            base_fg: Color::Red,
            bg: Some(Color::Blue),
            repo_base_url: Some("https://github.com/owner/repo"),
            ..RenderOptions::default()
        };
        let plain = RenderOptions {
            base_fg: Color::Green,
            ..RenderOptions::default()
        };
        renderer.render_lines("See #42", &linked);
        let lines = renderer.render_lines("See #42", &plain);
        assert!(
            lines[0]
                .spans
                .iter()
                .all(|span| span.style.fg == Some(Color::Green) && span.style.bg.is_none())
        );
    }

    #[test]
    fn tables_keep_the_readable_width_limit_on_wide_terminals() {
        let mut renderer = Renderer::default();
        let options = RenderOptions {
            width: 220,
            ..RenderOptions::default()
        };
        let text = format!(
            "| Name | Description |\n|---|---|\n| {} | {} |",
            "x".repeat(300),
            "y".repeat(300)
        );
        let lines = renderer.render_lines(&text, &options);
        assert!(
            lines.iter().all(|line| line.width() <= MARKDOWN_WRAP_WIDTH),
            "{:?}",
            lines.iter().map(Line::width).collect::<Vec<_>>()
        );
        assert!(lines.iter().all(is_rendered_table_line));
    }

    #[test]
    fn table_respects_the_requested_width_in_both_outputs() {
        let mut renderer = Renderer::default();
        let options = RenderOptions {
            width: 32,
            ..RenderOptions::default()
        };
        let text = "| Column | Value |\n|---|---|\n| Long example text that needs wrapping | 日本語を含む文章の折り返し |";
        let lines = renderer.layout_lines(text, &options);
        assert!(lines.iter().all(|line| line.width() <= 32));
        assert!(lines.iter().all(is_rendered_table_line));
        let blocks = renderer.layout_blocks(text, &options);
        assert_eq!(blocks.len(), lines.len());
        assert!(blocks.iter().all(|block| matches!(block, ContentBlock::Text(line) if line.width() <= 32 && is_rendered_table_line(line))));
    }

    #[test]
    fn narrow_tables_preserve_wide_characters_and_values() {
        let mut renderer = Renderer::default();
        let text = "| 名前 | 内容 |\n|---|---|\n| 日本語 | 折り返し |";
        for width in [2, 8, 11, 12] {
            let options = RenderOptions {
                width,
                ..RenderOptions::default()
            };
            let lines = renderer.layout_lines(text, &options);
            assert!(
                lines.iter().all(|line| line.width() <= width),
                "width {width}: {:?}",
                line_texts(&lines)
            );
            let output = line_texts(&lines).join("");
            assert!(output.contains('日') && output.contains('語') && output.contains('折'));
        }
    }

    #[test]
    fn markdown_highlights_do_not_leak_to_the_next_render() {
        let mut renderer = Renderer::default();
        let range = 1..=1;
        let options = RenderOptions {
            highlighted_lines: std::slice::from_ref(&range),
            highlight_style: Style::default().bg(Color::Blue),
            ..RenderOptions::default()
        };
        let highlighted = renderer.render_lines("Selected\n\nUnselected", &options);
        assert!(
            highlighted[0]
                .spans
                .iter()
                .all(|span| span.style.bg == Some(Color::Blue))
        );
        assert!(
            highlighted
                .last()
                .unwrap()
                .spans
                .iter()
                .all(|span| span.style.bg.is_none())
        );
        let plain = renderer.render_lines("Selected\n\nUnselected", &RenderOptions::default());
        assert!(
            plain
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.style.bg.is_none())
        );
    }

    #[test]
    fn mermaid_cache_evicts_entries_at_capacity() {
        let mut renderer = Renderer::default();
        for index in 0..=MERMAID_CACHE_CAPACITY {
            let _ = renderer.mermaid_to_ascii(&format!("unsupported-{index}"), 80);
        }
        assert_eq!(renderer.mermaid_cache.len(), MERMAID_CACHE_CAPACITY);
        assert!(
            renderer
                .mermaid_cache
                .iter()
                .all(|((source, _), _)| source != "unsupported-0")
        );
    }

    #[test]
    fn mermaid_width_is_explicit_for_each_render() {
        let mut renderer = Renderer::default();
        let text = "```mermaid\nflowchart LR\n  A --> B\n```";
        let narrow = RenderOptions {
            width: 1,
            ..RenderOptions::default()
        };
        assert!(
            line_texts(&renderer.render_lines(text, &narrow))
                .join("\n")
                .contains("[mermaid unavailable:")
        );
        let wide = renderer.render_lines(text, &RenderOptions::default());
        assert!(
            !line_texts(&wide)
                .join("\n")
                .contains("[mermaid unavailable:")
        );
    }

    #[test]
    fn layout_keeps_mermaid_rows_and_counts_wrapped_table_rows() {
        let mut renderer = Renderer::default();
        let options = RenderOptions {
            width: 18,
            ..RenderOptions::default()
        };
        let text = "```mermaid\nflowchart LR\n A --> B\n```\n\n| Name | Description |\n|---|---|\n| item | an explanation that wraps over several rows |";
        let lines = renderer.layout_lines(text, &options);
        assert!(lines.iter().all(|line| line.width() <= options.width));
        assert!(
            line_texts(&lines)
                .iter()
                .any(|line| line.starts_with("▎ ") && line.contains('┌'))
        );
        assert_eq!(
            renderer.line_count(text, options.width, &options),
            lines.len()
        );
    }

    fn line_texts(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    /// A graph whose layout places an edge target left of its source used to
    /// underflow in `ma`'s LR routing and spin the draw thread forever.
    #[test]
    fn mermaid_graph_with_a_back_edge_renders() {
        let md = "```mermaid\ngraph LR\n  A --> B\n  A --> C\n  B --> D\n  C --> D\n  D --> A\n```";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines).join("\n");
        assert!(
            texts.contains('┌') && texts.contains('A'),
            "expected ascii art, got: {texts}"
        );
    }

    #[test]
    fn mermaid_width_error_is_visible_in_markdown() {
        let options = RenderOptions {
            width: 1,
            ..RenderOptions::default()
        };
        let md = "```mermaid\nflowchart LR\n  A --> B\n```";
        let lines = Renderer::default().render_lines(md, &options);

        let texts = line_texts(&lines).join("\n");
        assert!(
            texts.contains("[mermaid unavailable:") && texts.contains("too wide for 1 columns"),
            "expected the render error, got: {texts}"
        );
    }

    #[test]
    fn mermaid_width_error_is_visible_in_content_blocks() {
        let options = RenderOptions {
            width: 1,
            ..RenderOptions::default()
        };
        let md = "```mermaid\nflowchart LR\n  A --> B\n```";
        let blocks = Renderer::default().render_blocks(md, &options);

        let texts = blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(line) => Some(
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            texts.contains("[mermaid unavailable:") && texts.contains("too wide for 1 columns"),
            "expected the render error, got: {texts}"
        );
    }

    #[test]
    fn mermaid_narrow_lr_graph_reflows_instead_of_falling_back() {
        let options = RenderOptions {
            width: 5,
            ..RenderOptions::default()
        };
        let md = "```mermaid\nflowchart LR\n  A --> B\n```";
        let lines = Renderer::default().render_lines(md, &options);

        let texts = line_texts(&lines).join("\n");
        assert!(
            texts.contains('A') && texts.contains('B') && !texts.contains("[mermaid unavailable:"),
            "expected the graph to reflow, got: {texts}"
        );
    }

    #[test]
    fn mermaid_narrow_subgraphs_stack_vertically() {
        let options = RenderOptions {
            width: 19,
            ..RenderOptions::default()
        };
        let md = "```mermaid\ngraph LR\n  subgraph One\n    A --> B\n  end\n  subgraph Two\n    C --> D\n  end\n  B --> C\n```";
        let lines = Renderer::default().render_lines(md, &options);

        let texts = line_texts(&lines);
        let one_row = texts.iter().position(|line| line.contains("One"));
        let two_row = texts.iter().position(|line| line.contains("Two"));
        assert_eq!(
            one_row.zip(two_row).map(|(one, two)| two > one),
            Some(true),
            "expected vertically stacked subgraphs, got: {texts:?}"
        );
        assert!(
            texts.iter().all(|line| line.width() <= 21)
                && !texts
                    .iter()
                    .any(|line| line.contains("[mermaid unavailable:")),
            "expected the graph to fit the Markdown code-block width, got: {texts:?}"
        );
    }

    #[test]
    fn mermaid_invisible_edge_between_subgraphs_does_not_render_endpoint_nodes() {
        let md = "```mermaid\nflowchart TB\n  subgraph left[Group A]\n    A\n  end\n  subgraph right[Group B]\n    B\n  end\n  left ~~~ right\n```";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);

        assert!(
            texts.iter().any(|line| line.contains("Group A"))
                && texts.iter().any(|line| line.contains("Group B")),
            "expected both subgraphs, got: {texts:?}"
        );
        assert!(
            !texts.iter().any(|line| line.contains("│ left │"))
                && !texts.iter().any(|line| line.contains("│ right │")),
            "subgraph IDs must not be rendered as nodes: {texts:?}"
        );
    }

    #[test]
    fn issue_ref_expanded_to_link() {
        let options = RenderOptions {
            repo_base_url: Some("https://github.com/owner/repo"),
            ..RenderOptions::default()
        };
        let md = "See #42 for details";
        let lines = Renderer::default().render_lines(md, &options);
        let texts = line_texts(&lines);
        assert!(
            texts[0].contains("#42"),
            "should contain #42 text: {texts:?}"
        );
        // Check that #42 is rendered with link style (blue + underline)
        let has_link_span = lines[0]
            .spans
            .iter()
            .any(|s| s.content.contains("#42") && s.style.fg == Some(LINK_FG));
        assert!(
            has_link_span,
            "should have link-styled #42 span: {:?}",
            lines[0].spans
        );
    }

    #[test]
    fn issue_ref_not_expanded_in_code() {
        let options = RenderOptions {
            repo_base_url: Some("https://github.com/owner/repo"),
            ..RenderOptions::default()
        };
        let md = "Use `#42` in code";
        let lines = Renderer::default().render_lines(md, &options);
        // #42 inside inline code should NOT become a link
        let has_link_span = lines[0]
            .spans
            .iter()
            .any(|s| s.content.contains("#42") && s.style.fg == Some(LINK_FG));
        assert!(
            !has_link_span,
            "#42 in inline code should not be a link: {:?}",
            lines[0].spans
        );
    }

    #[test]
    fn issue_ref_not_expanded_without_repo_url() {
        let options = RenderOptions {
            repo_base_url: Some(""),
            ..RenderOptions::default()
        };
        let md = "See #42 for details";
        let lines = Renderer::default().render_lines(md, &options);
        let has_link_span = lines[0]
            .spans
            .iter()
            .any(|s| s.content.contains("#42") && s.style.fg == Some(LINK_FG));
        assert!(
            !has_link_span,
            "#42 should not be a link without repo URL: {:?}",
            lines[0].spans
        );
    }

    #[test]
    fn issue_ref_inside_existing_link_not_doubled() {
        let options = RenderOptions {
            repo_base_url: Some("https://github.com/owner/repo"),
            ..RenderOptions::default()
        };
        let md = "See [#42](https://github.com/owner/repo/pull/42)";
        let lines = Renderer::default().render_lines(md, &options);
        let texts = line_texts(&lines);
        // Should not produce double-linked text like [#42](...)
        assert_eq!(
            texts[0].matches("#42").count(),
            1,
            "should have exactly one #42: {texts:?}"
        );
    }

    #[test]
    fn paragraphs_separated_by_blank_line() {
        let md = "First paragraph.\n\nSecond paragraph.";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        assert_eq!(
            texts.len(),
            3,
            "should have 3 lines (para, blank, para): {texts:?}"
        );
        assert_eq!(texts[0], "First paragraph.");
        assert!(
            texts[1].is_empty(),
            "should have blank separator: {texts:?}"
        );
        assert_eq!(texts[2], "Second paragraph.");
    }

    #[test]
    fn heading_then_paragraph_separated() {
        let md = "## Title\n\nBody text.";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        assert_eq!(texts.len(), 3, "heading + blank + body: {texts:?}");
        assert!(texts[0].contains("Title"));
        assert!(
            texts[1].is_empty(),
            "should have blank separator: {texts:?}"
        );
        assert_eq!(texts[2], "Body text.");
    }

    #[test]
    fn list_items_not_double_spaced() {
        let md = "- Item 1\n- Item 2\n- Item 3";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        assert_eq!(texts.len(), 3, "3 items, no blank lines between: {texts:?}");
    }

    #[test]
    fn content_blocks_paragraphs_separated() {
        let md = "First.\n\nSecond.";
        let blocks = markdown_to_content_blocks(md, Color::Rgb(220, 215, 186), None, None);
        let texts = block_texts(&blocks);
        assert_eq!(texts.len(), 3, "para + blank + para: {texts:?}");
        assert!(
            texts[1].is_empty(),
            "should have blank separator: {texts:?}"
        );
    }

    #[test]
    fn table_parsed_and_rendered() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |\n";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        // Top border
        assert!(texts[0].contains("┌"), "top border: {texts:?}");
        // Header row should contain cell text with │ separators
        assert!(texts[1].contains("A"), "header should contain A: {texts:?}");
        assert!(texts[1].contains("│"), "header should contain │: {texts:?}");
        // Separator line
        assert!(texts[2].contains("──"), "separator: {texts:?}");
        // Data row
        assert!(texts[3].contains("1"), "data row: {texts:?}");
        // Bottom border
        assert!(texts[4].contains("└"), "bottom border: {texts:?}");
    }

    #[test]
    fn pulldown_cmark_generates_table_events() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |\n";
        let parser = Parser::new_ext(md, md_options());
        let events: Vec<Event> = parser.collect();
        let has_table = events
            .iter()
            .any(|e| matches!(e, Event::Start(Tag::Table(_))));
        assert!(has_table, "should have Table event: {events:?}");
    }

    #[test]
    fn table_without_alignment_row_not_parsed() {
        // Without |---|---| row, pulldown-cmark does NOT recognize it as a table
        let md = "| A | B |\n| 1 | 2 |\n";
        let parser = Parser::new_ext(md, md_options());
        let events: Vec<Event> = parser.collect();
        let has_table = events
            .iter()
            .any(|e| matches!(e, Event::Start(Tag::Table(_))));
        assert!(
            !has_table,
            "should NOT be a table without alignment row: {events:?}"
        );
    }

    #[test]
    fn table_with_heading_before() {
        let md = "## 変更ファイル\n\n| ファイル | 変更内容 |\n|---|---|\n| `foo.rs` | bar |\n";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        assert!(texts[0].contains("変更ファイル"), "heading: {texts:?}");
        assert!(
            texts[1].is_empty(),
            "blank separator after heading: {texts:?}"
        );
        assert!(texts[2].contains("┌"), "top border: {texts:?}");
        assert!(
            texts[3].contains("│"),
            "table header should have │: {texts:?}"
        );
        assert!(texts[4].contains("──"), "separator: {texts:?}");
        assert!(texts[5].contains("foo.rs"), "data row: {texts:?}");
        assert!(texts[6].contains("└"), "bottom border: {texts:?}");
    }

    #[test]
    fn table_with_escaped_pipes() {
        let md = "| ファイル | 変更内容 |\n|---------|--------|\n| `foo.ts` | `val \\|\\| null` |\n| `bar.ts` | 同上 |\n";
        let parser = Parser::new_ext(md, md_options());
        let events: Vec<Event> = parser.collect();
        let has_table = events
            .iter()
            .any(|e| matches!(e, Event::Start(Tag::Table(_))));
        assert!(
            has_table,
            "should parse as table even with escaped pipes: {events:?}"
        );

        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        assert!(texts[0].contains("┌"), "top border: {texts:?}");
        assert!(texts[1].contains("│"), "header should have │: {texts:?}");
        assert!(
            texts[3].contains("foo.ts"),
            "data row with escaped pipes: {texts:?}"
        );
    }

    fn block_texts(blocks: &[ContentBlock]) -> Vec<String> {
        blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(line) => Some(
                    line.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn table_columns_aligned() {
        let md = "| A | BB |\n|---|---|\n| CCC | D |\n";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        // Header and data should have same total width (columns aligned)
        assert_eq!(
            texts[1].chars().count(),
            texts[3].chars().count(),
            "columns should be aligned: header={:?} data={:?}",
            texts[1],
            texts[3],
        );
        // Top border
        assert!(texts[0].contains('┌'), "top border: {}", texts[0]);
        // Separator should contain ├ and ┤
        assert!(texts[2].contains('├'), "separator start: {}", texts[2]);
        assert!(texts[2].contains('┤'), "separator end: {}", texts[2]);
        // Bottom border
        assert!(texts[4].contains('└'), "bottom border: {}", texts[4]);
    }

    #[test]
    fn table_wraps_long_cell_contents_without_splitting_the_table() {
        let long = "description ".repeat(20);
        let md = format!(
            "| Rule | Description | Level |\n|---|---|---|\n| rule-name | {long} | warning |\n"
        );

        let lines = markdown_to_lines(&md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        let data_lines: Vec<&String> = texts
            .iter()
            .filter(|line| line.starts_with('│') && !line.contains("Rule"))
            .collect();

        assert!(lines.iter().all(|line| line.width() <= MARKDOWN_WRAP_WIDTH));
        assert!(data_lines.len() > 1, "long cell should wrap: {texts:?}");
        let separators: Vec<Vec<usize>> = data_lines
            .iter()
            .map(|line| line.match_indices('│').map(|(index, _)| index).collect())
            .collect();
        assert!(
            separators.windows(2).all(|pair| pair[0] == pair[1]),
            "wrapped rows must keep column boundaries: {texts:?}"
        );
    }

    #[test]
    fn extract_urls_from_markdown_links() {
        let md =
            "Check [this PR](https://github.com/foo/bar/pull/1) and [docs](https://docs.rs/foo).";
        let urls = extract_urls(md, None);
        assert_eq!(
            urls,
            vec![
                (
                    "this PR".to_string(),
                    "https://github.com/foo/bar/pull/1".to_string()
                ),
                ("docs".to_string(), "https://docs.rs/foo".to_string()),
            ]
        );
    }

    #[test]
    fn extract_commit_hashes_from_text_and_inline_code() {
        let md = "Fixed by abcdef1 and `0123456789abcdef0123456789abcdef01234567`.";

        assert_eq!(
            extract_commit_hashes(md),
            vec![
                "abcdef1".to_string(),
                "0123456789abcdef0123456789abcdef01234567".to_string(),
            ]
        );
    }

    #[test]
    fn extract_commit_hashes_ignores_code_blocks_invalid_lengths_and_duplicates() {
        let md = "abcdef abcdef1 abcdef1 0123456789abcdef0123456789abcdef012345678 abcdef1xyz xabcdef1 foo_abcdef1\n\n```\ndeadbee\n```";

        assert_eq!(extract_commit_hashes(md), vec!["abcdef1".to_string()]);
    }

    #[test]
    fn extract_urls_raw_https() {
        let md = "See https://example.com/page and also https://other.com/path?q=1 for details.";
        let urls = extract_urls(md, None);
        assert_eq!(
            urls,
            vec![
                (
                    "https://example.com/page".to_string(),
                    "https://example.com/page".to_string()
                ),
                (
                    "https://other.com/path?q=1".to_string(),
                    "https://other.com/path?q=1".to_string()
                ),
            ]
        );
    }

    #[test]
    fn extract_urls_deduplicate() {
        let md = "Link: [foo](https://example.com) and https://example.com again.";
        let urls = extract_urls(md, None);
        assert_eq!(
            urls,
            vec![("foo".to_string(), "https://example.com".to_string())]
        );
    }

    #[test]
    fn extract_urls_empty() {
        let md = "No links here, just plain text.";
        let urls = extract_urls(md, None);
        assert!(urls.is_empty());
    }

    #[test]
    fn extract_urls_mixed() {
        let md = "See [link](https://a.com) and https://b.com and [other](http://c.com).";
        let urls = extract_urls(md, None);
        assert!(urls.iter().any(|(_, u)| u == "https://a.com"));
        assert!(urls.iter().any(|(_, u)| u == "https://b.com"));
        assert!(urls.iter().any(|(_, u)| u == "http://c.com"));
    }

    #[test]
    fn extract_urls_issue_ref_with_repo() {
        let md = "See #42 and #100 for details";
        let urls = extract_urls(md, Some("https://github.com/owner/repo"));
        assert!(
            urls.iter()
                .any(|(l, u)| l == "#42" && u == "https://github.com/owner/repo/issues/42")
        );
        assert!(
            urls.iter()
                .any(|(l, u)| l == "#100" && u == "https://github.com/owner/repo/issues/100")
        );
    }

    #[test]
    fn extract_urls_issue_ref_without_repo() {
        let md = "See #42 for details";
        let urls = extract_urls(md, None);
        assert!(
            urls.is_empty(),
            "should not extract issue refs without repo URL"
        );
    }

    #[test]
    fn extract_urls_issue_ref_in_code_block() {
        let md = "```\n#42\n```";
        let urls = extract_urls(md, Some("https://github.com/owner/repo"));
        assert!(
            urls.is_empty(),
            "should not extract issue refs from code blocks"
        );
    }

    #[test]
    fn emoji_shortcodes_replaced_in_text() {
        let md = "Hello :cry: world :+1:";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        assert!(
            texts[0].contains("😢"),
            "should contain cry emoji: {texts:?}"
        );
        assert!(
            texts[0].contains("👍"),
            "should contain +1 emoji: {texts:?}"
        );
        assert!(
            !texts[0].contains(":cry:"),
            "should not contain shortcode: {texts:?}"
        );
    }

    #[test]
    fn emoji_shortcodes_not_replaced_in_code_block() {
        let md = "```\n:cry:\n```";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        let all = texts.join("");
        assert!(
            all.contains(":cry:"),
            "code block should keep shortcode: {texts:?}"
        );
        assert!(
            !all.contains("😢"),
            "code block should not have emoji: {texts:?}"
        );
    }

    #[test]
    fn emoji_shortcodes_not_replaced_in_inline_code() {
        let md = "Use `:cry:` for sad face";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        let all = texts.join("");
        assert!(
            all.contains(":cry:"),
            "inline code should keep shortcode: {texts:?}"
        );
    }

    #[test]
    fn unknown_shortcode_left_as_is() {
        let md = "Hello :notarealemoji: world";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        assert!(
            texts[0].contains(":notarealemoji:"),
            "unknown shortcode should be kept: {texts:?}"
        );
    }

    #[test]
    fn content_blocks_table_with_heading_and_escaped_pipes() {
        let md = "### Changes\n\n\
            | File | Description |\n\
            |---------|--------|\n\
            | `app.tsx` | Add `userId?: string` field |\n\
            | `handler.ts` | `defaultVal: null` → `form.value \\|\\| null` |\n\
            | `util.ts` | Same as above |\n";
        let blocks = markdown_to_content_blocks(md, Color::Rgb(220, 215, 186), None, None);
        let texts = block_texts(&blocks);
        assert!(texts[0].contains("Changes"), "heading: {texts:?}");
        assert!(
            texts[1].is_empty(),
            "blank separator after heading: {texts:?}"
        );
        assert!(texts[2].contains("┌"), "top border: {texts:?}");
        assert!(texts[3].contains("File"), "header: {texts:?}");
        assert!(texts[4].contains("├"), "separator: {texts:?}");
        assert!(texts[5].contains("app.tsx"), "first data row: {texts:?}");
        assert!(texts[6].contains("├"), "row separator: {texts:?}");
        assert!(texts[7].contains("||"), "escaped pipes rendered: {texts:?}");
    }

    #[test]
    fn alert_note_rendered() {
        let md = "> [!NOTE]\n> This is a note.";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        let has_label = texts.iter().any(|t| t.contains("Note"));
        assert!(has_label, "should have Note label: {texts:?}");
        let has_content = texts.iter().any(|t| t.contains("This is a note."));
        assert!(has_content, "should have note content: {texts:?}");
    }

    #[test]
    fn alert_warning_rendered() {
        let md = "> [!WARNING]\n> Be careful.";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        let has_label = texts.iter().any(|t| t.contains("Warning"));
        assert!(has_label, "should have Warning label: {texts:?}");
    }

    #[test]
    fn alert_has_colored_border() {
        let md = "> [!NOTE]\n> Content here.";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let has_colored_border = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content.contains("▎") && s.style.fg == Some(ALERT_NOTE_FG))
        });
        assert!(
            has_colored_border,
            "should have blue-colored border for NOTE: {:?}",
            lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .map(|s| (&s.content, &s.style.fg))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn footnote_reference_rendered() {
        let md = "Text with a footnote[^1].\n\n[^1]: The footnote content.";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let has_ref = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content.contains("[1]") && s.style.fg == Some(LINK_FG))
        });
        assert!(
            has_ref,
            "should have [1] with link style: {:?}",
            line_texts(&lines)
        );
    }

    #[test]
    fn footnote_definition_rendered() {
        let md = "Text[^note].\n\n[^note]: Definition here.";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        let has_def = texts
            .iter()
            .any(|t| t.contains("[note]") && t.contains("Definition here."));
        assert!(has_def, "should have footnote definition: {texts:?}");
    }

    #[test]
    fn task_list_unchecked() {
        let md = "- [ ] todo item";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        assert!(
            texts[0].contains("☐"),
            "unchecked task should show ☐: {texts:?}"
        );
    }

    #[test]
    fn task_list_checked() {
        let md = "- [x] done item";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        assert!(
            texts[0].contains("☑"),
            "checked task should show ☑: {texts:?}"
        );
    }

    fn all_block_texts(blocks: &[ContentBlock]) -> Vec<String> {
        blocks
            .iter()
            .map(|b| match b {
                ContentBlock::Text(line) => line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>(),
                ContentBlock::Image { url, .. } => format!("[image:{url}]"),
                ContentBlock::Video { url, .. } => format!("[video:{url}]"),
                ContentBlock::CodeSnippet {
                    path,
                    start_line,
                    end_line,
                    ..
                } => {
                    if let Some(end) = end_line {
                        format!("[snippet:{path}#L{start_line}-L{end}]")
                    } else {
                        format!("[snippet:{path}#L{start_line}]")
                    }
                }
                ContentBlock::Suggestion { lines } => format!("[suggestion:{}]", lines.join("|")),
            })
            .collect()
    }

    #[test]
    fn blob_url_emits_code_snippet_block() {
        let md = "Check this:\n\nhttps://github.com/owner/repo/blob/abc123/src/main.go#L42\n\nGood right?";
        let blocks = markdown_to_content_blocks(md, Color::Rgb(220, 215, 186), None, None);
        let texts = all_block_texts(&blocks);
        let has_snippet = texts
            .iter()
            .any(|t| t.contains("[snippet:src/main.go#L42]"));
        assert!(has_snippet, "should produce CodeSnippet block: {texts:?}");
    }

    #[test]
    fn blob_url_with_range_emits_code_snippet_block() {
        let md = "https://github.com/owner/repo/blob/main/lib.rs#L10-L20";
        let blocks = markdown_to_content_blocks(md, Color::Rgb(220, 215, 186), None, None);
        let texts = all_block_texts(&blocks);
        let has_snippet = texts.iter().any(|t| t.contains("[snippet:lib.rs#L10-L20]"));
        assert!(
            has_snippet,
            "should produce CodeSnippet block with range: {texts:?}"
        );
    }

    #[test]
    fn non_blob_url_stays_as_text() {
        let md = "See https://example.com/foo for details";
        let blocks = markdown_to_content_blocks(md, Color::Rgb(220, 215, 186), None, None);
        let has_snippet = blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::CodeSnippet { .. }));
        assert!(!has_snippet, "non-blob URL should not produce CodeSnippet");
    }

    #[test]
    fn markdown_image_emits_image_block() {
        let md = "![demo](https://example.com/image.png)";
        let blocks = markdown_to_content_blocks(md, Color::Rgb(220, 215, 186), None, None);

        assert!(blocks.iter().any(|b| matches!(
            b,
            ContentBlock::Image { url, alt }
                if url == "https://example.com/image.png" && alt == "demo"
        )));
    }

    #[test]
    fn markdown_video_link_emits_video_block() {
        let md = "[demo clip](https://example.com/demo.mp4)";
        let blocks = markdown_to_content_blocks(md, Color::Rgb(220, 215, 186), None, None);

        assert!(blocks.iter().any(|b| matches!(
            b,
            ContentBlock::Video { url, alt }
                if url == "https://example.com/demo.mp4" && alt == "demo clip"
        )));
    }

    #[test]
    fn raw_media_urls_emit_media_blocks() {
        let md = "Image https://example.com/a.webp and video https://example.com/b.webm";
        let blocks = markdown_to_content_blocks(md, Color::Rgb(220, 215, 186), None, None);

        assert!(blocks.iter().any(|b| matches!(
            b,
            ContentBlock::Image { url, .. } if url == "https://example.com/a.webp"
        )));
        assert!(blocks.iter().any(|b| matches!(
            b,
            ContentBlock::Video { url, .. } if url == "https://example.com/b.webm"
        )));
    }

    #[test]
    fn regular_link_does_not_emit_media_block() {
        let md = "[docs](https://example.com/page)";
        let blocks = markdown_to_content_blocks(md, Color::Rgb(220, 215, 186), None, None);

        assert!(
            !blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. } | ContentBlock::Video { .. }))
        );
    }

    #[test]
    fn github_user_attachment_markdown_image_emits_image_block() {
        let md = "![shot](https://github.com/user-attachments/assets/abc-123-def)";
        let blocks = markdown_to_content_blocks(md, Color::Rgb(220, 215, 186), None, None);

        assert!(blocks.iter().any(|b| matches!(
            b,
            ContentBlock::Image { url, alt }
                if url == "https://github.com/user-attachments/assets/abc-123-def"
                    && alt == "shot"
        )));
    }

    #[test]
    fn github_user_attachment_bare_url_emits_image_block() {
        let md = "See https://github.com/user-attachments/assets/abc-123-def please";
        let blocks = markdown_to_content_blocks(md, Color::Rgb(220, 215, 186), None, None);

        assert!(blocks.iter().any(|b| matches!(
            b,
            ContentBlock::Image { url, .. }
                if url == "https://github.com/user-attachments/assets/abc-123-def"
        )));
    }

    #[test]
    fn html_img_tag_emits_image_block() {
        let md = r#"<img src="https://github.com/user-attachments/assets/img-1" alt="ui">"#;
        let blocks = markdown_to_content_blocks(md, Color::Rgb(220, 215, 186), None, None);

        assert!(blocks.iter().any(|b| matches!(
            b,
            ContentBlock::Image { url, alt }
                if url == "https://github.com/user-attachments/assets/img-1" && alt == "ui"
        )));
    }

    #[test]
    fn html_video_tag_emits_video_block() {
        let md = r#"<video src="https://github.com/user-attachments/assets/vid-1"></video>"#;
        let blocks = markdown_to_content_blocks(md, Color::Rgb(220, 215, 186), None, None);

        assert!(blocks.iter().any(|b| matches!(
            b,
            ContentBlock::Video { url, .. }
                if url == "https://github.com/user-attachments/assets/vid-1"
        )));
    }

    #[test]
    fn parse_blob_url_single_line() {
        let url = "https://github.com/octocat/hello-world/blob/a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2/src/app/handler.go#L584";
        let parsed = super::parse_blob_url(url).unwrap();
        assert_eq!(parsed.owner, "octocat");
        assert_eq!(parsed.repo, "hello-world");
        assert_eq!(parsed.git_ref, "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2");
        assert_eq!(parsed.path, "src/app/handler.go");
        assert_eq!(parsed.start_line, 584);
        assert_eq!(parsed.end_line, None);
    }

    #[test]
    fn parse_blob_url_line_range() {
        let url = "https://github.com/owner/repo/blob/main/src/lib.rs#L10-L20";
        let parsed = super::parse_blob_url(url).unwrap();
        assert_eq!(parsed.owner, "owner");
        assert_eq!(parsed.repo, "repo");
        assert_eq!(parsed.git_ref, "main");
        assert_eq!(parsed.path, "src/lib.rs");
        assert_eq!(parsed.start_line, 10);
        assert_eq!(parsed.end_line, Some(20));
    }

    #[test]
    fn parse_blob_url_no_fragment() {
        let url = "https://github.com/owner/repo/blob/main/src/lib.rs";
        assert!(super::parse_blob_url(url).is_none());
    }

    #[test]
    fn parse_blob_url_not_github() {
        let url = "https://gitlab.com/owner/repo/blob/main/src/lib.rs#L10";
        assert!(super::parse_blob_url(url).is_none());
    }

    #[test]
    fn parse_blob_url_nested_path() {
        let url = "https://github.com/org/repo/blob/abc123/a/b/c/d.go#L1-L100";
        let parsed = super::parse_blob_url(url).unwrap();
        assert_eq!(parsed.path, "a/b/c/d.go");
        assert_eq!(parsed.start_line, 1);
        assert_eq!(parsed.end_line, Some(100));
    }

    #[test]
    fn strikethrough_has_crossed_out_modifier() {
        let md = "This is ~~deleted~~ text";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let has_crossed = lines[0].spans.iter().any(|s| {
            s.content.contains("deleted") && s.style.add_modifier.contains(Modifier::CROSSED_OUT)
        });
        assert!(
            has_crossed,
            "~~deleted~~ should have CROSSED_OUT modifier: {:?}",
            lines[0].spans
        );
    }

    #[test]
    fn suggestion_block_detected() {
        let md = "```suggestion\nlet x = 42;\n```";
        let blocks = markdown_to_content_blocks(md, Color::Rgb(220, 215, 186), None, None);
        let has_suggestion = blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::Suggestion { .. }));
        assert!(
            has_suggestion,
            "should produce Suggestion block: {:?}",
            all_block_texts(&blocks)
        );
    }

    #[test]
    fn suggestion_block_lines() {
        let md = "```suggestion\nlet x = 42;\nlet y = 100;\n```";
        let blocks = markdown_to_content_blocks(md, Color::Rgb(220, 215, 186), None, None);
        let suggestion = blocks.iter().find_map(|b| match b {
            ContentBlock::Suggestion { lines } => Some(lines.clone()),
            _ => None,
        });
        assert_eq!(
            suggestion,
            Some(vec!["let x = 42;".to_string(), "let y = 100;".to_string()])
        );
    }

    #[test]
    fn suggestion_block_empty() {
        let md = "```suggestion\n```";
        let blocks = markdown_to_content_blocks(md, Color::Rgb(220, 215, 186), None, None);
        let suggestion = blocks.iter().find_map(|b| match b {
            ContentBlock::Suggestion { lines } => Some(lines.clone()),
            _ => None,
        });
        assert_eq!(
            suggestion,
            Some(vec![]),
            "empty suggestion should have empty lines: {:?}",
            all_block_texts(&blocks)
        );
    }

    #[test]
    fn suggestion_block_case_insensitive() {
        let md = "```Suggestion\nfoo\n```";
        let blocks = markdown_to_content_blocks(md, Color::Rgb(220, 215, 186), None, None);
        let has_suggestion = blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::Suggestion { .. }));
        assert!(
            has_suggestion,
            "Suggestion (uppercase) should be detected: {:?}",
            all_block_texts(&blocks)
        );
    }

    #[test]
    fn suggestion_not_detected_in_markdown_to_lines() {
        // markdown_to_lines should NOT produce Suggestion blocks — it returns Vec<Line>
        // so suggestion code blocks should just be rendered as normal code blocks.
        let md = "```suggestion\nlet x = 42;\n```";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        // Should contain the code text as normal code block rendering
        let has_code = texts.iter().any(|t| t.contains("let x = 42;"));
        assert!(
            has_code,
            "markdown_to_lines should render suggestion as normal code: {texts:?}"
        );
    }

    #[test]
    fn link_uses_github_accent_blue() {
        let md = "[docs](https://example.com)";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let has_accent = lines[0]
            .spans
            .iter()
            .any(|s| s.content.contains("docs") && s.style.fg == Some(LINK_FG));
        assert!(
            has_accent,
            "links should use accent blue LINK_FG: {:?}",
            lines[0].spans
        );
        assert_eq!(LINK_FG, Color::Rgb(88, 166, 255));
    }

    #[test]
    fn list_uses_bullet_marker() {
        let md = "- Item 1\n- Item 2";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        assert!(
            texts[0].contains('•'),
            "unordered list should use •: {texts:?}"
        );
        let marker_styled = lines[0]
            .spans
            .iter()
            .any(|s| s.content.contains('•') && s.style.fg == Some(LIST_MARKER_FG));
        assert!(
            marker_styled,
            "bullet should use LIST_MARKER_FG: {:?}",
            lines[0].spans
        );
    }

    #[test]
    fn nested_list_indents_deeper() {
        let md = "- outer\n  - inner";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        assert!(texts.len() >= 2, "expected two items: {texts:?}");
        let outer_indent = texts[0].find('•').unwrap_or(0);
        let inner_indent = texts[1]
            .find('◦')
            .or_else(|| texts[1].find('•'))
            .unwrap_or(0);
        assert!(
            inner_indent > outer_indent,
            "nested item should indent more: outer={outer_indent} inner={inner_indent} texts={texts:?}"
        );
    }

    #[test]
    fn task_checked_uses_success_color() {
        let md = "- [x] done";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let checked = lines[0]
            .spans
            .iter()
            .any(|s| s.content.contains('☑') && s.style.fg == Some(TASK_CHECKED_FG));
        assert!(
            checked,
            "checked task should be green: {:?}",
            lines[0].spans
        );
    }

    #[test]
    fn code_block_shows_language_label() {
        let md = "```rust\nfn main() {}\n```";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        assert!(
            texts.iter().any(|t| t.contains("rust")),
            "fenced code should show language label: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("fn main()")),
            "code body should still render: {texts:?}"
        );
    }

    #[test]
    fn horizontal_rule_is_long_separator() {
        let md = "above\n\n---\n\nbelow";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let texts = line_texts(&lines);
        let hr = texts.iter().find(|t| t.contains('─')).expect("hr line");
        assert!(
            hr.chars().filter(|c| *c == '─').count() >= 16,
            "hr should be a long separator: {hr:?}"
        );
    }

    #[test]
    fn heading_h1_underlined_bold() {
        let md = "# Title";
        let lines = markdown_to_lines(md, Color::Rgb(220, 215, 186), None, None);
        let title = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("Title"))
            .expect("title span");
        assert!(
            title.style.add_modifier.contains(Modifier::BOLD),
            "H1 should be bold"
        );
        assert!(
            title.style.add_modifier.contains(Modifier::UNDERLINED),
            "H1 should be underlined"
        );
    }
}
