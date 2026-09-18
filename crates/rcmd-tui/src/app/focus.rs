//! What has the keyboard, for the things that are not plain keys: a
//! paste, M-h's history list and M-Tab's completion, all of which go
//! to whichever line is being typed into - whatever form, prompt or
//! panel it belongs to.

use super::*;

impl App {
    /// The line being typed into, found the way `dispatch_key` finds
    /// who gets a key: the topmost thing that takes keys decides.
    pub fn focused_line(&mut self) -> Option<FocusedLine<'_>> {
        if self.screen_list.is_some() || self.fg_job().is_some() {
            return None;
        }
        if self.connect.is_some() {
            return match self.connect.as_mut()?.ask.as_mut()? {
                ConnectAsk::Password { value, cursor, .. } => {
                    Some(FocusedLine::Plain(value, cursor))
                }
                ConnectAsk::HostKey { .. } => None,
            };
        }
        if self.find.is_some() {
            return None;
        }
        if self.dialog.is_some() {
            return match self.dialog.as_mut()? {
                Dialog::Input(d) => Some(FocusedLine::Field(&mut d.field)),
                Dialog::Fuzzy(d) => Some(FocusedLine::Field(&mut d.field)),
                Dialog::Palette(d) => Some(FocusedLine::Field(&mut d.field)),
                Dialog::Find(d) => d.field().map(FocusedLine::Field),
                Dialog::Pattern(d) => d.field_mut().map(FocusedLine::Field),
                Dialog::Transfer(d) => match d.row {
                    0 => Some(FocusedLine::Field(&mut d.mask)),
                    TRANSFER_DEST_ROW => Some(FocusedLine::Field(&mut d.dest)),
                    _ => None,
                },
                Dialog::Link(d) => match d.row {
                    0 => Some(FocusedLine::Field(&mut d.target)),
                    1 if d.kind != LinkKind::EditSymlink => Some(FocusedLine::Field(&mut d.name)),
                    _ => None,
                },
                Dialog::Panelize(d) => match d.naming.as_mut() {
                    Some((name, cursor)) => Some(FocusedLine::Plain(name, cursor)),
                    None if !d.on_list => Some(FocusedLine::Field(&mut d.command)),
                    None => None,
                },
                Dialog::Chmod(d) if d.row == CHMOD_OCTAL_ROW => {
                    Some(FocusedLine::Plain(&mut d.octal, &mut d.octal_cursor))
                }
                _ => None,
            };
        }
        if self.help.is_some() {
            return None;
        }
        if self.editor().is_some() {
            return match self.editor_mut()?.prompt.as_mut()? {
                EditPrompt::Search(d) if d.row == VIEW_SEARCH_FIELD => {
                    Some(FocusedLine::Field(&mut d.field))
                }
                EditPrompt::ReplaceFind(field) => Some(FocusedLine::Field(field)),
                EditPrompt::SaveAs(field) => Some(FocusedLine::Field(field)),
                EditPrompt::ReplaceWith { field, .. } => Some(FocusedLine::Field(field)),
                EditPrompt::Goto { value, cursor } => Some(FocusedLine::Plain(value, cursor)),
                _ => None,
            };
        }
        if self.viewer().is_some() {
            let v = self.viewer_mut()?;
            if let Some((value, cursor)) = v.goto.as_mut() {
                return Some(FocusedLine::Plain(value, cursor));
            }
            return match v.prompt.as_mut() {
                Some(d) if d.row == VIEW_SEARCH_FIELD => Some(FocusedLine::Field(&mut d.field)),
                _ => None,
            };
        }
        if self.diff().is_some() {
            return match self.diff_mut()?.prompt.as_mut()? {
                DiffPrompt::Search(field) => Some(FocusedLine::Field(field)),
                DiffPrompt::Goto(value, cursor) => Some(FocusedLine::Plain(value, cursor)),
            };
        }
        if self.menu.is_some() || self.quick_search.is_some() {
            return None;
        }
        if !self.config.show_cmdline {
            return None;
        }
        Some(FocusedLine::Plain(
            &mut self.cmdline.value,
            &mut self.cmdline.cursor,
        ))
    }

    /// A bracketed paste - or the window's Ctrl+V. A line that has the
    /// keyboard takes it as text, newlines flattened to spaces so a
    /// paste never runs a command or submits a form. The editor takes
    /// it whole, as one undo step and without auto-indent, so pasted
    /// code keeps its shape. Anything else has no use for text.
    pub fn on_paste(&mut self, text: &str) {
        self.status = None;
        if self.field_popup.is_some() {
            return;
        }
        if let Some(line) = self.focused_line() {
            line.insert(text);
            if self.dialog.is_none() && self.editor().is_none() && self.viewer().is_none() {
                self.cmdline.hist_pos = None;
            }
            return;
        }
        let editor_free = self.fg_job().is_none()
            && self.dialog.is_none()
            && self.help.is_none()
            && self.screen_list.is_none();
        if editor_free
            && let Some(st) = self.editor_mut()
            && st.prompt.is_none()
            && st.menu.is_none()
        {
            // terminals send a pasted line break as CR
            let text = text.replace("\r\n", "\n").replace('\r', "\n");
            let line = st.ed.cursor.line;
            st.ed.insert(&text);
            if let Some(hl) = st.hl.as_mut() {
                hl.invalidate_from(line);
            }
        }
    }

    /// Open M-h's list over the focused field. False = no field with a
    /// history has the keyboard, and the key is someone else's.
    pub(super) fn open_field_popup(&mut self) -> bool {
        let Some(FocusedLine::Field(field)) = self.focused_line() else {
            return false;
        };
        if field.history_name().is_none() {
            return false;
        }
        // newest first, the way M-p walks them
        let rows: Vec<String> = field.entries().iter().rev().cloned().collect();
        if rows.is_empty() {
            self.status = Some(" nothing typed here before ".into());
            return true;
        }
        self.field_popup = Some(FieldPopup {
            title: " History ",
            picks: rows.clone(),
            rows,
            selected: 0,
            span: None,
        });
        true
    }

    /// Whether the panels, and so the command line, have the keyboard.
    pub(super) fn on_panels(&self) -> bool {
        self.fg_job().is_none()
            && self.connect.is_none()
            && self.find.is_none()
            && self.dialog.is_none()
            && self.help.is_none()
            && self.editor().is_none()
            && self.viewer().is_none()
            && self.diff().is_none()
            && self.menu.is_none()
            && self.quick_search.is_none()
    }

    /// Complete the word before the cursor in the line that has the
    /// keyboard. One candidate completes it; several advance it as far
    /// as they agree and open a list to pick from, where mc squeezed
    /// them onto the status line. False = no line has the keyboard.
    pub(super) fn complete_focused(&mut self, commands: bool) -> bool {
        let cwd = self.panels[self.active].local_cwd();
        let Some(mut line) = self.focused_line() else {
            return false;
        };
        let (value, cursor) = line.parts();
        match crate::field::complete(value, cursor, &cwd, commands) {
            crate::field::Completion::NoMatch => self.status = Some(" no match ".into()),
            crate::field::Completion::Done => {}
            crate::field::Completion::Several { names, words, span } => {
                self.field_popup = Some(FieldPopup {
                    title: " Complete ",
                    rows: names,
                    picks: words,
                    selected: 0,
                    span: Some(span),
                });
            }
        }
        true
    }

    pub(super) fn on_field_popup_key(&mut self, key: KeyEvent) {
        let Some(popup) = self.field_popup.as_mut() else {
            return;
        };
        let last = popup.rows.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => self.field_popup = None,
            KeyCode::Enter | KeyCode::Tab => {
                let Some(popup) = self.field_popup.take() else {
                    return;
                };
                let Some(pick) = popup.picks.get(popup.selected) else {
                    return;
                };
                match (self.focused_line(), popup.span) {
                    (Some(FocusedLine::Field(field)), None) => field.set(pick.clone()),
                    (Some(mut line), Some(span)) => {
                        let (value, cursor) = line.parts();
                        crate::field::replace_span(value, cursor, span, pick);
                    }
                    _ => {}
                }
                if self.on_panels() {
                    self.cmdline.hist_pos = None;
                }
            }
            KeyCode::Up => popup.selected = popup.selected.saturating_sub(1),
            KeyCode::Down => popup.selected = (popup.selected + 1).min(last),
            KeyCode::PageUp => popup.selected = popup.selected.saturating_sub(10),
            KeyCode::PageDown => popup.selected = (popup.selected + 10).min(last),
            KeyCode::Home => popup.selected = 0,
            KeyCode::End => popup.selected = last,
            _ => {}
        }
    }
}
