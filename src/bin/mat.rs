#[path = "support/assets.rs"]
mod assets;

use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use mat::{
    ContentBlock, RenderOptions, Renderer,
    terminal::{sanitize_text, write_line},
    terminal_image::{TerminalImageOptions, TerminalImageProtocol, write_image},
};
use ratatui::{
    style::{Color, Style},
    text::Line,
};

#[derive(Debug, Parser)]
#[command(version, about = "Render Markdown and images in the terminal")]
struct Args {
    /// Markdown file to read; omit or use - for standard input.
    #[arg(value_name = "FILE")]
    file: Option<PathBuf>,

    /// Output width in terminal cells (default: terminal width up to 160, or 80).
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    width: Option<u16>,

    /// Use ANSI text colors; auto respects NO_COLOR and redirected output.
    #[arg(long, value_enum, default_value_t = ColorMode::Auto)]
    color: ColorMode,

    /// Show images on terminals, or always use their text descriptions.
    #[arg(long, value_enum, default_value_t = ImageMode::Auto)]
    images: ImageMode,

    /// Load images synchronously for stable terminal ordering, or asynchronously for faster text.
    #[arg(long, value_enum, default_value_t = ImageLoading::Sync)]
    image_loading: ImageLoading,

    /// Report image failures after printing the document.
    #[arg(short, long)]
    verbose: bool,

