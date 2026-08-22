mod app;
mod config;
mod git;
mod keymap;
mod mcimport;
mod state;
mod subshell;
mod ui;

use std::path::PathBuf;

use anyhow::{Context, Result};

struct Args {
    printwd: Option<PathBuf>,
    dirs: Vec<PathBuf>,
    /// --import-mc [DIR]: print an rcmd config fragment built from mc's
    /// files instead of starting the UI.
    import_mc: Option<Option<PathBuf>>,
}

fn main() -> Result<()> {
    let args = parse_args()?;
    if let Some(dir) = &args.import_mc {
        return import_mc(dir.clone());
    }
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

    // Persist the panel state for the next session - onto the *on-disk*
    // state file, not this instance's copy: options-form and hotlist
    // changes are written through when they happen, and another instance
    // may have saved its own since we started.
    let panel = &app.panels[app.active];
    let (show_hidden, sort_reverse) = (panel.show_hidden, panel.sort_reverse);
    let sort_key = config::sort_key_name(panel.sort_key).to_string();
    let listing = config::list_mode_name(panel.list_mode).to_string();
    let history: Vec<String> = app.cmdline.history().to_vec();
    if let Err(err) = state::update(|s| {
        s.show_hidden = Some(show_hidden);
        s.sort_key = Some(sort_key);
        s.sort_reverse = Some(sort_reverse);
        s.listing = Some(listing);
        s.cmd_history = history;
    }) {
        eprintln!("rcmd: could not save state: {err}");
    }

    if let Some(path) = &args.printwd {
        let cwd = &app.panels[app.active].cwd;
        let _ = std::fs::write(path, cwd.as_os_str().as_encoded_bytes());
    }
    result
}

/// `rcmd --import-mc [DIR]`: convert mc's menu / mc.ext / keymap files
/// into an rcmd config fragment on stdout. Never writes config.toml -
/// that file is the user's, so the conversion is theirs to paste.
fn import_mc(dir: Option<PathBuf>) -> Result<()> {
    let dir = match dir.or_else(mcimport::default_dir) {
        Some(dir) => dir,
        None => anyhow::bail!("cannot locate mc's config directory (pass it explicitly)"),
    };
    if !dir.is_dir() {
        anyhow::bail!("{} is not a directory", dir.display());
    }
    let imported = mcimport::import_dir(&dir);
    print!("{}", mcimport::to_toml(&imported));
    for warning in &imported.warnings {
        eprintln!("rcmd: {warning}");
    }
    eprintln!(
        "rcmd: imported {} command(s), {} opener(s), {} view filter(s), {} key(s) from {}",
        imported.commands.len(),
        imported.open.len(),
        imported.view.len(),
        imported.keys.len(),
        dir.display()
    );
    Ok(())
}

fn parse_args() -> Result<Args> {
    let mut printwd = None;
    let mut import_mc = None;
    let mut dirs = Vec::new();
    let mut it = std::env::args_os().skip(1);
    while let Some(arg) = it.next() {
        match arg.to_str() {
            Some("-P") | Some("--printwd") => {
                let path = it.next().context("-P requires a file argument")?;
                printwd = Some(PathBuf::from(path));
            }
            Some("--import-mc") => {
                // an optional directory argument, but not the next flag
                let next = it.next();
                import_mc = Some(next.map(PathBuf::from));
            }
            Some("-h") | Some("--help") => {
                println!(
                    "usage: rcmd [-P FILE] [DIR1 [DIR2]]\n\
                     \x20      rcmd --import-mc [MC_CONFIG_DIR]\n\n\
                     -P FILE   write the last active directory to FILE on exit\n\
                     DIR1/DIR2 starting directories for the left/right panel\n\
                     --import-mc  print an rcmd config built from mc's menu,\n\
                     \x20            mc.ext and keymap files (default ~/.config/mc)"
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
    Ok(Args {
        printwd,
        dirs,
        import_mc,
    })
}
