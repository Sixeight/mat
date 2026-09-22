# mat
<img width="600" height="600" alt="mat logo" src="https://github.com/user-attachments/assets/bc726b0c-be80-4057-aeb7-e3990cc4294e" />

<img width="600" alt="demo" src="https://github.com/user-attachments/assets/f7f4bd3e-f844-4191-8ef5-8ae3725f6cc6" />

`mat` (Markdown cat) renders Markdown in your terminal.

```sh
cargo install --path . --locked
mat README.md
cat README.md | mat
mat README.md --color always --images never | less -R
```

Output goes to the normal screen, so the document stays in scrollback.
Supported: headings, lists, task lists, quotes, GitHub alerts, tables,
highlighted code blocks, emoji shortcodes, and Mermaid diagrams
(via [ma](https://github.com/Sixeight/ma)).
Tables too wide for the screen switch to a vertical layout.

## CLI

| Argument | Behavior |
| --- | --- |
| `[FILE]` | UTF-8 Markdown file. Omit or use `-` for stdin. |
| `--width N` | Width in cells. Default: terminal width (max 160), or 80 when redirected. |
| `--color auto\|always\|never` | `auto` disables color when redirected or `NO_COLOR` is set. |
| `--images auto\|never` | `auto` shows images only when stdout is a terminal. |
| `--image-loading sync\|async` | `async` prints text before images finish loading. Default: `sync`. |
| `-v`, `--verbose` | Report image failures on stderr. |
| `--base-dir PATH` | Base for relative image paths. Default: the file's directory, or the working directory for stdin. |

### Images

- Sources: local files and public HTTP(S) URLs, including SVG and badges.
- Limits: 16 MiB per image, 1280 px max edge, 10 s HTTP timeout.
- A failed image falls back to a text description; the rest still prints.
- Protocols: Kitty graphics, iTerm2 inline images, or colored half-blocks
  as a fallback. `TERM=dumb` uses descriptions.
- tmux: Kitty needs `allow-passthrough on`; iTerm2 falls back to half-blocks.

To test image output, run `cargo run --example image_stdout`
(append `-- halfblocks` or `-- iterm2` to force a protocol).

Videos and GitHub code snippets are shown as links.

## Library

Disable the CLI feature:

```toml
mat = { path = "../mat", default-features = false }
```

```rust
use mat::{RenderOptions, Renderer};

let theme = mat::syntax::default_theme();
let options = RenderOptions {
    width: 80,
    syntax_ctx: Some((mat::syntax::syntax_set(), &theme)),
    ..RenderOptions::default()
};
let mut renderer = Renderer::default();
let lines = renderer.layout_lines("# Hello\n\n**Markdown**", &options);
```

- `layout_lines` / `layout_blocks` wrap text to the width.
  `render_lines` / `render_blocks` don't (tables and Mermaid still fit the width).
- `ContentBlock` keeps image, video, and snippet metadata for the caller.
- `line_count` is exact; `estimate_line_count` is cheap.
- `image::LoadedImage` works with ratatui; `terminal_image::write_image`
  and `terminal::write_line` write to stdout-like streams.

The caller handles caching, media fetching, and authentication.

## Development

```sh
cargo test --locked
cargo test --locked --no-default-features
cargo fmt -- --check
cargo clippy --all-targets
python3 tests/pty_smoke.py --binary target/debug/mat
```

The `test-support` feature exposes image-state inspection for host app tests.
