//! Markdown rendering shared by terminal applications and the `mat` CLI.
//!
//! ```
//! use mat::{RenderOptions, Renderer};
//!
//! let mut renderer = Renderer::default();
//! let options = RenderOptions { width: 80, ..RenderOptions::default() };
//! let lines = renderer.layout_lines("# Hello\n\n**Markdown** in a terminal.", &options);
//! assert!(!lines.is_empty());
//! ```
//!
//! [`Renderer::render_lines`] and [`Renderer::render_blocks`] leave ordinary
//! text unwrapped for TUI callers. The `layout_*` methods apply wrapping while
//! preserving table and diagram geometry. Image fetching is left to the caller.

pub mod image;
mod kitty;
mod layout;
mod render;
pub mod syntax;
pub mod terminal;
pub mod terminal_image;

pub use layout::wrap_line;
pub use render::*;

fn hash_str(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}
