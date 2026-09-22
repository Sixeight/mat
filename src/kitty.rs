//! Kitty graphics rendering that survives tmux.
//!
//! `ratatui-image` packs a whole row of unicode placeholders into the row's
//! first buffer cell and marks the rest skipped. Outside tmux the terminal
//! parses that stream directly and it works, but tmux stores the run as
//! combining marks on a single cell — a repaint then emits one cell per row
//! instead of the full width, and the image collapses to nothing. Writing one
//! placeholder per cell, each carrying its own row/column/id diacritics, is
//! what tmux round-trips faithfully (verified against tmux 3.7).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

/// Row/column encoding table from the kitty graphics protocol.
/// <https://sw.kovidgoyal.net/kitty/graphics-protocol/#unicode-placeholders>
const DIACRITICS: [u32; 297] = [
    0x305, 0x30D, 0x30E, 0x310, 0x312, 0x33D, 0x33E, 0x33F, 0x346, 0x34A, 0x34B, 0x34C, 0x350,
    0x351, 0x352, 0x357, 0x35B, 0x363, 0x364, 0x365, 0x366, 0x367, 0x368, 0x369, 0x36A, 0x36B,
    0x36C, 0x36D, 0x36E, 0x36F, 0x483, 0x484, 0x485, 0x486, 0x487, 0x592, 0x593, 0x594, 0x595,
    0x597, 0x598, 0x599, 0x59C, 0x59D, 0x59E, 0x59F, 0x5A0, 0x5A1, 0x5A8, 0x5A9, 0x5AB, 0x5AC,
    0x5AF, 0x5C4, 0x610, 0x611, 0x612, 0x613, 0x614, 0x615, 0x616, 0x617, 0x657, 0x658, 0x659,
    0x65A, 0x65B, 0x65D, 0x65E, 0x6D6, 0x6D7, 0x6D8, 0x6D9, 0x6DA, 0x6DB, 0x6DC, 0x6DF, 0x6E0,
    0x6E1, 0x6E2, 0x6E4, 0x6E7, 0x6E8, 0x6EB, 0x6EC, 0x730, 0x732, 0x733, 0x735, 0x736, 0x73A,
    0x73D, 0x73F, 0x740, 0x741, 0x743, 0x745, 0x747, 0x749, 0x74A, 0x7EB, 0x7EC, 0x7ED, 0x7EE,
    0x7EF, 0x7F0, 0x7F1, 0x7F3, 0x816, 0x817, 0x818, 0x819, 0x81B, 0x81C, 0x81D, 0x81E, 0x81F,
    0x820, 0x821, 0x822, 0x823, 0x825, 0x826, 0x827, 0x829, 0x82A, 0x82B, 0x82C, 0x82D, 0x951,
    0x953, 0x954, 0xF82, 0xF83, 0xF86, 0xF87, 0x135D, 0x135E, 0x135F, 0x17DD, 0x193A, 0x1A17,
    0x1A75, 0x1A76, 0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F,
    0x1B70, 0x1B71, 0x1B72, 0x1B73, 0x1CD0, 0x1CD1, 0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1,
    0x1DC3, 0x1DC4, 0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3,
    0x1DD4, 0x1DD5, 0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF,
    0x1DE0, 0x1DE1, 0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1, 0x20D4, 0x20D5,
    0x20D6, 0x20D7, 0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0, 0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0,
    0x2DE1, 0x2DE2, 0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6, 0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC,
    0x2DED, 0x2DEE, 0x2DEF, 0x2DF0, 0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8,
    0x2DF9, 0x2DFA, 0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1,
    0xA8E0, 0xA8E1, 0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB,
    0xA8EC, 0xA8ED, 0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2, 0xAAB3, 0xAAB7, 0xAAB8, 0xAABE,
    0xAABF, 0xAAC1, 0xFE20, 0xFE21, 0xFE22, 0xFE23, 0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38,
    0x1D185, 0x1D186, 0x1D187, 0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242,
    0x1D243, 0x1D244,
];

pub(crate) const PLACEHOLDER: char = '\u{10EEEE}';
/// Payload bytes per transmit chunk (4096 base64 characters).
const CHUNK_BYTES: usize = 4096 / 4 * 3;

/// Whether output goes through tmux, which needs passthrough-wrapped escapes.
pub(super) fn in_tmux() -> bool {
    use std::sync::OnceLock;
    static IN_TMUX: OnceLock<bool> = OnceLock::new();
    *IN_TMUX.get_or_init(|| {
        std::env::var("TERM").is_ok_and(|term| term.starts_with("tmux"))
            || std::env::var("TERM_PROGRAM").is_ok_and(|program| program == "tmux")
    })
}

pub(crate) fn diacritic(index: u16) -> char {
    let index = usize::from(index).min(DIACRITICS.len() - 1);
    char::from_u32(DIACRITICS[index]).unwrap_or('\u{305}')
}

