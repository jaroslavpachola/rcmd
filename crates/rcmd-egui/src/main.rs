//! `rcmd-egui` - rcmd in a window.
//!
//! The same file manager as the `rcmd` binary, drawn by egui instead of
//! by a terminal. It is a front end and nothing more: the panels, the
//! viewer, the editor, the archive and FTP/SFTP handling, the keymap,
//! the themes and `config.toml` are all `rcmd_tui`'s, shared with the
//! terminal build rather than reimplemented. The window contributes a
//! ratatui backend that paints cells (`grid`), a translation from egui
//! input to the crossterm events the state machine already dispatches
//! on (`keys`), and an answer to "run this command" that does not
//! assume a tty (`exec`).

mod exec;
mod font;
mod grid;
mod gui;
mod keys;
mod menu;
mod term;

use std::path::PathBuf;

use anyhow::{Context, Result};
use ratatui::crossterm::event::KeyEvent;
use rcmd_tui::app::{App, Startup};
use rcmd_tui::{config, ui};

/// Big enough to read on a HiDPI screen, small enough for two panels.
const DEFAULT_FONT_SIZE: f32 = 14.0;

struct Args {
    dirs: Vec<PathBuf>,
    startup: Startup,
    theme: Option<String>,
    colors: Option<String>,
    font_size: f32,
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let (mut cfg, load_warning) = config::load();
    let mut warnings: Vec<String> = load_warning.into_iter().collect();

    // The subshell stays on: `term.rs` is the half of a terminal that
    // reads what it writes, so Ctrl+O has a screen to show after all.
    // Without one (`subshell = false`, or a shell that would not
    // spawn), `exec.rs` falls back to a terminal emulator.
    //
    // egui delivers pointer events regardless; the config flag is about
    // asking a terminal to report them, which is not a question here
    cfg.mouse = true;

    let theme = args.theme.clone().unwrap_or_else(|| cfg.theme.clone());
    warnings.extend(ui::init_theme(&theme));
    if let Some(spec) = &args.colors {
        warnings.extend(ui::set_color_spec(spec));
    }
    ui::set_tab_size(cfg.edit_tab_size as usize);
    if let Some(path) = config::config_path()
        && let Some(dir) = path.parent()
    {
        rcmd_edit::set_user_syntax_dir(dir.join("syntax"));
    }

    // $RCMD_EGUI_KEYS: keys played in at startup, spelled the way
    // `config.toml` spells them ("ctrl+o", "f5", "alt+H"), comma
    // separated. A window has no pty for a test harness to drive, so
    // this is how a screenshot reaches a screen that needs a keystroke.
    let startup_keys = parse_startup_keys(&mut warnings);

    let mut app = App::new(&args.dirs, cfg, warnings, &args.startup)?;
    // the pane's parser never answers a terminal query, and fish waits
    // for the DA1 reply before every prompt: the shim has to keep
    // answering while the pane is up, not only while the shell is hidden
    app.set_subshell_headless();
    // the menu bar is egui's, above the grid (menu.rs): the terminal
    // build's row and dropdown stay off, and F9 opens the real one
    app.set_external_menubar();
    app.open_startup(args.startup)?;

    let font_size = args.font_size;
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("rcmd-egui")
            .with_app_id("rcmd-egui")
            .with_inner_size(gui::window_size(font_size)),
        ..Default::default()
    };
    eframe::run_native(
        "rcmd-egui",
        options,
        Box::new(move |cc| Ok(Box::new(gui::Gui::new(cc, app, font_size, startup_keys)?))),
    )
    .map_err(|err| anyhow::anyhow!("{err}"))
}

/// `$RCMD_EGUI_KEYS` as key events, anything unparseable reported rather
/// than dropped in silence.
fn parse_startup_keys(warnings: &mut Vec<String>) -> Vec<KeyEvent> {
    let Ok(spec) = std::env::var("RCMD_EGUI_KEYS") else {
        return Vec::new();
    };
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| match rcmd_tui::keymap::parse_key(s) {
            Some((code, mods)) => Some(KeyEvent::new(code, mods)),
            None => {
                warnings.push(format!("RCMD_EGUI_KEYS: cannot parse {s:?}"));
                None
            }
        })
        .collect()
}

