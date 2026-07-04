mod app;
mod ui;

use std::path::PathBuf;

use anyhow::{Context, Result};

struct Args {
    printwd: Option<PathBuf>,
    dirs: Vec<PathBuf>,
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let mut terminal = ratatui::init();
    let result = run(&args, &mut terminal);
    ratatui::restore();
    result
}

fn run(args: &Args, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
    let mut app = app::App::new(&args.dirs)?;
    let result = app.run(terminal);
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
