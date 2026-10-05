# mdterm

A small (~1.5 MB), dependency-light terminal renderer for Markdown **and images**.
Works on Linux, macOS and Windows, over SSH, on servers and in TTYs.
It doesn't need a GUI.

```
mdterm README.md          # render a document
mdterm photo.jpg          # draw an image
cat notes.md | mdterm     # read stdin
```

## Features

- CommonMark + GFM: headings, emphasis, strikethrough, links, lists, task lists,
  tables (aligned, wrapped to fit), block quotes, GitHub alerts (`> [!NOTE]`),
  footnotes, fenced code blocks, rules. YAML front matter is hidden.
- Word wrapping that understands wide (CJK) characters.
- Images from Markdown `![]()` and HTML `<img src>`, relative to the document,
  plus `data:` URIs. Optional http(s) fetching with `--features remote`.
- Image output modes:

  | mode     | works in                                                      |
  |----------|---------------------------------------------------------------|
  | `kitty`  | kitty, Ghostty, WezTerm, lterm (full resolution)              |
  | `iterm`  | iTerm2, WezTerm, mintty (full resolution)                     |
  | `sixel`  | Windows Terminal 1.22+, foot, mlterm, Konsole, xterm (vt340)  |
  | `blocks` | any 24-bit color terminal, tmux, SSH (half-block pixels)      |
  | `ascii`  | anything at all, including `TERM=dumb`                        |
  | `none`   | alt text only                                                 |

  `auto` (the default) picks one from the environment. It falls back to `blocks`,
  and to plain alt text when output is piped.
- Safe on untrusted files: control characters in the document are replaced, so a
  Markdown file can't inject terminal escape sequences.

## Options

```
-w, --width <N>         Text width [default: terminal width, max 100]
-i, --images <MODE>     auto|kitty|iterm|sixel|blocks|ascii|none
    --color <WHEN>      auto|always|never
    --max-height <N>    Max image height in rows [default: terminal height]
    --cell-size <WxH>   Cell size in pixels for sizing images [default: 10x20]
    --no-urls           Hide link targets
```

`MDTERM_IMAGES=sixel` sets the default mode. `NO_COLOR` is respected.

## Build

```
cargo build --release                       # target/release/mdterm
cargo build --release --features remote     # + http(s) images
cargo build --release --target x86_64-pc-windows-gnu   # Windows from Linux (needs mingw-w64)
cargo test
```

Supported image formats: PNG, JPEG, GIF, WebP and BMP. SVG isn't supported (it shows as alt text).
