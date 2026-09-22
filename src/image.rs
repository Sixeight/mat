use ratatui::{Frame, layout::Rect};
use ratatui_image::{
    picker::{Picker, ProtocolType},
    protocol::StatefulProtocol,
};

use super::{hash_str, kitty};

mod svg;

/// Largest encoded image accepted for decoding.
pub const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
/// Longest retained image edge in pixels.
pub const MAX_IMAGE_EDGE: u32 = 1280;

pub fn decode_image(bytes: &[u8]) -> anyhow::Result<image::DynamicImage> {
    use anyhow::Context;
    anyhow::ensure!(
        bytes.len() <= MAX_IMAGE_BYTES,
        "Image is too large to display ({} bytes)",
        bytes.len()
    );
    if image::guess_format(bytes).is_err() {
        return svg::decode(bytes).context("Failed to decode image");
    }
    let image = image::load_from_memory(bytes).context("Failed to decode image")?;
    let longest = image.width().max(image.height());
    if longest <= MAX_IMAGE_EDGE {
        return Ok(image);
    }
    let ratio = f64::from(MAX_IMAGE_EDGE) / f64::from(longest);
    let width = ((f64::from(image.width()) * ratio).round() as u32).max(1);
    let height = ((f64::from(image.height()) * ratio).round() as u32).max(1);
    Ok(image.resize_exact(width, height, image::imageops::FilterType::Triangle))
}

/// A decoded image and its cached terminal protocol state.
///
/// Kitty terminals may discard pixels after their last placeholder leaves the
/// screen. Returning into view therefore requires another transmission.
pub struct LoadedImage {
    source: image::DynamicImage,
    /// Other protocols carry their pixels in each frame; Kitty uses placeholders.
    protocol: Option<StatefulProtocol>,
    kitty: Option<KittyPlacement>,
}

struct KittyPlacement {
    id: u32,
    cells: (u16, u16),
    /// Kept so a retransmit costs a memcpy instead of a resize, a PNG encode
    /// and a base64 pass.
    transmit: std::rc::Rc<str>,
    /// Sends still owed for these pixels. tmux silently drops passthrough
    /// output while a pane redraw is pending — measured against tmux 3.7, two
    /// of three back-to-back sends vanish, and all of them do while a pane is
    /// scrolling. Repeating a few times, spaced out, is what actually lands.
    sends_left: u8,
    /// Frames to wait before the next send.
    send_cooldown: u8,
    /// Where the placeholders were drawn last frame. While this keeps moving
    /// the pane is redrawing, which is exactly when tmux throws passthrough
    /// away, so the send budget is refilled instead of being spent.
    last_geometry: (u16, u16, u16, u16),
}

/// How many times a placement sends its pixels, and how many frames apart. One
/// resend covers a send that tmux dropped; more only adds traffic, since a
/// settled pane stops redrawing and the budget expires unused anyway.
const KITTY_SEND_ATTEMPTS: u8 = 2;
const KITTY_SEND_SPACING: u8 = 8;

/// Where this process starts handing image ids out.
///
/// Ids live in the terminal, and tmux delivers every pane's passthrough to the
/// same one. Counting from a fixed value would make two processes claim the
/// same id and replace each other's pixels, so the run starts from a hash of
/// the pid — which scatters the pids that panes actually get, consecutive
/// ones included, rather than starting them a few ids apart.
fn kitty_id_base(pid: u32) -> u32 {
    (hash_str(&pid.to_string()) & 0x00FF_FFFF) as u32
}

pub(crate) fn next_kitty_image_id() -> u32 {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: OnceLock<AtomicU32> = OnceLock::new();
    let next = NEXT.get_or_init(|| AtomicU32::new(kitty_id_base(std::process::id())));
    // The low three bytes travel as the placeholder's foreground colour and
    // the top byte as a diacritic, so keep the id inside 24 bits plus a fixed
    // non-zero tag.
    0x0100_0000 | (next.fetch_add(1, Ordering::Relaxed) & 0x00FF_FFFF)
}

