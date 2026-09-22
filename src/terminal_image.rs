//! Images in a terminal's normal screen, without a frame or input probing.

use std::io::Write;

use anyhow::Result;
use image::DynamicImage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalImageProtocol {
    Kitty,
    Iterm2,
    /// ANSI true-color cells; transparent pixels are composited over black.
    Halfblocks,
}

#[derive(Debug, Clone, Copy)]
pub struct TerminalImageOptions {
    pub protocol: TerminalImageProtocol,
    pub font_size: (u16, u16),
    pub max_cells: (u16, u16),
    /// The caller must enable passthrough in its tmux session.
    pub tmux: bool,
}

impl Default for TerminalImageOptions {
    fn default() -> Self {
        Self {
            protocol: TerminalImageProtocol::Halfblocks,
            font_size: (8, 16),
            max_cells: (80, 20),
            tmux: false,
        }
    }
}

/// Write an image at the current line's left edge, ending on the next empty line.
///
/// No terminal query is sent and no input is read. Protocol support and tmux
/// passthrough are the caller's responsibility.
pub fn write_image(
    writer: &mut impl Write,
    image: &DynamicImage,
    options: TerminalImageOptions,
) -> Result<(u16, u16)> {
    let cells = crate::kitty::fitted_cells(
        (image.width(), image.height()),
        options.font_size,
        options.max_cells,
    )
    .ok_or_else(|| anyhow::anyhow!("Image and terminal dimensions must be positive"))?;
    match options.protocol {
        TerminalImageProtocol::Kitty => write_kitty(writer, image, options, cells)?,
        TerminalImageProtocol::Iterm2 => write_iterm2(writer, image, options, cells)?,
        TerminalImageProtocol::Halfblocks => write_halfblocks(writer, image, cells)?,
    }
    Ok(cells)
}

fn write_kitty(
    writer: &mut impl Write,
    image: &DynamicImage,
    options: TerminalImageOptions,
    cells: (u16, u16),
) -> Result<()> {
    let image = image.resize(
        (u32::from(cells.0) * u32::from(options.font_size.0)).min(image.width()),
        (u32::from(cells.1) * u32::from(options.font_size.1)).min(image.height()),
        image::imageops::FilterType::Triangle,
    );
    let id = crate::image::next_kitty_image_id();
    let transmit =
        crate::kitty::transmit_sequence_with_cells(&image, id, options.tmux, Some(cells));
    writer.write_all(transmit.as_bytes())?;
    let [tag, red, green, blue] = id.to_be_bytes();
    for row in 0..cells.1 {
        write!(writer, "\r\x1b[0;38;2;{red};{green};{blue}m")?;
        for col in 0..cells.0 {
            write!(
                writer,
                "{}{}{}{}",
                crate::kitty::PLACEHOLDER,
                crate::kitty::diacritic(row),
                crate::kitty::diacritic(col),
                crate::kitty::diacritic(u16::from(tag))
            )?;
        }
        writer.write_all(b"\x1b[0m\r\n")?;
    }
    Ok(())
}

fn write_iterm2(
    writer: &mut impl Write,
    image: &DynamicImage,
    options: TerminalImageOptions,
    cells: (u16, u16),
) -> Result<()> {
    use base64::Engine;
    anyhow::ensure!(
        !options.tmux,
        "iTerm2 images through tmux are not supported; use Kitty or halfblocks"
    );
    let png = crate::kitty::encode_png(image);
    let payload = base64::engine::general_purpose::STANDARD.encode(&png);
    write!(
        writer,
        "\r\x1b]1337;File=inline=1;width={};height={};preserveAspectRatio=1;size={}:{}\x07\x1b[0m\r\n",
        cells.0,
        cells.1,
        png.len(),
        payload
    )?;
    Ok(())
}

fn write_halfblocks(
    writer: &mut impl Write,
    image: &DynamicImage,
    cells: (u16, u16),
) -> Result<()> {
    let pixels = image
        .resize_exact(
            u32::from(cells.0),
            u32::from(cells.1) * 2,
            image::imageops::FilterType::Triangle,
        )
        .to_rgba8();
    for row in 0..cells.1 {
        writer.write_all(b"\r")?;
        for col in 0..cells.0 {
            let [red, green, blue] =
                on_black(*pixels.get_pixel(u32::from(col), u32::from(row) * 2));
            let [bg_red, bg_green, bg_blue] =
                on_black(*pixels.get_pixel(u32::from(col), u32::from(row) * 2 + 1));
            write!(
                writer,
                "\x1b[38;2;{red};{green};{blue};48;2;{bg_red};{bg_green};{bg_blue}m▀"
            )?;
        }
        writer.write_all(b"\x1b[0m\r\n")?;
    }
    Ok(())
}