fn parse_args() -> Result<Args> {
    let mut it = std::env::args_os().skip(1);
    let mut dirs = Vec::new();
    let mut edit: Vec<PathBuf> = Vec::new();
    let mut view: Option<PathBuf> = None;
    let mut diff: Vec<PathBuf> = Vec::new();
    let mut theme = None;
    let mut colors = None;
    let mut font_size = DEFAULT_FONT_SIZE;

    while let Some(arg) = it.next() {
        let mut next = |flag: &str| -> Result<PathBuf> {
            it.next()
                .map(PathBuf::from)
                .with_context(|| format!("{flag} requires an argument"))
        };
        match arg.to_str() {
            Some("-e") | Some("--edit") => edit.push(next("-e")?),
            Some("-v") | Some("--view") => view = Some(next("-v")?),
            Some("-D") | Some("--diff") => {
                diff.push(next("-D")?);
                diff.push(next("-D")?);
            }
            Some("-S") | Some("--skin") => theme = Some(next("-S")?.to_string_lossy().into_owned()),
            Some("-b") | Some("--nocolor") => theme = Some("bw".to_string()),
            Some("-C") | Some("--colors") => {
                colors = Some(next("-C")?.to_string_lossy().into_owned())
            }
            Some("--font-size") => {
                font_size = next("--font-size")?
                    .to_string_lossy()
                    .parse()
                    .context("--font-size takes a number")?;
                if !(4.0..=96.0).contains(&font_size) {
                    anyhow::bail!("--font-size must be between 4 and 96");
                }
            }
            Some("-h") | Some("--help") => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            Some("-V") | Some("--version") => {
                println!("rcmd-egui {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            Some(flag) if flag.starts_with('-') && flag != "-" => {
                anyhow::bail!("unknown option: {flag}")
            }
            _ => dirs.push(PathBuf::from(arg)),
        }
    }

    let startup = if !diff.is_empty() {
        Startup::Diff(diff[0].clone(), diff[1].clone())
    } else if !edit.is_empty() {
        Startup::Edit(edit)
    } else if let Some(file) = view {
        Startup::View(file)
    } else {
        Startup::Panels
    };
    Ok(Args {
        dirs,
        startup,
        theme,
        colors,
        font_size,
    })
}

const USAGE: &str = "\
usage: rcmd-egui [OPTIONS] [DIR1 [DIR2]]

rcmd in a window: the same panels, keys, themes and config.toml as the
`rcmd` terminal binary, drawn by egui.

  -e, --edit FILE      start in the editor on FILE (repeatable)
  -v, --view FILE      start in the viewer on FILE
  -D, --diff A B       start in the diff viewer on A and B
  -S, --skin NAME      theme: mc, dark, bw
  -b, --nocolor        black and white
  -C, --colors SPEC    mc colour spec: keyword=fg,bg:keyword=fg,bg
      --font-size N    point size of the grid font (default 14)
  -V, --version        print the version
  -h, --help           this text

Environment:
  RCMD_EGUI_KEYS  keys played in at startup, spelled as config.toml
                  spells them and comma separated (\"ctrl+o\",
                  \"f5,down,enter\"). A window has no pty for a harness to
                  drive; this is how a screenshot reaches a screen that
                  needs a keystroke.
  RCMD_EGUI_FONT  a .ttf/.otf monospace face to draw the grid in;
                  without it a system DejaVu/Liberation/Menlo/Consolas
                  is looked for, and egui's bundled font is the
                  fallback.
  TERMINAL        the emulator to open for commands that want a tty (F2
                  user commands, the command line). Openers and [[open]]
                  rules are spawned detached and need none.

There is no persistent subshell in this build: a window has no tty to
hand to a child, so Ctrl+O has nothing to show. Commands run in a
terminal emulator instead.
";
