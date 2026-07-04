mod app;
mod config;
mod keymap;
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
    let mut terminal = ratatui::init();
    let result = run(&args, cfg, [load_warning, theme_warning], &mut terminal);
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

    // Persist panel settings and the hotlist for the next session.
    let panel = &app.panels[app.active];
    app.config.show_hidden = panel.show_hidden;
    app.config.sort_key = config::sort_key_name(panel.sort_key).to_string();
    app.config.sort_reverse = panel.sort_reverse;
    if let Err(err) = config::save(&app.config) {
        eprintln!("rcmd: could not save config: {err}");
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
            Some(flag) if flag.starts_with('-') => anyhow::bail!("unknown option: {flag}"),
            _ => dirs.push(PathBuf::from(arg)),
        }
    }
    Ok(Args { printwd, dirs })
}
