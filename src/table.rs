use crate::render::{MediaKind, html_attr_value, media_from_html_tag};
use crate::{ContentBlock, wrap_line};
use pulldown_cmark::{Event, Parser};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

#[derive(Clone, Debug, PartialEq)]
pub struct TableCell {
    pub colspan: usize,
    pub content: Vec<ContentBlock>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Table {
    pub rows: Vec<Vec<TableCell>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImagePlacement {
    pub row: usize,
    pub column: usize,
    pub width: usize,
    pub height: usize,
    pub url: String,
}

#[derive(Default)]
pub struct TableLayout {
    pub lines: Vec<Line<'static>>,
    pub images: Vec<ImagePlacement>,
}

impl Table {
    pub fn layout(
        &self,
        width: usize,
        mut image_height: impl FnMut(&str, usize) -> Option<usize>,
    ) -> TableLayout {
        let width = width.max(1);
        let columns = self
            .rows
            .iter()
            .map(|row| row.iter().map(|c| c.colspan.clamp(1, 64)).sum::<usize>())
            .max()
            .unwrap_or(1)
            .max(1);
        let mut result = TableLayout::default();
        if width < columns.saturating_mul(2).saturating_add(1) {
            for row in &self.rows {
                for cell in row {
                    let (lines, images) = cell_layout(cell, width, &mut image_height);
                    let offset = result.lines.len();
                    result.images.extend(images.into_iter().map(|mut image| {
                        image.row += offset;
                        image
                    }));
                    result.lines.extend(lines);
                }
            }
            return result;
        }
        let space = width - columns - 1;
        let widths: Vec<_> = (0..columns)
            .map(|col| space / columns + usize::from(col < space % columns))
            .collect();
        let mut previous = vec![false; width];
        for row in &self.rows {
            let mut boundaries = vec![false; width];
            boundaries[0] = true;
            boundaries[width - 1] = true;
            let mut column = 0;
            let mut x = 1;
            let mut cells = Vec::new();
            let mut height = 1;
            for cell in row {
                let span = cell.colspan.clamp(1, 64).min(columns - column);
                let cell_width = widths[column..column + span].iter().sum::<usize>() + span - 1;
                let (lines, images) = cell_layout(cell, cell_width, &mut image_height);
                height = height.max(lines.len());
                cells.push((x, cell_width, lines, images));
                x += cell_width + 1;
                boundaries[x - 1] = true;
                column += span;
            }
            result.lines.push(border(&previous, &boundaries));
            let y = result.lines.len();
            for line_index in 0..height {
                let mut spans = vec![Span::raw("│")];
                let mut used = 1;
                for (_, cell_width, lines, _) in &cells {
                    let line = lines.get(line_index).cloned().unwrap_or_default();
                    let padding = cell_width.saturating_sub(line.width());
                    spans.extend(line.spans);
                    spans.push(Span::raw(format!("{}│", " ".repeat(padding))));
                    used += cell_width + 1;
                }
                if used < width {
                    spans.push(Span::raw(format!("{}│", " ".repeat(width - used - 1))));
                }
                result.lines.push(Line::from(spans));
            }
            for (x, _, _, images) in cells {
                result.images.extend(images.into_iter().map(|mut image| {
                    image.row += y;
                    image.column += x;
                    image
                }));
            }
            previous = boundaries;
        }
        if !self.rows.is_empty() {
            result.lines.push(border(&previous, &vec![false; width]));
        }
        result
    }
}

fn border(above: &[bool], below: &[bool]) -> Line<'static> {
    let last = above.len() - 1;
    let text: String = (0..above.len())
        .map(|x| match (above[x], below[x], x) {
            (false, true, 0) => '┌',
            (true, false, 0) => '└',
            (true, true, 0) => '├',
            (false, true, x) if x == last => '┐',
            (true, false, x) if x == last => '┘',
            (true, true, x) if x == last => '┤',
            (true, true, _) => '┼',
            (true, false, _) => '┴',
            (false, true, _) => '┬',
            _ => '─',
        })
        .collect();
    Line::from(text)
}

fn cell_layout(
    cell: &TableCell,
    width: usize,
    image_height: &mut impl FnMut(&str, usize) -> Option<usize>,
) -> (Vec<Line<'static>>, Vec<ImagePlacement>) {
    let mut lines = Vec::new();
    let mut images = Vec::new();
    for block in &cell.content {
        match block {
            ContentBlock::Text(line) => lines.extend(wrap_line(line.clone(), width)),
            ContentBlock::Image { url, alt } => {
                if let Some(height) = image_height(url, width) {
                    let height = height.max(1);
                    images.push(ImagePlacement {
                        row: lines.len(),
                        column: 0,
                        width,
                        height,
                        url: url.clone(),
                    });
                    lines.extend((0..height).map(|_| Line::default()));
                } else {
                    lines.extend(wrap_line(
                        Line::from(format!("[image: {alt}] {url}")),
                        width,
                    ));
                }
            }
            ContentBlock::Video { url, alt } => lines.extend(wrap_line(
                Line::from(format!("[video: {alt}] {url}")),
                width,
            )),
            _ => {}
        }
    }
    (lines, images)
}

pub(crate) fn html_blocks(html: &str, base: Style, pending: &mut bool) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();
    let mut table: Option<Table> = None;
    let mut row = Vec::new();
    let mut cell: Option<TableCell> = None;
    let mut header = false;
    let mut bold = false;
    let mut remaining = html;
    while !remaining.is_empty() {
        if remaining.starts_with("<!--") {
            let Some(end) = remaining.find("-->") else {
                break;
            };
            remaining = &remaining[end + 3..];
            continue;
        }
        if !remaining.starts_with('<') {
            let end = remaining.find('<').unwrap_or(remaining.len());
            if let Some(cell) = cell.as_mut() {
                let text: String = Parser::new(&remaining[..end])
                    .filter_map(|event| match event {
                        Event::Text(text) | Event::Code(text) => Some(text.into_string()),
                        Event::SoftBreak | Event::HardBreak => Some(" ".into()),
                        _ => None,
                    })
                    .collect();
                let leading = remaining[..end].starts_with(char::is_whitespace);
                let trailing = remaining[..end].ends_with(char::is_whitespace);
                let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
                let text = format!(
                    "{}{}{}",
                    if leading { " " } else { "" },
                    words,
                    if trailing && !words.is_empty() {
                        " "
                    } else {
                        ""
                    }
                );
                if !text.is_empty() {
                    let style = if header || bold {
                        base.add_modifier(Modifier::BOLD)
                    } else {
                        base
                    };
                    let span = Span::styled(text, style);
                    if let Some(ContentBlock::Text(line)) = cell.content.last_mut() {
                        line.spans.push(span);
                    } else {
                        cell.content.push(ContentBlock::Text(Line::from(span)));
                    }
                }
            }
            remaining = &remaining[end..];
            continue;
        }
        let mut quote = None;
        let end = remaining.char_indices().find_map(|(i, c)| {
            match (quote, c) {
                (Some(q), c) if q == c => quote = None,
                (None, '\'' | '"') => quote = Some(c),
                (None, '>') => return Some(i + 1),
                _ => {}
            }
            None
        });
        let Some(end) = end else { break };
        let tag = &remaining[..end];
        let name = tag[1..]
            .trim_start()
            .split(|c: char| c.is_whitespace() || c == '>')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match name.as_str() {
            "table" if table.is_none() => table = Some(Table::default()),
            "tr" if table.is_some() => row.clear(),
            "td" | "th" if table.is_some() => {
                if let Some(previous) = cell.take() {
                    row.push(previous);
                }
                header = name == "th";
                cell = Some(TableCell {
                    colspan: html_attr_value(tag, "colspan")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(1)
                        .clamp(1, 64),
                    content: Vec::new(),
                });
            }
            "/td" | "/th" => {
                if let Some(cell) = cell.take() {
                    row.push(cell);
                }
            }
            "/tr" => {
                if let Some(cell) = cell.take() {
                    row.push(cell);
                }
                if let Some(table) = table.as_mut()
                    && !row.is_empty()
                {
                    table.rows.push(std::mem::take(&mut row));
                }
            }
            "/table" => {
                if let Some(cell) = cell.take() {
                    row.push(cell);
                }
                if let Some(mut table) = table.take() {
                    if !row.is_empty() {
                        table.rows.push(std::mem::take(&mut row));
                    }
                    blocks.push(ContentBlock::Table(table));
                }
            }
            "b" | "strong" => bold = true,
            "/b" | "/strong" => bold = false,
            "br" | "br/" => {
                if let Some(cell) = cell.as_mut() {
                    cell.content.push(ContentBlock::Text(Line::default()));
                }
            }
            _ => {
                if let Some((kind, url, alt)) = media_from_html_tag(tag) {
                    let block = match kind {
                        MediaKind::Image => ContentBlock::Image { url, alt },
                        MediaKind::Video => ContentBlock::Video { url, alt },
                    };
                    if let Some(cell) = cell.as_mut() {
                        cell.content.push(block);
                    } else {
                        blocks.push(block);
                    }
                }
            }
        }
        remaining = &remaining[end..];
    }
    *pending = table.is_some();
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn images_share_a_row_and_fit_their_cells() {
        let blocks = html_blocks(
            r#"<table><tr><th>Before</th><th>After</th></tr><tr><td colspan="2">Scene</td></tr><tr><td><img src="a"></td><td><img src="b"></td></tr></table>"#,
            Style::default(),
            &mut false,
        );
        let ContentBlock::Table(table) = &blocks[0] else {
            panic!("missing table")
        };
        let layout = table.layout(41, |url, _| Some(if url == "a" { 4 } else { 2 }));
        assert_eq!(layout.images.len(), 2);
        assert_eq!(layout.images[0].row, layout.images[1].row);
        assert!(layout.images[0].column + layout.images[0].width < layout.images[1].column);
        assert!(layout.images[1].column + layout.images[1].width < 41);
        assert!(layout.lines.iter().all(|l| l.width() == 41));
        let scene = layout
            .lines
            .iter()
            .find(|l| l.to_string().contains("Scene"))
            .unwrap();
        assert_eq!(scene.to_string().matches('│').count(), 2);
        assert!(
            layout
                .lines
                .iter()
                .any(|l| l.to_string().contains("Before") && l.to_string().contains("After"))
        );
    }
    #[test]
    fn narrow_layout_preserves_both_images() {
        let blocks = html_blocks(
            r#"<table><tr><td><img src="a"></td><td><img src="b"></td></tr></table>"#,
            Style::default(),
            &mut false,
        );
        let ContentBlock::Table(table) = &blocks[0] else {
            panic!("missing table")
        };
        for width in [1, 3, 5, 7, 40] {
            let layout = table.layout(width, |_, _| Some(2));
            assert_eq!(layout.images.len(), 2);
            assert!(layout.lines.iter().all(|l| l.width() <= width));
            assert!(
                layout
                    .images
                    .iter()
                    .all(|image| image.column + image.width <= width)
            );
        }
    }
}