impl LoadedImage {
    pub fn new(source: image::DynamicImage) -> Self {
        Self {
            source,
            protocol: None,
            kitty: None,
        }
    }

    pub fn render(
        &mut self,
        frame: &mut Frame,
        picker: &Picker,
        full_slot: Rect,
        visible_height: u16,
        first_row: u16,
        first_of_frame: bool,
    ) -> bool {
        if full_slot.width == 0 || full_slot.height == 0 || visible_height == 0 {
            return false;
        }
        if picker.protocol_type() == ProtocolType::Kitty {
            // The full slot keeps scrolling from changing the image's scale.
            let Some((placement, id, transmit)) =
                self.kitty_placement(picker.font_size(), full_slot, kitty::in_tmux())
            else {
                return false;
            };
            let visible = Rect::new(
                placement.x,
                placement.y,
                placement.width,
                placement
                    .height
                    .saturating_sub(first_row)
                    .min(visible_height),
            );
            if visible.height == 0 {
                return false;
            }
            kitty::render_placeholders(
                visible,
                frame.buffer_mut(),
                id,
                transmit.as_deref(),
                first_row,
            );
            self.mark_kitty_drawn(
                transmit.is_some(),
                (visible.x, visible.y, visible.height, first_row),
                first_of_frame,
            );
        } else {
            let visible = Rect::new(
                full_slot.x,
                full_slot.y,
                full_slot.width,
                full_slot.height.min(visible_height),
            );
            frame.render_stateful_widget(
                ratatui_image::StatefulImage::default(),
                visible,
                self.protocol_mut(picker),
            );
        }
        true
    }

    /// Placement for the kitty renderer: where the image goes, under which id,
    /// and the transmit sequence when the pixels have to be sent again.
    pub fn kitty_placement(
        &mut self,
        font_size: (u16, u16),
        area: Rect,
        in_tmux: bool,
    ) -> Option<(Rect, u32, Option<std::rc::Rc<str>>)> {
        let cells = kitty::fitted_cells(
            (self.source.width(), self.source.height()),
            font_size,
            (area.width, area.height),
        )?;
        let placement = Rect::new(area.x, area.y, cells.0, cells.1);

        if let Some(existing) = &self.kitty
            && existing.cells == cells
        {
            // Same size: send the pixels only while sends remain (a release
            // arms them again).
            let due = existing.sends_left > 0 && existing.send_cooldown == 0;
            let transmit = due.then(|| existing.transmit.clone());
            return Some((placement, existing.id, transmit));
        }

        // A new size needs new pixels. The id is kept across resizes so the
        // terminal replaces the image instead of accumulating one per size.
        let id = self
            .kitty
            .as_ref()
            .map_or_else(next_kitty_image_id, |k| k.id);
        let width = u32::from(cells.0) * u32::from(font_size.0);
        let height = u32::from(cells.1) * u32::from(font_size.1);
        let resized =
            self.source
                .resize_exact(width, height, image::imageops::FilterType::Triangle);
        let transmit: std::rc::Rc<str> = kitty::transmit_sequence(&resized, id, in_tmux).into();
        self.kitty = Some(KittyPlacement {
            id,
            cells,
            transmit: transmit.clone(),
            sends_left: KITTY_SEND_ATTEMPTS,
            send_cooldown: 0,
            last_geometry: (u16::MAX, u16::MAX, u16::MAX, u16::MAX),
        });
        Some((placement, id, Some(transmit)))
    }

    pub fn protocol_mut(&mut self, picker: &Picker) -> &mut StatefulProtocol {
        if self.protocol.is_none() {
            self.protocol = Some(picker.new_resize_protocol(self.source.clone()));
        }
        self.protocol
            .as_mut()
            .expect("protocol was just initialized")
    }

