use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, ensure};
use image::{DynamicImage, RgbaImage};
use resvg::{tiny_skia, usvg};

use super::MAX_IMAGE_EDGE;

pub(super) fn decode(bytes: &[u8]) -> Result<DynamicImage> {
    let text = std::str::from_utf8(bytes).context("SVG is not UTF-8")?;
    let document = usvg::roxmltree::Document::parse(text).context("Invalid SVG XML")?;
    ensure!(
        document
            .root_element()
            .has_tag_name(("http://www.w3.org/2000/svg", "svg")),
        "Image is not an SVG document"
    );

    let options = usvg::Options {
        font_family: "sans-serif".to_owned(),
        fontdb: system_fonts(),
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_string: Box::new(|_, _| None),
            ..Default::default()
        },
        ..Default::default()
    };
    rasterize(&document, &options)
}

fn system_fonts() -> Arc<usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    Arc::clone(FONTS.get_or_init(|| {
        let mut fonts = usvg::fontdb::Database::new();
        fonts.load_system_fonts();
        Arc::new(fonts)
    }))
}

fn rasterize(
    document: &usvg::roxmltree::Document<'_>,
    options: &usvg::Options<'_>,
) -> Result<DynamicImage> {
    let tree = usvg::Tree::from_xmltree(document, options).context("Invalid SVG image")?;
    let size = tree.size();
    let scale = (MAX_IMAGE_EDGE as f32 / size.width().max(size.height())).min(1.0);
    let width = (size.width() * scale).round().max(1.0) as u32;
    let height = (size.height() * scale).round().max(1.0) as u32;
    let mut pixmap = tiny_skia::Pixmap::new(width, height).context("Cannot allocate SVG image")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let image = RgbaImage::from_raw(width, height, pixmap.take_demultiplied())
        .context("Invalid SVG pixel buffer")?;
    Ok(DynamicImage::ImageRgba8(image))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_text_renders_with_a_supplied_font() {
        let text = r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="30">
            <text x="2" y="22" font-family="Tuffy" font-size="20" fill="white">CI pass</text>
        </svg>"#;
        let document = usvg::roxmltree::Document::parse(text).unwrap();
        let mut options = usvg::Options::default();
        options
            .fontdb_mut()
            .load_font_data(include_bytes!("../../tests/fixtures/fonts/Tuffy.ttf").to_vec());
        let image = rasterize(&document, &options).unwrap().to_rgba8();
        assert_eq!(image.dimensions(), (100, 30));
        let visible = image.pixels().filter(|pixel| pixel.0[3] > 0).count();
        assert!(visible > 100, "text must produce visible glyphs: {visible}");
        assert_eq!(image.get_pixel(99, 29).0, [0, 0, 0, 0]);
    }

    #[test]
    fn svg_external_images_are_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("external.png");
        RgbaImage::from_pixel(10, 10, image::Rgba([255, 0, 0, 255]))
            .save(&path)
            .unwrap();
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
                <image href="{}" width="10" height="10"/>
            </svg>"#,
            path.display()
        );
        let image = decode(svg.as_bytes()).unwrap().to_rgba8();
        assert!(image.pixels().all(|pixel| pixel.0[3] == 0));
    }
}
