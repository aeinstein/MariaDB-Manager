//! Virtualisiertes Datengrid: zeichnet nur sichtbare Zeilen, Text direkt per Painter.

use crate::icons::{self, Icon};
use crate::db::ResultSet;
use eframe::egui::{self, Align2, Color32, Key, RichText, Sense};
use egui_extras::{Column, TableBuilder};
use std::collections::{HashMap, HashSet};

/// Zeilenhöhe im Datengrid (aus den Optionen, als f32-Bits).
static ROW_H: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x41A0_0000); // 20.0

pub fn row_h() -> f32 {
    f32::from_bits(ROW_H.load(std::sync::atomic::Ordering::Relaxed))
}

pub fn set_row_h(h: f32) {
    ROW_H.store(h.clamp(14.0, 60.0).to_bits(), std::sync::atomic::Ordering::Relaxed);
}
const MAX_CELL_CHARS: usize = 256;

#[derive(Default)]
pub struct GridState {
    pub sel: Option<(usize, usize)>,
    pub editing: Option<(usize, usize, String, bool)>, // (Zeile, Spalte, Puffer, Fokus gesetzt)
    pub scroll_to: Option<usize>,
    /// Hatte im letzten Frame ein anderes Widget den Fokus? (verhindert, dass z. B. Enter im Filter das Grid erreicht)
    pub had_focus_elsewhere: bool,
}

#[derive(Default)]
pub struct GridEdits {
    /// Geänderte Zellen (Zeile, Spalte) -> neuer Wert
    pub changed: HashMap<(usize, usize), Option<String>>,
    /// Zum Löschen markierte Zeilen
    pub deleted: HashSet<usize>,
    /// Zeilen ab diesem Index sind neu (INSERT)
    pub new_from: usize,
    /// Spalten, die nicht bearbeitbar sind (BLOB/Binär)
    pub readonly_cols: HashSet<usize>,
}

impl GridEdits {
    pub fn is_dirty(&self, total_rows: usize) -> bool {
        !self.changed.is_empty() || !self.deleted.is_empty() || total_rows > self.new_from
    }
    pub fn clear(&mut self, rows: usize) {
        self.changed.clear();
        self.deleted.clear();
        self.new_from = rows;
    }
}

#[derive(Default)]
pub struct GridOut {
    pub sort_clicked: Option<usize>,
    pub edited: bool,
    pub filter_by: Option<(usize, Option<String>)>,
}

enum Action {
    Copy(String),
    CopyRow(usize),
    SetNull(usize, usize),
    Delete(usize),
    Undelete(usize),
    Edit(usize, usize),
    FilterBy(usize, Option<String>),
}