    /// Confirm the pixels reached the terminal. Only the caller knows whether
    /// the placeholders were really drawn — a placement scrolled fully out of
    /// view is skipped, and a transmit dropped there would leave the image
    /// blank when it scrolls back.
    /// `first_of_frame` is false for the second and later places the same URL
    /// is drawn in one frame. They hold different geometry by definition, and
    /// a placement that took them for movement would refill its budget forever
    /// on a screen that is standing still.
    pub fn mark_kitty_drawn(
        &mut self,
        sent: bool,
        geometry: (u16, u16, u16, u16),
        first_of_frame: bool,
    ) {
        let Some(placement) = &mut self.kitty else {
            return;
        };
        if first_of_frame && placement.last_geometry != geometry {
            // Moving: the budget is refilled, but the spacing still applies —
            // a pane scrolled with a held key would otherwise ship the whole
            // image on every redraw, which is what keeps the pane redrawing.
            placement.last_geometry = geometry;
            placement.sends_left = KITTY_SEND_ATTEMPTS;
        }
        if sent {
            placement.sends_left = placement.sends_left.saturating_sub(1);
            placement.send_cooldown = KITTY_SEND_SPACING;
        } else {
            placement.send_cooldown = placement.send_cooldown.saturating_sub(1);
        }
    }

    /// Forget the transmitted state so the next render sends the pixels again.
    ///
    /// Only kitty needs this: every other protocol writes its pixels into the
    /// cells on each render, so dropping it here would just buy a re-encode
    /// each time an image scrolls back into view.
    pub fn release(&mut self) {
        if let Some(placement) = &mut self.kitty {
            placement.sends_left = KITTY_SEND_ATTEMPTS;
            placement.send_cooldown = 0;
        }
    }

    /// Also drop what the other protocols built — after a clear or a resize
    /// the terminal has thrown away whatever they were holding.
    pub fn release_all(&mut self) {
        self.release();
        self.protocol = None;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn has_protocol(&self) -> bool {
        self.protocol.is_some()
    }

    /// Sends still owed for the current placement, and the frames until the
    /// next one — a released placement is back at a full budget, due now.
    #[cfg(any(test, feature = "test-support"))]
    pub fn kitty_send_state(&self) -> Option<(u8, u8)> {
        self.kitty
            .as_ref()
            .map(|placement| (placement.sends_left, placement.send_cooldown))
    }
}

#[cfg(test)]
mod loaded_image_tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    const FONT: (u16, u16) = (8, 16);

    /// Frames enough for the budget to be spent and the cooldown to expire.
    const FRAMES: u16 = KITTY_SEND_SPACING as u16 * (KITTY_SEND_ATTEMPTS as u16 + 2);

    fn loaded() -> LoadedImage {
        LoadedImage::new(image::DynamicImage::new_rgba8(64, 64))
    }

    fn render(
        loaded: &mut LoadedImage,
        protocol: ProtocolType,
        visible_height: u16,
        first_row: u16,
    ) -> (bool, ratatui::buffer::Buffer) {
        let mut picker = Picker::from_fontsize(FONT);
        picker.set_protocol_type(protocol);
        let mut terminal = Terminal::new(TestBackend::new(20, 8)).unwrap();
        let mut drawn = false;
        let frame = terminal
            .draw(|frame| {
                drawn = loaded.render(
                    frame,
                    &picker,
                    Rect::new(1, 1, 18, 6),
                    visible_height,
                    first_row,
                    true,
                );
            })
            .unwrap();
        (drawn, frame.buffer.clone())
    }

    #[test]
    fn a_partially_visible_kitty_image_keeps_its_full_size() {
        let mut loaded = loaded();
        let (drawn, buffer) = render(&mut loaded, ProtocolType::Kitty, 2, 1);
        assert!(drawn);
        assert_eq!(loaded.kitty.as_ref().unwrap().cells, (8, 4));
        assert!(buffer[(1, 1)].symbol().contains("\u{10EEEE}\u{30D}"));
        assert!(buffer[(1, 2)].symbol().contains("\u{10EEEE}\u{30E}"));
        assert_eq!(buffer[(1, 3)].symbol(), " ");
    }

