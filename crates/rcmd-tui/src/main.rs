mod app;
mod config;
mod format;
mod git;
mod keymap;
mod mcimport;
mod state;
mod subshell;
mod theme;
mod ui;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use app::Startup;

struct Args {
    printwd: Option<PathBuf>,
    dirs: Vec<PathBuf>,
    /// --import-mc [DIR]: print an rcmd config fragment built from mc's
    /// files instead of starting the UI.
    import_mc: Option<Option<PathBuf>>,
    /// The personality: panels, or one of mc's editor/viewer/diff modes.
    startup: Startup,
    over: Overrides,
}

/// The flags that say the config file is wrong for this one run - mc's
/// `-b -c -C -S -d -u -U -l`. All `None` means the config stands.
#[derive(Default)]
struct Overrides {
    /// `-S NAME`: the theme (mc calls it a skin).
    theme: Option<String>,
    /// `-b` / `-c`: false = black and white, true = colour.
    color: Option<bool>,
    /// `-C SPEC`: mc's `keyword=fg,bg:...`.
    colors: Option<String>,
    /// `-d`: no mouse.
    mouse: Option<bool>,
    /// `-u` / `-U`: subshell off / on.
    subshell: Option<bool>,
    /// `-l FILE`: log the FTP/fish dialogue there.
    ftplog: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = parse_args()?;
    if let Some(dir) = &args.import_mc {
        return import_mc(dir.clone());
    }
    let (mut cfg, load_warning) = config::load();
    let mut warnings: Vec<String> = load_warning.into_iter().collect();
    if let Some(mouse) = args.over.mouse {
        cfg.mouse = mouse;
    }
    if let Some(subshell) = args.over.subshell {
        cfg.subshell = subshell;
    }
    let theme = match (args.over.theme.clone(), args.over.color) {
        // -b wins over -S: it is what you type when the colours are
        // not arriving at all, and a skin is still colours
        (_, Some(false)) => "bw".to_string(),
        (Some(name), _) => name,
        // -c against a black-and-white config means "colour, please"
        (None, Some(true)) if cfg.theme == "bw" => "mc".to_string(),
        (None, _) => cfg.theme.clone(),
    };
    warnings.extend(ui::init_theme(&theme));
    if let Some(spec) = &args.over.colors {
        warnings.extend(ui::set_color_spec(spec));
    }
    if let Some(path) = &args.over.ftplog
        && let Err(err) = rcmd_core::vfslog::open(path)
    {
        warnings.push(format!("cannot write {}: {err}", path.display()));
    }
    ui::set_tab_size(cfg.edit_tab_size as usize);
    // the editor's own syntax files, alongside the themes
    if let Some(config) = config::config_path()
        && let Some(dir) = config.parent()
    {
        rcmd_edit::set_user_syntax_dir(dir.join("syntax"));
    }
    let mouse = cfg.mouse;
    let mut terminal = ratatui::init();
    if mouse {
        app::set_mouse_capture(true);
    }
    let result = run(args, cfg, warnings, &mut terminal);
    // Unconditional: the options form can turn the mouse on mid-session
    // (disabling an inactive capture is a harmless escape sequence).
    app::set_mouse_capture(false);
    ratatui::restore();
    result
}

fn run(
    args: Args,
    cfg: config::Config,
    warnings: Vec<String>,
    terminal: &mut ratatui::DefaultTerminal,
) -> Result<()> {
    let mut app = app::App::new(&args.dirs, cfg, warnings, &args.startup)?;
    app.open_startup(args.startup)?;
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

/// What the binary was called: mc ships `mcedit`, `mcview` and `mcdiff`
/// as links to itself and reads argv[0] to decide what it is. rcmd does
/// the same, and answers to mc's names too - somebody's muscle memory
/// is already typing them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Alias {
    None,
    Edit,
    View,
    Diff,
}

fn alias_of(argv0: &Path) -> Alias {
    let name = argv0
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match name.trim_start_matches("rc").trim_start_matches("mc") {
        "edit" => Alias::Edit,
        "view" => Alias::View,
        "diff" => Alias::Diff,
        _ => Alias::None,
    }
}

