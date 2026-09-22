use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use mat::image::{MAX_IMAGE_BYTES, decode_image};
use reqwest::{Url, blocking::Client};

pub struct ImageLoader {
    base_dir: PathBuf,
    client: Option<Client>,
}

enum Source {
    File(PathBuf),
    Http(Url),
}

impl ImageLoader {
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            client: None,
        }
    }

    pub fn load(&mut self, source: &str) -> Result<image::DynamicImage> {
        let bytes = match resolve_source(source, &self.base_dir)? {
            Source::File(path) => {
                let file = std::fs::File::open(&path)
                    .with_context(|| format!("cannot read image {}", path.display()))?;
                read_limited(file)?
            }
            Source::Http(url) => {
                if self.client.is_none() {
                    self.client = Some(
                        Client::builder()
                            .connect_timeout(Duration::from_secs(3))
                            .timeout(Duration::from_secs(10))
                            .redirect(reqwest::redirect::Policy::limited(5))
                            .user_agent(concat!("mat/", env!("CARGO_PKG_VERSION")))
                            .build()
                            .context("cannot initialize image HTTP client")?,
                    );
                }
                let response = self
                    .client
                    .as_ref()
                    .ok_or_else(|| anyhow!("image HTTP client unavailable"))?
                    .get(url)
                    .send()
                    .context("image request failed")?
                    .error_for_status()
                    .context("image server returned an error")?;
                if response
                    .content_length()
                    .is_some_and(|length| length > MAX_IMAGE_BYTES as u64)
                {
                    bail!(
                        "image exceeds the {} MiB limit",
                        MAX_IMAGE_BYTES / 1024 / 1024
                    );
                }
                read_limited(response)?
            }
        };
        decode_image(&bytes)
    }
}

fn resolve_source(source: &str, base_dir: &Path) -> Result<Source> {
    let url = match Url::parse(source) {
        Ok(url) => url,
        Err(_) => Url::from_directory_path(base_dir)
            .map_err(|_| anyhow!("invalid image base directory {}", base_dir.display()))?
            .join(source)
            .context("invalid image source")?,
    };
    match url.scheme() {
        "http" | "https" => Ok(Source::Http(url)),
        "file" => url
            .to_file_path()
            .map(Source::File)
            .map_err(|_| anyhow!("invalid local image path")),
        scheme => bail!("unsupported image source scheme: {scheme}"),
    }
}

fn read_limited(reader: impl Read) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .context("cannot read image data")?;
    if bytes.len() > MAX_IMAGE_BYTES {
        bail!(
            "image exceeds the {} MiB limit",
            MAX_IMAGE_BYTES / 1024 / 1024
        );
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_image_source_uses_base_directory_and_decodes_spaces() {
        let Source::File(path) =
            resolve_source("images/my%20image.png", Path::new("/tmp/docs")).expect("source")
        else {
            panic!("expected file")
        };
        assert_eq!(path, Path::new("/tmp/docs/images/my image.png"));
    }

    #[test]
    fn http_image_source_is_preserved() {
        let Source::Http(url) =
            resolve_source("https://example.com/image.png", Path::new("/tmp")).expect("source")
        else {
            panic!("expected HTTP")
        };
        assert_eq!(url.as_str(), "https://example.com/image.png");
    }

    #[test]
    fn unsupported_image_schemes_are_rejected() {
        assert!(resolve_source("ftp://example.com/image.png", Path::new("/tmp")).is_err());
    }

    #[test]
    fn reader_rejects_payload_larger_than_image_limit() {
        assert!(read_limited(std::io::repeat(0).take(MAX_IMAGE_BYTES as u64 + 1)).is_err());
        assert_eq!(
            read_limited(std::io::Cursor::new(b"image")).expect("read"),
            b"image"
        );
    }

    #[test]
    fn loader_decodes_relative_local_png() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = image::DynamicImage::new_rgb8(3, 2);
        source
            .save(dir.path().join("sample.png"))
            .expect("save PNG");
        let mut loader = ImageLoader::new(dir.path().to_owned());
        let decoded = loader.load("sample.png").expect("decode");
        assert_eq!((decoded.width(), decoded.height()), (3, 2));
    }
}
