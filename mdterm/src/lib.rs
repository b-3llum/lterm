//! mdterm: render Markdown and images in the terminal.
//! Used by the `mdterm` binary and embedded in `lterm` as `lterm md`.

mod graphics;
mod render;
mod style;
mod wrap;

use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use graphics::{ImageOpts, Protocol, Rendered};
use render::Config;

const HELP: &str = "\
mdterm - render Markdown and images in the terminal

USAGE:
    mdterm [OPTIONS] [FILE]...

    FILE is a Markdown document or an image (png, jpeg, gif, webp, bmp).
    With no FILE, or when FILE is -, read standard input.

OPTIONS:
    -w, --width <N>         Text width in columns [default: terminal width, max 100]
    -i, --images <MODE>     auto, kitty, iterm, sixel, blocks, ascii, none [default: auto]
        --color <WHEN>      auto, always, never [default: auto]
        --max-height <N>    Maximum image height in rows [default: terminal height]
        --cell-size <WxH>   Terminal cell size in pixels, used to size images [default: 10x20]
        --no-urls           Don't print link targets after link text
    -h, --help              Print help
    -V, --version           Print version

ENVIRONMENT:
    MDTERM_IMAGES           Default for --images
    NO_COLOR                Disable color (and use ascii images) when set

EXAMPLES:
    mdterm README.md
    mdterm photo.jpg
    curl -s https://example.com/notes.md | mdterm
    mdterm -i sixel docs/guide.md      # Windows Terminal 1.22+
    mdterm --color always README.md | less -R
";

struct Args {
    files: Vec<String>,
    width: Option<usize>,
    images: Option<Protocol>,
    color: Option<bool>,
    max_height: Option<usize>,
    cell: (u32, u32),
    show_urls: bool,
}

/// Print, ignoring errors such as a closed pipe (`mdterm --help | head`).
fn say(text: &str) {
    let mut out = io::stdout().lock();
    let _ = out.write_all(text.as_bytes()).and_then(|_| out.flush());
}

/// Parse options. `Ok(None)` means help/version was printed and we should exit.
fn parse_args(prog: &str, args: Vec<String>) -> Result<Option<Args>, String> {
    let mut a = Args {
        files: Vec::new(),
        width: None,
        images: None,
        color: None,
        max_height: None,
        cell: (10, 20),
        show_urls: true,
    };
    if let Ok(v) = std::env::var("MDTERM_IMAGES") {
        a.images = Protocol::parse(&v).map_err(|e| format!("MDTERM_IMAGES: {e}"))?;
    }
    let mut it = args.into_iter();
    let mut only_files = false;
    while let Some(arg) = it.next() {
        if only_files || arg == "-" || !arg.starts_with('-') {
            a.files.push(arg);
            continue;
        }
        let (key, inline) = match arg.split_once('=') {
            Some((k, v)) if arg.starts_with("--") => (k.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = || inline.clone().or_else(|| it.next()).ok_or(format!("{key} needs a value"));
        let num = |v: String| v.parse::<usize>().map_err(|_| format!("invalid number '{v}'"));
        match key.as_str() {
            "-h" | "--help" => {
                say(&HELP.replace("mdterm", prog));
                return Ok(None);
            }
            "-V" | "--version" => {
                say(&format!("{prog} {}\n", env!("CARGO_PKG_VERSION")));
                return Ok(None);
            }
            "--" => only_files = true,
            "-w" | "--width" => a.width = Some(num(value()?)?),
            "--max-height" => a.max_height = Some(num(value()?)?),
            "-i" | "--images" => a.images = Protocol::parse(&value()?)?,
            "--color" => {
                a.color = match value()?.as_str() {
                    "auto" => None,
                    "always" => Some(true),
                    "never" => Some(false),
                    v => return Err(format!("invalid --color '{v}'")),
                }
            }
            "--cell-size" => {
                let v = value()?;
                let parsed = v
                    .split_once(['x', 'X'])
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                    .filter(|&(w, h): &(u32, u32)| w > 0 && h > 0);
                a.cell = parsed.ok_or(format!("invalid --cell-size '{v}', expected e.g. 10x20"))?;
            }
            "--no-urls" => a.show_urls = false,
            _ => return Err(format!("unknown option '{arg}'")),
        }
    }
    Ok(Some(a))
}

/// Run mdterm with `args` (excluding the program name). `prog` is the name used in
/// messages, e.g. "mdterm" or "lterm md".
pub fn run(prog: &str, args: Vec<String>) -> ExitCode {
    #[cfg(windows)]
    let _ = enable_ansi_support::enable_ansi_support();

    let mut args = match parse_args(prog, args) {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{prog}: {e}\nTry '{prog} --help'.");
            return ExitCode::from(2);
        }
    };

    let tty = io::stdout().is_terminal();
    let color = args.color.unwrap_or(tty && std::env::var_os("NO_COLOR").is_none());
    let (term_w, term_h) = terminal_size::terminal_size()
        .map(|(w, h)| (w.0 as usize, h.0 as usize))
        .unwrap_or((80, 24));
    let protocol = args.images.unwrap_or(if !tty && args.color != Some(true) {
        Protocol::None
    } else if !color {
        Protocol::Ascii
    } else {
        Protocol::detect()
    });
    let cfg = Config {
        width: args.width.unwrap_or(term_w.min(100)).max(20),
        color,
        show_urls: args.show_urls,
        image: ImageOpts {
            protocol,
            max_rows: args.max_height.unwrap_or(term_h.saturating_sub(2).max(8)),
            cell_w: args.cell.0,
            cell_h: args.cell.1,
        },
    };

    if args.files.is_empty() {
        if io::stdin().is_terminal() {
            eprint!("{}", HELP.replace("mdterm", prog));
            return ExitCode::from(2);
        }
        args.files.push("-".into());
    }

    let mut stdout = io::stdout().lock();
    let mut status = ExitCode::SUCCESS;
    for (i, file) in args.files.iter().enumerate() {
        let (bytes, base) = match read_input(file) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{prog}: {file}: {e}");
                status = ExitCode::FAILURE;
                continue;
            }
        };
        let out = render_input(&bytes, &base, file, &cfg, args.width.unwrap_or(term_w));
        let sep = if i + 1 < args.files.len() { "\n" } else { "" };
        if stdout.write_all(out.as_bytes()).and_then(|_| stdout.write_all(sep.as_bytes())).is_err() {
            break; // e.g. broken pipe from `| head`
        }
    }
    let _ = stdout.flush();
    status
}

fn read_input(file: &str) -> io::Result<(Vec<u8>, PathBuf)> {
    if file == "-" {
        let mut buf = Vec::new();
        io::stdin().lock().read_to_end(&mut buf)?;
        Ok((buf, std::env::current_dir().unwrap_or_default()))
    } else {
        let path = Path::new(file);
        let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
        Ok((std::fs::read(path)?, base))
    }
}

fn render_input(bytes: &[u8], base: &Path, name: &str, cfg: &Config, image_width: usize) -> String {
    // A file whose magic bytes decode as an image is drawn directly.
    if image::guess_format(bytes).is_ok() {
        if let Ok(img) = graphics::decode(bytes) {
            if cfg.image.protocol == Protocol::None {
                return format!("[image: {name} {}x{}]\n", img.width(), img.height());
            }
            return match graphics::render(&img, image_width.max(1), &cfg.image) {
                Rendered::Rows(rows) => rows.iter().map(|r| format!("{r}\n")).collect(),
                Rendered::Raw(seq) => format!("{seq}\n"),
            };
        }
    }
    render::render(&String::from_utf8_lossy(bytes), base, cfg)
}
