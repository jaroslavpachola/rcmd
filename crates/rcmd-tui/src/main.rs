mod app;
mod config;
mod git;
mod keymap;
mod state;
mod subshell;
mod ui;

use std::path::PathBuf;

use anyhow::{Context, Result};

struct Args {
    printwd: Option<PathBuf>,
    dirs: Vec<PathBuf>,
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let (cfg, load_warning) = config::load();
    let theme_warning = ui::init_theme(&cfg.theme);
    let mouse = cfg.mouse;
    let mut terminal = ratatui::init();
    if mouse {
        app::set_mouse_capture(true);
    }
    let result = run(&args, cfg, [load_warning, theme_warning], &mut terminal);
    // Unconditional: the options form can turn the mouse on mid-session
    // (disabling an inactive capture is a harmless escape sequence).
    app::set_mouse_capture(false);
    ratatui::restore();
    result
}

fn run(
    args: &Args,
    cfg: config::Config,
    warnings: [Option<String>; 2],
    terminal: &mut ratatui::DefaultTerminal,
) -> Result<()> {
    let warnings = warnings.into_iter().flatten().collect();
    let mut app = app::App::new(&args.dirs, cfg, warnings)?;
    let result = app.run(terminal);

    // Persist the panel state for the next session — onto the *on-disk*
    // state file, not this instance's copy: options-form and hotlist
    // changes are written through when they happen, and another instance
    // may have saved its own since we started.
    let panel = &app.panels[app.active];
    let (show_hidden, sort_reverse) = (panel.show_hidden, panel.sort_reverse);
    let sort_key = config::sort_key_name(panel.sort_key).to_string();
    let listing = config::list_mode_name(panel.list_mode).to_string();
    if let Err(err) = state::update(|s| {
        s.show_hidden = Some(show_hidden);
        s.sort_key = Some(sort_key);
        s.sort_reverse = Some(sort_reverse);
        s.listing = Some(listing);
    }) {
        eprintln!("rcmd: could not save state: {err}");
    }

    if let Some(path) = &args.printwd {
        let cwd = &app.panels[app.active].cwd;
        let _ = std::fs::write(path, cwd.as_os_str().as_encoded_bytes());
    }
    result
}

fn parse_args() -> Result<Args> {
    let mut printwd = None;
    let mut dirs = Vec::new();
    let mut it = std::env::args_os().skip(1);
    while let Some(arg) = it.next() {
        match arg.to_str() {
            Some("-P") | Some("--printwd") => {
                let path = it.next().context("-P requires a file argument")?;
                printwd = Some(PathBuf::from(path));
            }
            Some("-h") | Some("--help") => {
                println!(
                    "usage: rcmd [-P FILE] [DIR1 [DIR2]]\n\n\
                     -P FILE   write the last active directory to FILE on exit\n\
                     DIR1/DIR2 starting directories for the left/right panel"
                );
                std::process::exit(0);
            }
            Some("-V") | Some("--version") => {
                println!("rcmd {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            Some(flag) if flag.starts_with('-') => anyhow::bail!("unknown option: {flag}"),
            _ => dirs.push(PathBuf::from(arg)),
        }
    }
    Ok(Args { printwd, dirs })
}
