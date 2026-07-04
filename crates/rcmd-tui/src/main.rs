mod app;
mod ui;

use anyhow::Result;

fn main() -> Result<()> {
    let mut terminal = ratatui::init();
    let result = app::App::new().and_then(|mut app| app.run(&mut terminal));
    ratatui::restore();
    result
}