    #[test]
    fn a_kitty_image_scrolled_past_its_last_row_keeps_its_send_budget() {
        let mut loaded = loaded();
        let (drawn, buffer) = render(&mut loaded, ProtocolType::Kitty, 6, 4);
        assert!(!drawn);
        assert_eq!(loaded.kitty_send_state(), Some((KITTY_SEND_ATTEMPTS, 0)));
        assert!(buffer.content.iter().all(|cell| cell.symbol() == " "));
    }

    #[test]
    fn an_empty_viewport_does_not_initialize_image_protocols() {
        for protocol in [ProtocolType::Kitty, ProtocolType::Halfblocks] {
            let mut loaded = loaded();
            let (drawn, _) = render(&mut loaded, protocol, 0, 0);
            assert!(!drawn);
            assert!(!loaded.has_protocol());
            assert!(loaded.kitty.is_none());
        }
    }

    #[test]
    fn other_protocols_keep_rendering_inside_the_visible_height() {
        let mut loaded = loaded();
        let (drawn, buffer) = render(&mut loaded, ProtocolType::Halfblocks, 2, 1);
        assert!(drawn);
        assert!(loaded.has_protocol());
        assert!(loaded.kitty.is_none());
        assert_eq!(buffer[(1, 1)].symbol(), "▀");
        assert_eq!(buffer[(1, 2)].symbol(), "▀");
        assert_eq!(buffer[(1, 3)].symbol(), " ");
    }

    /// Draws `frames` frames, moving the placement down a row each frame when
    /// `scrolling`, and returns how many of them transmitted the pixels.
    fn transmits(loaded: &mut LoadedImage, frames: u16, scrolling: bool) -> usize {
        let mut sent = 0;
        for frame in 0..frames {
            let y = if scrolling { frame } else { 0 };
            let area = Rect::new(0, y, 20, 8);
            let Some((placement, _, transmit)) = loaded.kitty_placement(FONT, area, false) else {
                panic!("a 64x64 image always has a placement in 20x8 cells");
            };
            sent += usize::from(transmit.is_some());
            loaded.mark_kitty_drawn(
                transmit.is_some(),
                (placement.x, placement.y, placement.height, 0),
                true,
            );
        }
        sent
    }

    #[test]
    fn a_settled_placement_transmits_exactly_its_send_budget() {
        assert_eq!(
            transmits(&mut loaded(), FRAMES, false),
            usize::from(KITTY_SEND_ATTEMPTS)
        );
    }

    #[test]
    fn a_moving_placement_transmits_once_per_cooldown_not_once_per_frame() {
        // Scrolling refills the budget, so the sends keep coming — but spaced,
        // or a scrolled pane ships the whole image on every redraw.
        let sent = transmits(&mut loaded(), FRAMES, true);
        assert!(
            sent <= usize::from(FRAMES / u16::from(KITTY_SEND_SPACING)) + 1,
            "{sent} transmits in {FRAMES} scrolled frames"
        );
        assert!(
            sent > 0,
            "a moving placement still has to reach the terminal"
        );
    }

    /// The transmit rides in a cell's symbol, so it only reaches the terminal
    /// when ratatui's diff emits that cell. Two frames running with the same
    /// symbol emit once — and the second would still be counted as sent, which
    /// is how a placement runs out of budget having transmitted nothing.
    #[test]
    fn no_two_frames_in_a_row_carry_the_transmit() {
        for scrolling in [false, true] {
            let mut loaded = loaded();
            let mut previous = false;
            for frame in 0..FRAMES {
                let y = if scrolling { frame } else { 0 };
                let area = Rect::new(0, y, 20, 8);
                let (placement, _, transmit) = loaded
                    .kitty_placement(FONT, area, false)
                    .expect("placement");
                let sent = transmit.is_some();
                assert!(
                    !(sent && previous),
                    "frames {} and {frame} both transmit (scrolling: {scrolling})",
                    frame - 1
                );
                previous = sent;
                loaded.mark_kitty_drawn(
                    sent,
                    (placement.x, placement.y, placement.height, 0),
                    true,
                );
            }
        }
    }