pub fn show(
    ui: &mut egui::Ui,
    id: &str,
    rs: &ResultSet,
    st: &mut GridState,
    mut edits: Option<&mut GridEdits>,
    sort: Option<(usize, bool)>,
) -> GridOut {
    let mut out = GridOut::default();
    let ncols = rs.columns.len();
    let nrows = rs.rows.len();
    if ncols == 0 {
        ui.weak("Keine Spalten.");
        return out;
    }
    let dark = ui.visuals().dark_mode;
    let font = egui::TextStyle::Body.resolve(ui.style());
    let text_col = ui.visuals().text_color();
    let weak_col = ui.visuals().weak_text_color();
    let sel_bg = ui.visuals().selection.bg_fill;
    let changed_bg = if dark { Color32::from_rgb(0x5a, 0x4a, 0x10) } else { Color32::from_rgb(0xff, 0xf2, 0xb0) };
    let deleted_bg = if dark { Color32::from_rgb(0x5a, 0x1e, 0x1e) } else { Color32::from_rgb(0xff, 0xd0, 0xd0) };
    let new_bg = if dark { Color32::from_rgb(0x1e, 0x4a, 0x24) } else { Color32::from_rgb(0xd8, 0xf5, 0xd8) };
    let can_edit = edits.is_some();

    // Tastatur (nur wenn kein anderes Widget den Fokus hat)
    let no_focus = ui.ctx().memory(|m| m.focused().is_none());
    let keys_ok = no_focus && !st.had_focus_elsewhere;
    st.had_focus_elsewhere = !no_focus && st.editing.is_none();
    let mut action: Option<Action> = None;
    if keys_ok && st.editing.is_none() {
        if let Some((r, c)) = st.sel {
            let del = ui.input(|i| i.key_pressed(Key::Delete));
            if del && can_edit {
                let marked = edits.as_ref().is_some_and(|e| e.deleted.contains(&r));
                action = Some(if marked { Action::Undelete(r) } else { Action::Delete(r) });
            }
            let (up, down, left, right, pgup, pgdn, enter, f2, copy, home, end) = ui.input(|i| {
                (
                    i.key_pressed(Key::ArrowUp),
                    i.key_pressed(Key::ArrowDown),
                    i.key_pressed(Key::ArrowLeft),
                    i.key_pressed(Key::ArrowRight),
                    i.key_pressed(Key::PageUp),
                    i.key_pressed(Key::PageDown),
                    i.key_pressed(Key::Enter),
                    i.key_pressed(Key::F2),
                    i.modifiers.command && i.key_pressed(Key::C),
                    i.modifiers.command && i.key_pressed(Key::Home),
                    i.modifiers.command && i.key_pressed(Key::End),
                )
            });
            let mut nr = r;
            let mut nc = c;
            if up {
                nr = r.saturating_sub(1);
            }
            if down {
                nr = (r + 1).min(nrows.saturating_sub(1));
            }
            if pgup {
                nr = r.saturating_sub(25);
            }
            if pgdn {
                nr = (r + 25).min(nrows.saturating_sub(1));
            }
            if home {
                nr = 0;
            }
            if end {
                nr = nrows.saturating_sub(1);
            }
            if left {
                nc = c.saturating_sub(1);
            }
            if right {
                nc = (c + 1).min(ncols - 1);
            }
            if (nr, nc) != (r, c) {
                st.sel = Some((nr, nc));
                if nr != r {
                    st.scroll_to = Some(nr);
                }
            }
            if (enter || f2) && can_edit {
                action = Some(Action::Edit(r, c));
            }
            if copy {
                let v = cell_value(rs, edits.as_deref(), r, c).cloned().flatten().unwrap_or_default();
                action = Some(Action::Copy(v));
            }
        }
    }

    let scroll_to = st.scroll_to.take();

    egui::ScrollArea::horizontal().id_salt((id, "h")).auto_shrink([false, false]).show(ui, |ui| {
        let mut tb = TableBuilder::new(ui)
            .id_salt(id)
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
        for (ci, col) in rs.columns.iter().enumerate() {
            // Startbreite aus den ersten Zeilen schätzen
            let mut w = col.name.len().max(4) as f32 * 7.5 + 24.0;
            for row in rs.rows.iter().take(50) {
                if let Some(Some(v)) = row.get(ci) {
                    w = w.max(v.chars().take(40).count() as f32 * 7.0 + 12.0);
                }
            }
            tb = tb.column(Column::initial(w.clamp(50.0, 320.0)).at_least(24.0).clip(true));
        }
        if let Some(r) = scroll_to {
            tb = tb.scroll_to_row(r, None);
        }
        tb.header(row_h().max(20.0) + 2.0, |mut h| {
            for (ci, col) in rs.columns.iter().enumerate() {
                h.col(|ui| {
                    let arrow = match sort {
                        Some((s, true)) if s == ci => " ⏶",
                        Some((s, false)) if s == ci => " ⏷",
                        _ => "",
                    };
                    let r = ui.add(
                        egui::Label::new(RichText::new(format!("{}{}", col.name, arrow)).strong())
                            .sense(Sense::click())
                            .selectable(false)
                            .truncate(),
                    );
                    if r.clicked() {
                        out.sort_clicked = Some(ci);
                    }
                });
            }
        })
        .body(|body| {
            body.rows(row_h(), nrows, |mut row| {
                let r = row.index();
                let is_new = edits.as_ref().is_some_and(|e| r >= e.new_from);
                let is_del = edits.as_ref().is_some_and(|e| e.deleted.contains(&r));
                row.set_selected(st.sel.is_some_and(|(sr, _)| sr == r));
                for c in 0..ncols {
                    let editing_here = matches!(&st.editing, Some((er, ec, _, _)) if *er == r && *ec == c);
                    let changed = edits.as_ref().is_some_and(|e| e.changed.contains_key(&(r, c)));
                    let value = cell_value(rs, edits.as_deref(), r, c).cloned().flatten();
                    let numeric = rs.columns[c].numeric;
                    let selected = st.sel == Some((r, c));
                    let (rect, resp) = row.col(|ui| {
                        if editing_here {
                            if let Some((_, _, buf, focused)) = st.editing.as_mut() {
                                let te = ui.add(
                                    egui::TextEdit::singleline(buf)
                                        .desired_width(f32::INFINITY)
                                        .margin(egui::vec2(2.0, 0.0))
                                        // ganze Zeilenhöhe, Text vertikal mittig
                                        .min_size(egui::vec2(0.0, ui.available_height()))
                                        .align(egui::Align2::LEFT_CENTER),
                                );
                                if !*focused {
                                    te.request_focus();
                                    *focused = true;
                                }
                                let (enter, esc) =
                                    ui.input(|i| (i.key_pressed(Key::Enter), i.key_pressed(Key::Escape)));
                                if esc {
                                    st.editing = None;
                                } else if enter || te.lost_focus() {
                                    if let Some((er, ec, buf, _)) = st.editing.take() {
                                        if let Some(e) = edits.as_deref_mut() {
                                            commit_edit(rs, e, er, ec, Some(buf));
                                            out.edited = true;
                                        }
                                    }
                                }
                            }
                            return;
                        }
                        let rect = ui.max_rect();
                        if is_del {
                            ui.painter().rect_filled(rect, 0.0, deleted_bg);
                        } else if changed {
                            ui.painter().rect_filled(rect, 0.0, changed_bg);
                        } else if is_new {
                            ui.painter().rect_filled(rect, 0.0, new_bg);
                        }
                        if selected {
                            ui.painter().rect_filled(rect, 0.0, sel_bg.gamma_multiply(0.6));
                        }
                        let (txt, color) = match &value {
                            None => ("(NULL)".to_string(), weak_col),
                            Some(v) => (display_text(v), text_col),
                        };
                        let (pos, align) = if numeric && value.is_some() {
                            (rect.right_center() - egui::vec2(4.0, 0.0), Align2::RIGHT_CENTER)
                        } else {
                            (rect.left_center() + egui::vec2(4.0, 0.0), Align2::LEFT_CENTER)
                        };
                        ui.painter().text(pos, align, txt, font.clone(), color);
                    });
                    let _ = rect;
                    if resp.clicked() {
                        st.sel = Some((r, c));
                    }
                    if resp.double_clicked() && can_edit {
                        action = Some(Action::Edit(r, c));
                    }
                    resp.context_menu(|ui| {
                        st.sel = Some((r, c));
                        if ui.add(icons::button(Icon::Copy, "Wert kopieren")).clicked() {
                            action = Some(Action::Copy(value.clone().unwrap_or_default()));
                            ui.close();
                        }
                        if ui.add(icons::button(Icon::Copy, "Zeile kopieren (Tab-getrennt)")).clicked() {
                            action = Some(Action::CopyRow(r));
                            ui.close();
                        }
                        if ui.add(icons::button(Icon::Search, "Nach diesem Wert filtern")).clicked() {
                            action = Some(Action::FilterBy(c, value.clone()));
                            ui.close();
                        }
                        if can_edit {
                            ui.separator();
                            let ro = edits.as_ref().is_some_and(|e| e.readonly_cols.contains(&c));
                            if ui.add_enabled(!ro, icons::button(Icon::Edit, "Bearbeiten (F2)")).clicked() {
                                action = Some(Action::Edit(r, c));
                                ui.close();
                            }
                            if ui.add_enabled(!ro, egui::Button::new("Auf NULL setzen")).clicked() {
                                action = Some(Action::SetNull(r, c));
                                ui.close();
                            }
                            if is_del {
                                if ui.button("↺ Löschung zurücknehmen").clicked() {
                                    action = Some(Action::Undelete(r));
                                    ui.close();
                                }
                            } else if ui.add(icons::button(Icon::Delete, "Zeile löschen (Entf)")).clicked() {
                                action = Some(Action::Delete(r));
                                ui.close();
                            }
                        }
                    });
                }
            });
        });
    });

    match action {
        Some(Action::Copy(v)) => ui.ctx().copy_text(v),
        Some(Action::CopyRow(r)) => {
            let line: Vec<String> = (0..ncols)
                .map(|c| cell_value(rs, edits.as_deref(), r, c).cloned().flatten().unwrap_or_else(|| "NULL".into()))
                .collect();
            ui.ctx().copy_text(line.join("\t"));
        }
        Some(Action::Edit(r, c)) => {
            if !edits.as_ref().is_some_and(|e| e.readonly_cols.contains(&c)) {
                let v = cell_value(rs, edits.as_deref(), r, c).cloned().flatten().unwrap_or_default();
                st.sel = Some((r, c));
                st.editing = Some((r, c, v, false));
            }
        }
        Some(Action::SetNull(r, c)) => {
            if let Some(e) = edits.as_deref_mut() {
                commit_edit(rs, e, r, c, None);
                out.edited = true;
            }
        }
        Some(Action::Delete(r)) => {
            if let Some(e) = edits.as_deref_mut() {
                e.deleted.insert(r);
                out.edited = true;
            }
        }
        Some(Action::Undelete(r)) => {
            if let Some(e) = edits.as_deref_mut() {
                e.deleted.remove(&r);
                out.edited = true;
            }
        }
        Some(Action::FilterBy(c, v)) => out.filter_by = Some((c, v)),
        None => {}
    }
    out
}

pub fn cell_value<'a>(rs: &'a ResultSet, edits: Option<&'a GridEdits>, r: usize, c: usize) -> Option<&'a Option<String>> {
    if let Some(e) = edits {
        if let Some(v) = e.changed.get(&(r, c)) {
            return Some(v);
        }
    }
    rs.rows.get(r).and_then(|row| row.get(c))
}

fn commit_edit(rs: &ResultSet, e: &mut GridEdits, r: usize, c: usize, v: Option<String>) {
    let orig = rs.rows.get(r).and_then(|row| row.get(c)).cloned().flatten();
    if r < e.new_from && orig == v {
        e.changed.remove(&(r, c));
    } else {
        e.changed.insert((r, c), v);
    }
}

fn display_text(v: &str) -> String {
    let first = v.split(['\n', '\r']).next().unwrap_or("");
    let mut s: String = first.chars().take(MAX_CELL_CHARS).collect();
    if s.len() < v.len() {
        s.push('…');
    }
    s
}