/// Cells an image occupies inside `area`, preserving its aspect ratio and
/// never scaling up beyond the source resolution.
pub(crate) fn fitted_cells(
    image: (u32, u32),
    font_size: (u16, u16),
    area: (u16, u16),
) -> Option<(u16, u16)> {
    let (image_width, image_height) = image;
    let (font_width, font_height) = font_size;
    let (cols, rows) = area;
    if image_width == 0 || image_height == 0 || font_width == 0 || font_height == 0 {
        return None;
    }
    if cols == 0 || rows == 0 {
        return None;
    }
    let avail_width = f64::from(cols) * f64::from(font_width);
    let avail_height = f64::from(rows) * f64::from(font_height);
    let scale = (avail_width / f64::from(image_width))
        .min(avail_height / f64::from(image_height))
        .min(1.0);
    let width_cells = ((f64::from(image_width) * scale) / f64::from(font_width)).round() as u16;
    let height_cells = ((f64::from(image_height) * scale) / f64::from(font_height)).round() as u16;
    // A placeholder carries its row and column as a diacritic, so a placement
    // wider or taller than the table can encode would repeat its last row and
    // column instead of ending there.
    let encodable = DIACRITICS.len() as u16;
    Some((
        width_cells.clamp(1, cols.min(encodable)),
        height_cells.clamp(1, rows.min(encodable)),
    ))
}

/// Escape sequence transmitting `image` under `id` as a virtual placement.
///
/// The pixels travel as PNG (`f=100`): a screenshot compresses to a fraction
/// of its raw size, and every retransmit has to cross tmux, so the encode buys
/// back far more than it costs.
pub(crate) fn transmit_sequence(image: &image::DynamicImage, id: u32, in_tmux: bool) -> String {
    transmit_sequence_with_cells(image, id, in_tmux, None)
}

pub(crate) fn transmit_sequence_with_cells(
    image: &image::DynamicImage,
    id: u32,
    in_tmux: bool,
    cells: Option<(u16, u16)>,
) -> String {
    use base64::Engine;
    use std::fmt::Write;

    let placement = cells.map_or_else(String::new, |(cols, rows)| format!("c={cols},r={rows},"));
    let (width, height) = (image.width(), image.height());
    let encoded = encode_png(image);
    let chunks: Vec<&[u8]> = encoded.chunks(CHUNK_BYTES).collect();
    let chunk_count = chunks.len();

    // In tmux every escape must be doubled and wrapped in a passthrough DCS,
    // which tmux unwraps towards the outer terminal. Each chunk gets its own
    // wrapper: tmux discards a passthrough sequence larger than 1 MB outright,
    // and a full-size screenshot is comfortably past that in one piece.
    // (Measured against tmux 3.7: 900 KB passes, 1.2 MB vanishes.)
    let (open, esc, close) = if in_tmux {
        ("\u{1b}Ptmux;", "\u{1b}\u{1b}", "\u{1b}\\")
    } else {
        ("", "\u{1b}", "")
    };

    let mut sequence = String::new();
    for (index, chunk) in chunks.into_iter().enumerate() {
        let payload = base64::engine::general_purpose::STANDARD.encode(chunk);
        let more = u8::from(index + 1 < chunk_count);
        sequence.push_str(open);
        sequence.push_str(esc);
        if index == 0 {
            let _ = write!(
                sequence,
                "_Gq=2,i={id},a=T,U=1,{placement}f=100,t=d,s={width},v={height},m={more};{payload}"
            );
        } else {
            let _ = write!(sequence, "_Gq=2,m={more};{payload}");
        }
        sequence.push_str(esc);
        sequence.push('\\');
        sequence.push_str(close);
    }
    sequence
}

pub(crate) fn encode_png(image: &image::DynamicImage) -> Vec<u8> {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    use image::{ImageEncoder, error::ImageError};

    let rgba = image.to_rgba8();
    let mut encoded = Vec::new();
    let result = PngEncoder::new_with_quality(&mut encoded, CompressionType::Fast, FilterType::Sub)
        .write_image(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            image::ExtendedColorType::Rgba8,
        );
    match result {
        Ok(()) => encoded,
        // Nothing sensible to fall back to; an empty payload just means the
        // terminal ignores the transmission.
        Err(ImageError::IoError(_)) | Err(_) => Vec::new(),
    }
}

/// Largest passthrough sequence tmux forwards; anything bigger is dropped.
#[cfg(test)]
const TMUX_PASSTHROUGH_LIMIT: usize = 1024 * 1024;