    #[test]
    fn a_released_placement_transmits_again() {
        let mut loaded = loaded();
        assert_eq!(
            transmits(&mut loaded, FRAMES, false),
            usize::from(KITTY_SEND_ATTEMPTS)
        );
        loaded.release();
        assert_eq!(
            transmits(&mut loaded, FRAMES, false),
            usize::from(KITTY_SEND_ATTEMPTS)
        );
    }

    #[test]
    fn the_same_image_shown_twice_still_settles_after_its_send_budget() {
        // One URL pasted in two comments: a single placement is drawn at two
        // places per frame, and the two geometries must not read as movement.
        let mut loaded = loaded();
        let mut sent = 0;
        for _ in 0..FRAMES {
            for (index, area) in [Rect::new(0, 0, 20, 8), Rect::new(0, 24, 20, 8)]
                .into_iter()
                .enumerate()
            {
                let (placement, _, transmit) = loaded
                    .kitty_placement(FONT, area, false)
                    .expect("placement");
                sent += usize::from(transmit.is_some());
                loaded.mark_kitty_drawn(
                    transmit.is_some(),
                    (placement.x, placement.y, placement.height, 0),
                    index == 0,
                );
            }
        }
        assert_eq!(sent, usize::from(KITTY_SEND_ATTEMPTS));
    }

    #[test]
    fn a_resize_between_frames_does_not_stop_the_pixels_from_arriving() {
        let mut loaded = loaded();
        let narrow = Rect::new(0, 0, 20, 8);
        let wide = Rect::new(0, 0, 40, 8);
        let mut sent = 0;
        for frame in 0..FRAMES {
            let area = if frame == 1 { wide } else { narrow };
            let (placement, _, transmit) = loaded
                .kitty_placement(FONT, area, false)
                .expect("placement");
            sent += usize::from(transmit.is_some());
            loaded.mark_kitty_drawn(
                transmit.is_some(),
                (placement.x, placement.y, placement.height, 0),
                true,
            );
        }
        // The resize sends its own pixels, then the budget settles as usual.
        assert!(sent >= usize::from(KITTY_SEND_ATTEMPTS), "{sent} transmits");
    }

    #[test]
    fn a_resize_keeps_the_image_id_so_the_terminal_replaces_the_image() {
        let mut loaded = loaded();
        let (_, first, _) = loaded
            .kitty_placement(FONT, Rect::new(0, 0, 20, 8), false)
            .expect("placement");
        let (_, second, _) = loaded
            .kitty_placement(FONT, Rect::new(0, 0, 40, 8), false)
            .expect("placement");
        assert_eq!(first, second, "a resize must replace, not accumulate");
        assert_eq!(first & 0xFF00_0000, 0x0100_0000);
    }

    #[test]
    fn going_off_screen_keeps_the_protocol_of_the_renderers_that_redraw_it() {
        let picker = Picker::from_fontsize(FONT);
        let mut loaded = loaded();
        let _ = loaded.protocol_mut(&picker);

        // Every protocol but kitty re-emits its pixels each frame, so leaving
        // the viewport must not cost them a re-encode on the way back.
        loaded.release();
        assert!(loaded.protocol.is_some());

        loaded.release_all();
        assert!(loaded.protocol.is_none());
    }