    /// Resolve relative images from this directory instead of the file's directory.
    #[arg(long, value_name = "PATH")]
    base_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ColorMode {
    Auto,
    Always,
    Never,
}

impl ColorMode {
    fn enabled(self, tty: bool, no_color: bool) -> bool {
        match self {
            Self::Auto => tty && !no_color,
            Self::Always => true,
            Self::Never => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ImageMode {
    Auto,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ImageLoading {
    Sync,
    Async,
}

type ImageTaskResult = (String, String, anyhow::Result<Vec<u8>>);

fn main() -> std::process::ExitCode {
    match run(Args::parse()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) if is_broken_pipe(&error) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("mat: {}", sanitize_text(&format!("{error:#}")));
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<()> {
    let cwd = std::env::current_dir().context("cannot determine current directory")?;
    let base_dir = image_base_dir(&args, &cwd);
    let input = sanitize_text(&read_input(args.file.as_deref())?);
    let stdout = io::stdout();
    let tty = stdout.is_terminal();
    let width = output_width(args.width, tty);
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
    let color = args.color.enabled(tty, no_color);
    let image_options = if args.images == ImageMode::Auto && tty {
        terminal_image_options(width)
    } else {
        None
    };
    let theme = mat::syntax::default_theme();
    let syntax_set = mat::syntax::syntax_set();
    let options = RenderOptions {
        width: usize::from(width),
        syntax_ctx: Some((syntax_set, &theme)),
        ..RenderOptions::default()
    };
    let blocks = Renderer::default().layout_blocks(&input, &options);
    let mut output = io::BufWriter::new(stdout.lock());
    let mut loader = assets::ImageLoader::new(base_dir.clone());
    let mut image_tasks: Vec<JoinHandle<ImageTaskResult>> = Vec::new();
    let mut image_diagnostics = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(line) => write_line(&mut output, &line, color)?,
            ContentBlock::Image { url, alt } => {
                if let Some(image_options) = image_options {
                    if args.image_loading == ImageLoading::Sync {
                        let rendered = loader.load(&url).and_then(|image| {
                            let mut encoded = Vec::new();
                            write_image(&mut encoded, &image, image_options)?;
                            Ok(encoded)
                        });
                        match rendered {
                            Ok(encoded) => output.write_all(&encoded)?,
                            Err(error) => {
                                write_fallback(
                                    &mut output,
                                    media_label("image", &url, &alt),
                                    width,
                                    color,
                                )?;
                                if args.verbose {
                                    image_diagnostics.push(format!(
                                        "mat: image {}: {}",
                                        sanitize_text(&url),
                                        sanitize_text(&format!("{error:#}"))
                                    ));
                                }
                            }
                        }
                        continue;
                    }
                    let task_url = url.clone();
                    let task_alt = alt.clone();
                    let task_base_dir = base_dir.clone();
                    image_tasks.push(std::thread::spawn(move || {
                        let rendered = assets::ImageLoader::new(task_base_dir)
                            .load(&task_url)
                            .and_then(|image| {
                                let mut encoded = Vec::new();
                                write_image(&mut encoded, &image, image_options)?;
                                Ok(encoded)
                            });
                        (task_url, task_alt, rendered)
                    }));
                    continue;
                }
                write_fallback(&mut output, media_label("image", &url, &alt), width, color)?;
            }
            ContentBlock::Video { url, alt } => {
                write_fallback(&mut output, media_label("video", &url, &alt), width, color)?;
            }
            ContentBlock::CodeSnippet {
                url,
                path,
                start_line,
                end_line,
            } => {
                let range = end_line.map_or_else(
                    || start_line.to_string(),
                    |end| format!("{start_line}-{end}"),
                );
                write_fallback(&mut output, format!("{path}:{range} {url}"), width, color)?;
            }
            ContentBlock::Suggestion { lines } => {
                for line in lines {
                    let line = Line::styled(format!("+ {line}"), Style::default().fg(Color::Green));
                    for line in mat::wrap_line(line, usize::from(width)) {
                        write_line(&mut output, &line, color)?;
                    }
                }
            }
        }
    }
    output.flush().context("cannot write rendered Markdown")?;
    for task in image_tasks {
        let (url, alt, rendered) = task
            .join()
            .map_err(|_| anyhow::anyhow!("image worker panicked"))?;
        match rendered {
            Ok(encoded) => output.write_all(&encoded)?,
            Err(error) => {
                write_fallback(&mut output, media_label("image", &url, &alt), width, color)?;
                if args.verbose {
                    image_diagnostics.push(format!(
                        "mat: image {}: {}",
                        sanitize_text(&url),
                        sanitize_text(&format!("{error:#}"))
                    ));
                }
            }
        }
    }
    output.flush().context("cannot write rendered images")?;
    for diagnostic in image_diagnostics {
        eprintln!("{diagnostic}");
    }
    Ok(())
}

fn read_input(path: Option<&Path>) -> Result<String> {
    if let Some(path) = path.filter(|path| *path != Path::new("-")) {
        std::fs::read_to_string(path)
            .with_context(|| format!("cannot read Markdown file {}", path.display()))
    } else {
        let mut input = String::new();
        io::stdin()
            .read_to_string(&mut input)
            .context("cannot read Markdown from standard input")?;
        Ok(input)
    }
}

fn image_base_dir(args: &Args, cwd: &Path) -> PathBuf {
    let path = args.base_dir.as_deref().or_else(|| {
        args.file
            .as_deref()
            .filter(|path| *path != Path::new("-"))
            .and_then(Path::parent)
    });
    path.map_or_else(|| cwd.to_owned(), |path| cwd.join(path))
}

fn output_width(width: Option<u16>, tty: bool) -> u16 {
    width.unwrap_or_else(|| {
        if tty {
            detected_width(crossterm::terminal::size().ok())
        } else {
            80
        }
    })
}

fn detected_width(size: Option<(u16, u16)>) -> u16 {
    size.map(|(width, _)| width)
        .filter(|width| *width > 0)
        .map_or(80, |width| width.min(160))
}

fn terminal_image_options(width: u16) -> Option<TerminalImageOptions> {
    let term = std::env::var("TERM").unwrap_or_default();
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let tmux = term.starts_with("tmux") || term_program == "tmux" || env_nonempty("TMUX");
    let protocol = image_protocol(
        &term,
        &term_program,
        env_nonempty("KITTY_WINDOW_ID") || env_nonempty("GHOSTTY_RESOURCES_DIR"),
        env_nonempty("ITERM_SESSION_ID"),
    )?;
    let protocol = if tmux && protocol == TerminalImageProtocol::Iterm2 {
        TerminalImageProtocol::Halfblocks
    } else {
        protocol
    };
    let font_size = crossterm::terminal::window_size()
        .ok()
        .and_then(|size| {
            if size.columns == 0 || size.rows == 0 {
                return None;
            }
            let dimensions = (size.width / size.columns, size.height / size.rows);
            (dimensions.0 > 0 && dimensions.1 > 0).then_some(dimensions)
        })
        .unwrap_or((8, 16));
    Some(TerminalImageOptions {
        protocol,
        font_size,
        max_cells: (width, mat::IMAGE_HEIGHT),
        tmux,
    })
}

fn image_protocol(
    term: &str,
    term_program: &str,
    kitty_hint: bool,
    iterm_hint: bool,
) -> Option<TerminalImageProtocol> {
    if term == "dumb" {
        None
    } else if term == "xterm-kitty" || term_program == "ghostty" || kitty_hint {
        Some(TerminalImageProtocol::Kitty)
    } else if term_program == "iTerm.app" || term_program == "WezTerm" || iterm_hint {
        Some(TerminalImageProtocol::Iterm2)
    } else {
        Some(TerminalImageProtocol::Halfblocks)
    }
}

fn env_nonempty(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn media_label(kind: &str, source: &str, alt: &str) -> String {
    if alt.is_empty() {
        format!("[{kind}] {source}")
    } else {
        format!("[{kind}: {alt}] {source}")
    }
}

fn write_fallback(
    output: &mut impl Write,
    text: String,
    width: u16,
    color: bool,
) -> io::Result<()> {
    let line = Line::styled(text, Style::default().fg(Color::DarkGray));
    for line in mat::wrap_line(line, usize::from(width)) {
        write_line(output, &line, color)?;
    }
    Ok(())
}

fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_selection_respects_tty_and_explicit_overrides() {
        assert!(ColorMode::Auto.enabled(true, false));
        assert!(!ColorMode::Auto.enabled(false, false));
        assert!(!ColorMode::Auto.enabled(true, true));
        assert!(ColorMode::Always.enabled(false, true));
        assert!(!ColorMode::Never.enabled(true, false));
    }

    #[test]
    fn automatic_width_defaults_when_terminal_size_is_missing_or_zero() {
        assert_eq!(detected_width(None), 80);
        assert_eq!(detected_width(Some((0, 0))), 80);
        assert_eq!(detected_width(Some((100, 30))), 100);
        assert_eq!(detected_width(Some((240, 60))), 160);
    }

    #[test]
    fn image_base_directory_uses_file_parent_and_explicit_override() {
        let cwd = Path::new("/tmp/project");
        let file_args = Args::parse_from(["mat", "docs/readme.md"]);
        assert_eq!(
            image_base_dir(&file_args, cwd),
            Path::new("/tmp/project/docs")
        );
        let stdin_args = Args::parse_from(["mat", "-"]);
        assert_eq!(image_base_dir(&stdin_args, cwd), cwd);
        let override_args = Args::parse_from(["mat", "--base-dir", "assets", "docs/readme.md"]);
        assert_eq!(
            image_base_dir(&override_args, cwd),
            Path::new("/tmp/project/assets")
        );
    }

    #[test]
    fn protocol_selection_needs_no_input_query() {
        assert_eq!(
            image_protocol("xterm-kitty", "", false, false),
            Some(TerminalImageProtocol::Kitty)
        );
        assert_eq!(
            image_protocol("xterm-256color", "ghostty", false, false),
            Some(TerminalImageProtocol::Kitty)
        );
        assert_eq!(
            image_protocol("tmux-256color", "tmux", false, true),
            Some(TerminalImageProtocol::Iterm2)
        );
        assert_eq!(
            image_protocol("xterm-256color", "", false, false),
            Some(TerminalImageProtocol::Halfblocks)
        );
        assert_eq!(image_protocol("dumb", "", true, true), None);
    }
}