/// Fill `area` with the placeholders that display image `id`.
///
/// `first_row` is the image row the top of `area` shows, so an image scrolled
/// half out of the pane keeps displaying its lower half rather than being
/// squeezed into the space that is left.
///
/// `transmit` is emitted from the first cell, which is the only way to get
/// bytes to the terminal from inside a frame.
pub(super) fn render_placeholders(
    area: Rect,
    buffer: &mut Buffer,
    id: u32,
    transmit: Option<&str>,
    first_row: u16,
) {
    let [id_extra, red, green, blue] = id.to_be_bytes();
    let mut transmit = transmit;

    for row in 0..area.height {
        for col in 0..area.width {
            let Some(cell) = buffer.cell_mut((area.x + col, area.y + row)) else {
                continue;
            };
            let mut symbol = transmit.take().unwrap_or_default().to_string();
            symbol.push(PLACEHOLDER);
            symbol.push(diacritic(first_row.saturating_add(row)));
            symbol.push(diacritic(col));
            symbol.push(diacritic(u16::from(id_extra)));
            cell.set_symbol(&symbol);
            cell.set_fg(Color::Rgb(red, green, blue));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wide_image_fits_the_area_and_keeps_its_ratio() {
        // 1280x720 into 100x20 cells of 8x17px: the 340px of height binds, so
        // the image lands at 604x340px — 76 cells wide, all 20 rows tall.
        assert_eq!(
            fitted_cells((1280, 720), (8, 17), (100, 20)),
            Some((76, 20))
        );
    }

    #[test]
    fn a_small_image_is_never_scaled_up() {
        assert_eq!(fitted_cells((80, 34), (8, 17), (100, 20)), Some((10, 2)));
    }

    #[test]
    fn a_placement_stays_inside_the_columns_the_protocol_can_encode() {
        // A wide pane fitting an ultra-wide image: the placement would span
        // more columns than there are diacritics, and everything past the last
        // one would alias onto it.
        let (cols, rows) = fitted_cells((4000, 400), (8, 16), (400, 20)).expect("placement");
        assert!(cols <= DIACRITICS.len() as u16, "{cols} columns");
        assert!(rows <= DIACRITICS.len() as u16, "{rows} rows");
    }

    #[test]
    fn degenerate_sizes_have_no_placement() {
        assert_eq!(fitted_cells((0, 10), (8, 17), (10, 10)), None);
        assert_eq!(fitted_cells((10, 10), (0, 17), (10, 10)), None);
        assert_eq!(fitted_cells((10, 10), (8, 17), (0, 10)), None);
    }

    #[test]
    fn every_cell_carries_its_own_row_and_column() {
        let area = Rect::new(0, 0, 3, 2);
        let mut buffer = Buffer::empty(area);
        render_placeholders(area, &mut buffer, 0x01_02_03_04, None, 0);

        let cell = buffer.cell((2, 1)).unwrap();
        let expected: String = [PLACEHOLDER, diacritic(1), diacritic(2), diacritic(1)]
            .into_iter()
            .collect();
        assert_eq!(cell.symbol(), expected);
        assert_eq!(cell.fg, Color::Rgb(0x02, 0x03, 0x04));
    }

    #[test]
    fn the_transmit_rides_on_the_first_cell_only() {
        let area = Rect::new(0, 0, 2, 1);
        let mut buffer = Buffer::empty(area);
        render_placeholders(area, &mut buffer, 1, Some("SEQ"), 0);

        assert!(buffer.cell((0, 0)).unwrap().symbol().starts_with("SEQ"));
        assert!(!buffer.cell((1, 0)).unwrap().symbol().contains("SEQ"));
    }

    #[test]
    fn a_scrolled_image_shows_its_lower_rows() {
        let area = Rect::new(0, 0, 1, 2);
        let mut buffer = Buffer::empty(area);
        render_placeholders(area, &mut buffer, 1, None, 7);

        // The top of the visible area is image row 7, not row 0.
        assert!(buffer.cell((0, 0)).unwrap().symbol().contains(diacritic(7)));
        assert!(buffer.cell((0, 1)).unwrap().symbol().contains(diacritic(8)));
    }

    #[test]
    fn the_transmit_is_wrapped_for_tmux() {
        let image = image::DynamicImage::new_rgba8(2, 2);
        let plain = transmit_sequence(&image, 7, false);
        assert!(plain.starts_with("\u{1b}_Gq=2,i=7,a=T,U=1,f=100,t=d,s=2,v=2,m=0;"));
        assert!(!plain.contains("Ptmux;"));

        let tmuxed = transmit_sequence(&image, 7, true);
        assert!(tmuxed.starts_with("\u{1b}Ptmux;"));
        assert!(tmuxed.contains("\u{1b}\u{1b}_Gq=2,i=7"));
        assert!(tmuxed.ends_with("\u{1b}\\"));
    }

    #[test]
    fn no_single_passthrough_sequence_can_be_dropped_by_tmux() {
        // A full-width screenshot: one wrapper around everything would be past
        // 1 MB and tmux would silently discard the whole transmission. Noise,
        // so PNG cannot compress the payload below the limit.
        let mut pixels = image::RgbaImage::new(1200, 900);
        let mut state: u32 = 0x1234_5678;
        let mut next = move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        };
        for pixel in pixels.pixels_mut() {
            *pixel = image::Rgba([next(), next(), next(), 255]);
        }
        let sequence = transmit_sequence(&image::DynamicImage::ImageRgba8(pixels), 9, true);
        assert!(
            sequence.len() > TMUX_PASSTHROUGH_LIMIT,
            "test image too small"
        );

        let longest = sequence
            .split("\u{1b}Ptmux;")
            .map(str::len)
            .max()
            .unwrap_or_default();
        assert!(
            longest < TMUX_PASSTHROUGH_LIMIT,
            "a {longest} byte passthrough sequence would be dropped"
        );
    }
}