    #[test]
    fn two_processes_do_not_start_from_the_same_image_id() {
        // Ids are terminal-global: two processes sharing a terminal must not
        // both claim the same one, or each replaces the other's pixels.
        // Neighbouring pids are the case that matters — panes started together
        // get them — and each needs room for a session's worth of ids.
        let session = 4096;
        for pid in [1u32, 2, 999, 1000, 54_320, 54_321] {
            let base = kitty_id_base(pid);
            assert_eq!(base & 0xFF00_0000, 0, "pid {pid} leaves the 24 bit range");
            assert!(
                base.abs_diff(kitty_id_base(pid + 1)) > session,
                "pid {pid} and {} start {} ids apart",
                pid + 1,
                base.abs_diff(kitty_id_base(pid + 1))
            );
        }
    }
}

#[cfg(test)]
mod decode_tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::DynamicImage::new_rgb8(width, height);
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    #[test]
    fn decode_retains_small_images_and_reduces_large_ones() {
        let small = decode_image(&png(16, 9)).unwrap();
        assert_eq!((small.width(), small.height()), (16, 9));
        let large = decode_image(&png(2560, 1440)).unwrap();
        assert_eq!((large.width(), large.height()), (1280, 720));
        let portrait = decode_image(&png(900, 3600)).unwrap();
        assert_eq!((portrait.width(), portrait.height()), (320, 1280));
        let thin = decode_image(&png(16000, 3)).unwrap();
        assert_eq!((thin.width(), thin.height()), (1280, 1));
    }

    #[test]
    fn decode_rejects_invalid_data_and_oversize_bodies() {
        assert!(decode_image(b"<html>not an image</html>").is_err());
        let error = decode_image(&vec![0; MAX_IMAGE_BYTES + 1]).unwrap_err();
        assert!(error.to_string().contains("too large"));
    }

    #[test]
    fn decode_svg_detects_xml_and_preserves_transparency() {
        let svg = br##"<?xml version="1.0"?>
            <!-- badge without a filename or content type -->
            <svg xmlns="http://www.w3.org/2000/svg" width="80" height="20">
                <rect width="40" height="20" fill="#ff0000" fill-opacity="0.5"/>
            </svg>"##;
        let image = decode_image(svg).unwrap().to_rgba8();
        assert_eq!(image.dimensions(), (80, 20));
        let pixel = image.get_pixel(10, 10).0;
        assert_eq!(&pixel[..3], &[255, 0, 0]);
        assert!((127..=128).contains(&pixel[3]));
        assert_eq!(image.get_pixel(60, 10).0, [0, 0, 0, 0]);
    }

    #[test]
    fn decode_svg_limits_dimensions_before_rasterizing() {
        for (width, height, expected) in [
            (100_000, 50_000, (1280, 640)),
            (50_000, 100_000, (640, 1280)),
            (100_000, 1, (1280, 1)),
        ] {
            let svg = format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}">
                    <rect width="100%" height="100%" fill="red"/>
                </svg>"#
            );
            let image = decode_image(svg.as_bytes()).unwrap();
            assert_eq!((image.width(), image.height()), expected);
        }
    }

    #[test]
    fn decode_svg_uses_viewbox_dimensions_and_accepts_utf8_bom() {
        let svg = "\u{feff}<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 30 10\"><path d=\"M0 0h30v10H0z\" fill=\"blue\"/></svg>";
        let image = decode_image(svg.as_bytes()).unwrap().to_rgba8();
        assert_eq!(image.dimensions(), (30, 10));
        assert_eq!(image.get_pixel(15, 5).0, [0, 0, 255, 255]);
    }

    #[test]
    fn decode_svg_rejects_invalid_xml_non_svg_and_empty_dimensions() {
        for svg in [
            "<svg",
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="0" height="20"/>"#,
            r#"<html><svg xmlns="http://www.w3.org/2000/svg" width="20" height="20"/></html>"#,
        ] {
            assert!(decode_image(svg.as_bytes()).is_err(), "{svg}");
        }
    }
}