fn parse_args() -> Result<Args> {
    let mut it = std::env::args_os();
    let alias = alias_of(Path::new(&it.next().unwrap_or_default()));
    let mut printwd = None;
    let mut import_mc = None;
    let mut over = Overrides::default();
    let mut edit: Vec<PathBuf> = Vec::new();
    let mut view: Option<PathBuf> = None;
    let mut free = Vec::new();
    while let Some(arg) = it.next() {
        let mut next = |flag: &str| -> Result<PathBuf> {
            it.next()
                .map(PathBuf::from)
                .with_context(|| format!("{flag} requires a file argument"))
        };
        match arg.to_str() {
            Some("-P") | Some("--printwd") => printwd = Some(next("-P")?),
            Some("-e") | Some("--edit") => edit.push(next("-e")?),
            Some("-v") | Some("--view") => view = Some(next("-v")?),
            Some("-l") | Some("--ftplog") => over.ftplog = Some(next("-l")?),
            Some("-S") | Some("--skin") => {
                over.theme = Some(next("-S")?.to_string_lossy().into_owned())
            }
            Some("-C") | Some("--colors") => {
                over.colors = Some(next("-C")?.to_string_lossy().into_owned())
            }
            Some("-b") | Some("--nocolor") => over.color = Some(false),
            Some("-c") | Some("--color") => over.color = Some(true),
            Some("-d") | Some("--nomouse") => over.mouse = Some(false),
            Some("-u") | Some("--nosubshell") => over.subshell = Some(false),
            Some("-U") | Some("--subshell") => over.subshell = Some(true),
            Some("--import-mc") => {
                // an optional directory argument, but not the next flag
                let next = it.next();
                import_mc = Some(next.map(PathBuf::from));
            }
            Some("-h") | Some("--help") => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            Some("-V") | Some("--version") => {
                println!("rcmd {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            Some(flag) if flag.starts_with('-') && flag != "-" => {
                anyhow::bail!("unknown option: {flag}")
            }
            _ => free.push(PathBuf::from(arg)),
        }
    }
    // argv[0] decides what the free arguments are: files to rcedit and
    // its cousins, starting directories to rcmd
    let (startup, dirs) = match alias {
        Alias::Edit => (
            Startup::Edit(take_files(&mut free, "rcedit", 0)?),
            Vec::new(),
        ),
        Alias::View => (
            Startup::View(one(take_files(&mut free, "rcview", 1)?)),
            Vec::new(),
        ),
        Alias::Diff => {
            let files = take_files(&mut free, "rcdiff", 2)?;
            (
                Startup::Diff(files[0].clone(), files[1].clone()),
                Vec::new(),
            )
        }
        Alias::None if !edit.is_empty() => (Startup::Edit(edit), free),
        Alias::None => match view {
            Some(file) => (Startup::View(file), free),
            None => (Startup::Panels, free),
        },
    };
    Ok(Args {
        printwd,
        dirs,
        import_mc,
        startup,
        over,
    })
}

/// The files an alias was invoked on, checked for how many it needs.
fn take_files(free: &mut Vec<PathBuf>, name: &str, want: usize) -> Result<Vec<PathBuf>> {
    let files = std::mem::take(free);
    match want {
        0 if files.is_empty() => anyhow::bail!("{name}: no file to open"),
        0 => Ok(files),
        n if files.len() == n => Ok(files),
        1 => anyhow::bail!("{name}: expects one file"),
        n => anyhow::bail!("{name}: expects {n} files"),
    }
}

fn one(files: Vec<PathBuf>) -> PathBuf {
    files.into_iter().next().unwrap_or_default()
}

const USAGE: &str = "\
usage: rcmd [OPTIONS] [DIR1 [DIR2]]
       rcedit FILE...    rcview FILE    rcdiff FILE1 FILE2
       rcmd --import-mc [MC_CONFIG_DIR]

  -e, --edit FILE     start in the editor on FILE (repeatable)
  -v, --view FILE     start in the viewer on FILE
  -P, --printwd FILE  write the last active directory to FILE on exit
  -S, --skin NAME     theme: mc, dark, bw
  -b, --nocolor       black and white
  -c, --color         colour (the default)
  -C, --colors SPEC   mc colour spec: keyword=fg,bg:keyword=fg,bg
  -d, --nomouse       no mouse
  -u, --nosubshell    no persistent subshell
  -U, --subshell      persistent subshell
  -l, --ftplog FILE   log the FTP/fish dialogue to FILE
  -V, --version       print the version
  -h, --help          this text
  --import-mc [DIR]   print an rcmd config built from mc's menu,
                      mc.ext and keymap files (default ~/.config/mc)

DIR1/DIR2 are the starting directories for the left/right panel.
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv0_decides_the_personality() {
        assert!(matches!(alias_of(Path::new("/usr/bin/rcmd")), Alias::None));
        assert!(matches!(alias_of(Path::new("rcedit")), Alias::Edit));
        assert!(matches!(alias_of(Path::new("./rcview")), Alias::View));
        assert!(matches!(alias_of(Path::new("/opt/b/rcdiff")), Alias::Diff));
        // somebody's links are still named after mc
        assert!(matches!(alias_of(Path::new("mcedit")), Alias::Edit));
        assert!(matches!(alias_of(Path::new("MCDIFF")), Alias::Diff));
        // not an alias, just a name that starts the same way
        assert!(matches!(alias_of(Path::new("rcmdx")), Alias::None));
    }

    #[test]
    fn an_alias_says_how_many_files_it_wants() {
        let mut two = vec![PathBuf::from("a"), PathBuf::from("b")];
        assert_eq!(take_files(&mut two, "rcdiff", 2).unwrap().len(), 2);
        let mut one = vec![PathBuf::from("a")];
        assert!(take_files(&mut one, "rcdiff", 2).is_err());
        let mut none: Vec<PathBuf> = Vec::new();
        assert!(take_files(&mut none, "rcedit", 0).is_err());
        let mut many = vec![PathBuf::from("a"), PathBuf::from("b")];
        assert_eq!(take_files(&mut many, "rcedit", 0).unwrap().len(), 2);
    }
}
