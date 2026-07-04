use anyhow::{Context, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;
use ratatui::DefaultTerminal;
use rcmd_core::panel::Panel;

use crate::ui;

pub struct App {
    pub panels: [Panel; 2],
    pub table_states: [TableState; 2],
    pub active: usize,
    pub status: Option<String>,
    /// Rows visible inside a panel; updated on every draw, drives PgUp/PgDn.
    pub panel_rows: usize,
    pub quit: bool,
}

impl App {
    pub fn new() -> Result<Self> {
        let cwd = std::env::current_dir().context("cannot determine current directory")?;
        let left = Panel::new(cwd.clone())
            .with_context(|| format!("cannot read directory {}", cwd.display()))?;
        let right = Panel::new(cwd.clone())
            .with_context(|| format!("cannot read directory {}", cwd.display()))?;
        Ok(App {
            panels: [left, right],
            table_states: [TableState::default(), TableState::default()],
            active: 0,
            status: None,
            panel_rows: 1,
            quit: false,
        })
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|frame| ui::draw(frame, self))?;
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    self.on_key(key);
                }
            }
        }
        Ok(())
    }

    fn on_key(&mut self, key: KeyEvent) {
        self.status = None;
        let page = self.panel_rows.saturating_sub(1).max(1);
        match (key.code, key.modifiers) {
            (KeyCode::F(10), _) => self.quit = true,
            (KeyCode::Char('q'), KeyModifiers::NONE) => self.quit = true,
            (KeyCode::Tab, _) | (KeyCode::BackTab, _) => self.active ^= 1,
            (KeyCode::Up, _) => self.panel().move_up(),
            (KeyCode::Down, _) => self.panel().move_down(),
            (KeyCode::Home, _) => self.panel().move_top(),
            (KeyCode::End, _) => self.panel().move_bottom(),
            (KeyCode::PageUp, _) => self.panel().page_up(page),
            (KeyCode::PageDown, _) => self.panel().page_down(page),
            (KeyCode::Enter, _) => self.fallible(|p| p.enter()),
            (KeyCode::Backspace, _) => self.fallible(|p| p.go_up()),
            (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
                self.fallible(|p| p.reload().map(|()| true))
            }
            _ => {}
        }
    }

    fn panel(&mut self) -> &mut Panel {
        &mut self.panels[self.active]
    }

    fn fallible(&mut self, op: impl FnOnce(&mut Panel) -> std::io::Result<bool>) {
        if let Err(err) = op(self.panel()) {
            self.status = Some(format!(" {err} "));
        }
    }
}