fn on_black(pixel: image::Rgba<u8>) -> [u8; 3] {
    let alpha = u16::from(pixel[3]);
    [pixel[0], pixel[1], pixel[2]].map(|channel| (u16::from(channel) * alpha / 255) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use image::{ImageBuffer, Rgba};

    fn source() -> DynamicImage {
        DynamicImage::ImageRgba8(ImageBuffer::from_fn(32, 32, |x, y| {
            Rgba([x as u8 * 8, y as u8 * 8, 128, 255])
        }))
    }

    fn kitty_png(output: &str) -> DynamicImage {
        let mut png = Vec::new();
        for command in output.split("\x1b_G").skip(1) {
            let payload = command
                .split_once(';')
                .unwrap()
                .1
                .split("\x1b\\")
                .next()
                .unwrap();
            png.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(payload)
                    .unwrap(),
            );
        }
        image::load_from_memory(&png).unwrap()
    }

    fn options(protocol: TerminalImageProtocol) -> TerminalImageOptions {
        TerminalImageOptions {
            protocol,
            font_size: (8, 16),
            max_cells: (20, 8),
            tmux: false,
        }
    }

    #[test]
    fn kitty_output_preserves_cells_and_leaves_the_next_line_unstyled() {
        let mut output = Vec::new();
        let cells = write_image(
            &mut output,
            &source(),
            options(TerminalImageProtocol::Kitty),
        )
        .unwrap();
        assert_eq!(cells, (4, 2));
        output.extend_from_slice(b"after\n");
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains(",c=4,r=2,"));
        assert_eq!(output.matches('\u{10eeee}').count(), 8);
        assert_eq!(output.matches("\r\n").count(), 2);
        assert!(output.ends_with("\x1b[0m\r\nafter\n"));
        assert!(!output.contains("?1049"));
        assert!(!output.contains("a=d"));
    }

    #[test]
    fn kitty_png_payload_matches_the_placement_pixel_size() {
        let mut output = Vec::new();
        write_image(
            &mut output,
            &source(),
            options(TerminalImageProtocol::Kitty),
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        let image = kitty_png(&output);
        assert_eq!((image.width(), image.height()), (32, 32));
    }

    #[test]
    fn tmux_wraps_only_graphics_commands_and_keeps_placeholder_rows_visible() {
        let mut output = Vec::new();
        let mut options = options(TerminalImageProtocol::Kitty);
        options.tmux = true;
        write_image(&mut output, &source(), options).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\x1bPtmux;\x1b\x1b_Gq=2,"));
        let placeholders = output.find('\u{10eeee}').unwrap();
        assert!(output[..placeholders].ends_with("m"));
        assert!(!output[placeholders..].contains("tmux;"));
    }

    #[test]
    fn halfblocks_use_two_pixel_rows_and_reset_each_terminal_row() {
        let mut output = Vec::new();
        let mut options = options(TerminalImageProtocol::Halfblocks);
        options.max_cells = (2, 1);
        assert_eq!(
            write_image(&mut output, &source(), options).unwrap(),
            (2, 1)
        );
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.matches('▀').count(), 2);
        assert!(output.contains("38;2;"));
        assert!(output.contains("48;2;"));
        assert!(output.ends_with("\x1b[0m\r\n"));
        assert!(!output.contains("_G"));
    }

    #[test]
    fn invalid_geometry_writes_nothing() {
        for (font_size, max_cells) in [((0, 16), (20, 8)), ((8, 16), (0, 8))] {
            let mut output = Vec::new();
            let mut options = options(TerminalImageProtocol::Kitty);
            options.font_size = font_size;
            options.max_cells = max_cells;
            assert!(write_image(&mut output, &source(), options).is_err());
            assert!(output.is_empty());
        }
    }

    #[test]
    fn iterm2_transfers_an_inline_png_and_leaves_the_next_line() {
        let mut output = Vec::new();
        write_image(
            &mut output,
            &source(),
            options(TerminalImageProtocol::Iterm2),
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("1337;File=inline=1;width=4;height=2;"));
        assert!(output.ends_with("\x07\x1b[0m\r\n"));
    }

    #[test]
    fn iterm2_through_tmux_is_rejected_before_any_output() {
        let mut output = Vec::new();
        let mut options = options(TerminalImageProtocol::Iterm2);
        options.tmux = true;
        assert!(write_image(&mut output, &source(), options).is_err());
        assert!(output.is_empty());
    }

    #[test]
    fn large_font_metrics_do_not_allocate_upscaled_pixels() {
        let mut output = Vec::new();
        let mut options = options(TerminalImageProtocol::Kitty);
        options.font_size = (512, 512);
        write_image(&mut output, &source(), options).unwrap();
        let output = String::from_utf8(output).unwrap();
        let image = kitty_png(&output);
        assert_eq!((image.width(), image.height()), (32, 32));
    }

    #[test]
    fn write_errors_are_returned() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let error = write_image(
            &mut Broken,
            &source(),
            options(TerminalImageProtocol::Kitty),
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::BrokenPipe
        );
    }
}
