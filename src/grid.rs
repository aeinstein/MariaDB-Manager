//! Virtualisiertes Datengrid: zeichnet nur sichtbare Zeilen, Text direkt per Painter.

use crate::icons::{self, Icon};
use crate::db::{qi, ResultSet, Stmt};
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
/// Mindestbreite einer Grid-Spalte (Startbreite und beim Ziehen).
const MIN_W: f32 = 30.0;

#[derive(Default)]
pub struct GridState {
    pub sel: Option<(usize, usize)>,
    pub editing: Option<(usize, usize, String, bool)>, // (Zeile, Spalte, Puffer, Fokus gesetzt)
    pub scroll_to: Option<usize>,
    /// Spalte, zu der horizontal gescrollt werden soll (Cursor per Tastatur bewegt)
    scroll_col: Option<usize>,
    /// Tastaturfokus nach dem Bearbeiten an das Grid zurückgeben
    refocus: bool,
    /// Hatte das Grid im letzten Frame den Tastaturfokus?
    pub focused: bool,
    /// Gemessene Spaltenbreiten (Schlüssel aus Spaltennamen + „hat Zeilen“, siehe `col_key`).
    widths: Option<(u64, Vec<f32>)>,
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

/// Wählt aus einer `SHOW KEYS`-Antwort den Schlüssel zur Zeilenidentifikation: PRIMARY KEY, sonst den
/// ersten UNIQUE-Index. `find` bildet einen Spaltennamen der Tabelle auf die Spalte im Ergebnis ab; fehlt
/// eine Schlüsselspalte im Ergebnis, gibt es keinen Schlüssel (leere Liste → Vollvergleich).
/// Liefert die Spaltenindizes und eine Beschreibung für die Statusanzeige.
pub fn pick_key_cols(keys: &ResultSet, find: impl Fn(&str) -> Option<usize>) -> (Vec<usize>, String) {
    let none = "keiner – Änderungen per Vollvergleich (LIMIT 1)".to_string();
    let mut best: Option<String> = None;
    for r in 0..keys.rows.len() {
        let name = keys.get(r, "Key_name");
        if name == "PRIMARY" {
            best = Some(name.into());
            break;
        }
        if best.is_none() && keys.get(r, "Non_unique") == "0" {
            best = Some(name.into());
        }
    }
    let Some(k) = best else { return (Vec::new(), none) };
    let mut cols: Vec<(u32, String)> = (0..keys.rows.len())
        .filter(|r| keys.get(*r, "Key_name") == k)
        .map(|r| (keys.get(r, "Seq_in_index").parse().unwrap_or(0), keys.get(r, "Column_name").to_string()))
        .collect();
    cols.sort();
    match cols.iter().map(|(_, n)| find(n)).collect::<Option<Vec<usize>>>() {
        Some(idx) => {
            let desc = format!("{k} ({})", cols.iter().map(|c| c.1.as_str()).collect::<Vec<_>>().join(", "));
            (idx, desc)
        }
        None => (Vec::new(), none),
    }
}

/// Zielspalten beim Schreiben von Änderungen: SQL-Spaltenname je Ergebnisspalte, Schlüsselspalten und
/// die Vergleichsspalten für den Vollvergleich, wenn es keinen Schlüssel gibt.
pub struct EditTarget<'a> {
    /// Qualifizierter Tabellenname (bereits gequotet).
    pub fq: &'a str,
    pub names: &'a [String],
    pub key_cols: &'a [usize],
    pub cmp_cols: &'a [usize],
}

impl EditTarget<'_> {
    /// WHERE-Klausel, die Zeile `r` über ihre ursprünglichen Werte bestimmt (ohne Schlüssel: alle
    /// Vergleichsspalten mit `LIMIT 1`).
    pub fn key_where(&self, rs: &ResultSet, r: usize, params: &mut Vec<Option<String>>) -> String {
        let cols = if self.key_cols.is_empty() { self.cmp_cols } else { self.key_cols };
        let mut parts = Vec::new();
        for &c in cols {
            parts.push(format!("{} <=> ?", qi(&self.names[c])));
            params.push(rs.rows[r][c].clone());
        }
        let limit = if self.key_cols.is_empty() { " LIMIT 1" } else { "" };
        format!(" WHERE {}{}", parts.join(" AND "), limit)
    }

    /// DELETE/UPDATE/INSERT für alle gesammelten Änderungen.
    pub fn stmts(&self, rs: &ResultSet, e: &GridEdits) -> Vec<Stmt> {
        let fq = self.fq;
        let mut stmts = Vec::new();
        let mut deleted: Vec<usize> = e.deleted.iter().copied().collect();
        deleted.sort_unstable();
        for &r in &deleted {
            if r >= e.new_from || r >= rs.rows.len() {
                continue;
            }
            let mut p = Vec::new();
            let w = self.key_where(rs, r, &mut p);
            stmts.push(Stmt::with(format!("DELETE FROM {fq}{w}"), p));
        }
        let ncols = rs.columns.len();
        for r in 0..e.new_from.min(rs.rows.len()) {
            if e.deleted.contains(&r) {
                continue;
            }
            let changed: Vec<usize> = (0..ncols).filter(|c| e.changed.contains_key(&(r, *c))).collect();
            if changed.is_empty() {
                continue;
            }
            let mut p = Vec::new();
            let sets: Vec<String> = changed
                .iter()
                .map(|c| {
                    p.push(e.changed[&(r, *c)].clone());
                    format!("{} = ?", qi(&self.names[*c]))
                })
                .collect();
            let w = self.key_where(rs, r, &mut p);
            stmts.push(Stmt::with(format!("UPDATE {fq} SET {}{w}", sets.join(", ")), p));
        }
        for r in e.new_from..rs.rows.len() {
            if e.deleted.contains(&r) {
                continue;
            }
            let changed: Vec<usize> = (0..ncols).filter(|c| e.changed.contains_key(&(r, *c))).collect();
            let names: Vec<String> = changed.iter().map(|c| qi(&self.names[*c])).collect();
            let qm: Vec<&str> = changed.iter().map(|_| "?").collect();
            let p: Vec<Option<String>> = changed.iter().map(|c| e.changed[&(r, *c)].clone()).collect();
            stmts.push(Stmt::with(format!("INSERT INTO {fq} ({}) VALUES ({})", names.join(", "), qm.join(", ")), p));
        }
        stmts
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
    let cursor_col = ui.visuals().selection.stroke.color;
    let ui_ctx = ui.ctx().clone();
    let changed_bg = if dark { Color32::from_rgb(0x5a, 0x4a, 0x10) } else { Color32::from_rgb(0xff, 0xf2, 0xb0) };
    let deleted_bg = if dark { Color32::from_rgb(0x5a, 0x1e, 0x1e) } else { Color32::from_rgb(0xff, 0xd0, 0xd0) };
    // Zeilenmuster: 2× normal, 1× rötlich, 2× normal, 1× bläulich
    let red_bg = if dark { Color32::from_rgb(0x3a, 0x28, 0x2a) } else { Color32::from_rgb(0xfb, 0xe6, 0xe6) };
    let blue_bg = if dark { Color32::from_rgb(0x24, 0x2c, 0x3c) } else { Color32::from_rgb(0xe3, 0xec, 0xfb) };
    let gap = ui.spacing().item_spacing.x;
    let new_bg = if dark { Color32::from_rgb(0x1e, 0x4a, 0x24) } else { Color32::from_rgb(0xd8, 0xf5, 0xd8) };
    let can_edit = edits.is_some();

    // Tastaturfokus des Grids: Klick ins Grid holt ihn, Klick daneben gibt ihn ab; Pfeiltasten und Tab
    // bleiben beim Grid. Während eine Zelle bearbeitet wird, gehört der Fokus dem Editor.
    let focus_id = ui.make_persistent_id((id, "grid_focus"));
    if st.editing.is_none() {
        crate::app::focus_area(ui, ui.available_rect_before_wrap(), focus_id);
        let filter = egui::EventFilter { vertical_arrows: true, horizontal_arrows: true, tab: true, ..Default::default() };
        ui.memory_mut(|m| m.set_focus_lock_filter(focus_id, filter));
        if std::mem::take(&mut st.refocus) {
            ui.memory_mut(|m| m.request_focus(focus_id));
        }
    }
    let has_focus = ui.memory(|m| m.has_focus(focus_id));
    st.focused = has_focus;
    // Ohne Cursor beim ersten Fokus auf die erste Zelle setzen
    if has_focus && st.sel.is_none() && nrows > 0 {
        st.sel = Some((0, 0));
    }

    let mut action: Option<Action> = None;
    if has_focus && st.editing.is_none() {
        if let Some((r, c)) = st.sel.filter(|&(r, c)| r < nrows && c < ncols) {
            let del = ui.input(|i| i.key_pressed(Key::Delete));
            if del && can_edit {
                let marked = edits.as_ref().is_some_and(|e| e.deleted.contains(&r));
                action = Some(if marked { Action::Undelete(r) } else { Action::Delete(r) });
            }
            let (up, down, left, right, pgup, pgdn, enter, f2, copy, cmd, home, end, tab, shift) = ui.input(|i| {
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
                    i.modifiers.command,
                    i.key_pressed(Key::Home),
                    i.key_pressed(Key::End),
                    i.key_pressed(Key::Tab),
                    i.modifiers.shift,
                )
            });
            let last_r = nrows.saturating_sub(1);
            let mut nr = r;
            let mut nc = c;
            if up {
                nr = r.saturating_sub(1);
            }
            if down {
                nr = (r + 1).min(last_r);
            }
            if pgup {
                nr = r.saturating_sub(25);
            }
            if pgdn {
                nr = (r + 25).min(last_r);
            }
            // Pos1/Ende: erste/letzte Spalte, mit Strg: erste/letzte Zeile
            if home {
                if cmd { nr = 0 } else { nc = 0 }
            }
            if end {
                if cmd { nr = last_r } else { nc = ncols - 1 }
            }
            if left {
                nc = c.saturating_sub(1);
            }
            if right {
                nc = (c + 1).min(ncols - 1);
            }
            // Tab / Umschalt+Tab: nächste/vorige Zelle mit Zeilenumbruch
            if tab && !shift {
                if c + 1 < ncols {
                    nc = c + 1;
                } else if r < last_r {
                    (nr, nc) = (r + 1, 0);
                }
            }
            if tab && shift {
                if c > 0 {
                    nc = c - 1;
                } else if r > 0 {
                    (nr, nc) = (r - 1, ncols - 1);
                }
            }
            if (nr, nc) != (r, c) {
                st.sel = Some((nr, nc));
                if nr != r {
                    st.scroll_to = Some(nr);
                }
                if nc != c {
                    st.scroll_col = Some(nc);
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

    // Startbreiten so schmal wie möglich: am breitesten Inhalt der ersten Zeilen bzw. am Spaltenkopf.
    // Neue Spalten (andere Abfrage, Daten nach dem Laden) bekommen über den Schlüssel einen neuen
    // Tabellenzustand und damit die neu gemessenen Breiten; manuell gezogene Breiten bleiben sonst erhalten.
    let key = col_key(rs);
    if st.widths.as_ref().is_none_or(|(k, _)| *k != key) {
        st.widths = Some((key, measure_widths(ui, rs, &font)));
    }
    let widths = st.widths.as_ref().map(|(_, w)| w.clone()).unwrap_or_default();

    egui::ScrollArea::horizontal().id_salt((id, "h")).auto_shrink([false, false]).show(ui, |ui| {
        let mut tb = TableBuilder::new(ui)
            .id_salt((id, key))
            .resizable(true)
            .sense(Sense::click())
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
        for w in widths.iter() {
            tb = tb.column(Column::initial(*w).at_least(MIN_W).clip(true));
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
                let row_sel = st.sel.is_some_and(|(sr, _)| sr == r);
                row.set_selected(row_sel);
                let stripe = match r % 6 {
                    2 if !row_sel => Some(red_bg),
                    5 if !row_sel => Some(blue_bg),
                    _ => None,
                };
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
                                    st.refocus = true;
                                } else if enter || te.lost_focus() {
                                    // Enter: Fokus zurück ans Grid, damit sofort weiter navigiert werden kann
                                    st.refocus = enter;
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
                        if let Some(bg) = stripe {
                            // inkl. Spaltenabstand, damit die Zeile durchgehend gefärbt ist
                            let full = rect.expand2(egui::vec2(gap / 2.0, 0.0));
                            ui.painter().clone().with_clip_rect(full).rect_filled(full, 0.0, bg);
                        }
                        if is_del {
                            ui.painter().rect_filled(rect, 0.0, deleted_bg);
                        } else if changed {
                            ui.painter().rect_filled(rect, 0.0, changed_bg);
                        } else if is_new {
                            ui.painter().rect_filled(rect, 0.0, new_bg);
                        }
                        if selected {
                            ui.painter().rect_filled(rect, 0.0, sel_bg.gamma_multiply(0.6));
                            // Cursorrahmen: kräftig, solange das Grid den Tastaturfokus hat
                            let stroke = if has_focus { cursor_col } else { cursor_col.gamma_multiply(0.45) };
                            ui.painter().rect_stroke(
                                rect.shrink(1.0),
                                0.0,
                                egui::Stroke::new(2.0, stroke),
                                egui::StrokeKind::Inside,
                            );
                            if st.scroll_col == Some(c) {
                                ui.scroll_to_rect(rect, None);
                                st.scroll_col = None;
                            }
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
                        ui_ctx.memory_mut(|m| m.request_focus(focus_id));
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

/// Schlüssel für die Spaltenbreiten: Spaltennamen und ob schon Zeilen da sind
/// (beim Laden kommen oft erst die Spalten, dann die Daten).
fn col_key(rs: &ResultSet) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for c in &rs.columns {
        c.name.hash(&mut h);
    }
    rs.rows.is_empty().hash(&mut h);
    h.finish()
}

/// Kleinstmögliche Breite je Spalte: Spaltenkopf (inkl. Platz für den Sortierpfeil) bzw. breitester
/// angezeigter Wert der ersten Zeilen, höchstens `MAX_W`.
fn measure_widths(ui: &egui::Ui, rs: &ResultSet, font: &egui::FontId) -> Vec<f32> {
    const SAMPLE_ROWS: usize = 200;
    const PAD: f32 = 10.0; // 4 px Innenabstand je Seite + etwas Luft
    const MAX_W: f32 = 400.0;
    let c = ui.visuals().text_color();
    let measure = |t: String| ui.painter().layout_no_wrap(t, font.clone(), c).size().x;
    let arrow = measure(" ⏷".into());
    rs.columns
        .iter()
        .enumerate()
        .map(|(ci, col)| {
            let mut w = measure(col.name.clone()) + arrow;
            for row in rs.rows.iter().take(SAMPLE_ROWS) {
                let t = match row.get(ci) {
                    Some(Some(v)) => display_text(v),
                    _ => "(NULL)".to_string(),
                };
                w = w.max(measure(t));
                if w >= MAX_W {
                    break;
                }
            }
            (w + PAD).clamp(MIN_W, MAX_W).ceil()
        })
        .collect()
}

fn display_text(v: &str) -> String {
    let first = v.split(['\n', '\r']).next().unwrap_or("");
    let mut s: String = first.chars().take(MAX_CELL_CHARS).collect();
    if s.len() < v.len() {
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Col;

    fn rs() -> ResultSet {
        let col = |n: &str| Col { name: n.into(), ..Default::default() };
        ResultSet {
            columns: vec![col("id"), col("n")],
            rows: vec![vec![Some("1".into()), Some("alt".into())], vec![Some("2".into()), None]],
            truncated: false,
        }
    }

    #[test]
    fn edit_stmts_where() {
        let rs = rs();
        let mut e = GridEdits::default();
        e.clear(rs.rows.len());
        e.changed.insert((0, 1), Some("neu".into()));
        e.deleted.insert(1);
        // Alias „n“ → Originalspalte „name“
        let names = vec!["id".to_string(), "name".to_string()];

        let t = EditTarget { fq: "`db`.`t`", names: &names, key_cols: &[0], cmp_cols: &[0, 1] };
        let s = t.stmts(&rs, &e);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].sql, "DELETE FROM `db`.`t` WHERE `id` <=> ?");
        assert_eq!(s[0].params, Some(vec![Some("2".into())]));
        assert_eq!(s[1].sql, "UPDATE `db`.`t` SET `name` = ? WHERE `id` <=> ?");
        assert_eq!(s[1].params, Some(vec![Some("neu".into()), Some("1".into())]));

        // Ohne Schlüssel: Vollvergleich über alle Spalten mit LIMIT 1
        let t = EditTarget { fq: "`db`.`t`", names: &names, key_cols: &[], cmp_cols: &[0, 1] };
        let s = t.stmts(&rs, &e);
        assert_eq!(s[0].sql, "DELETE FROM `db`.`t` WHERE `id` <=> ? AND `name` <=> ? LIMIT 1");
        assert_eq!(s[0].params, Some(vec![Some("2".into()), None]));
        assert_eq!(s[1].sql, "UPDATE `db`.`t` SET `name` = ? WHERE `id` <=> ? AND `name` <=> ? LIMIT 1");
        assert_eq!(s[1].params, Some(vec![Some("neu".into()), Some("1".into()), Some("alt".into())]));
    }

    #[test]
    fn pick_key_cols_primary() {
        let col = |n: &str| Col { name: n.into(), ..Default::default() };
        let keys = ResultSet {
            columns: vec![col("Key_name"), col("Non_unique"), col("Seq_in_index"), col("Column_name")],
            rows: vec![
                vec![Some("u".into()), Some("0".into()), Some("1".into()), Some("n".into())],
                vec![Some("PRIMARY".into()), Some("0".into()), Some("1".into()), Some("id".into())],
            ],
            truncated: false,
        };
        let (k, d) = pick_key_cols(&keys, |n| ["id", "n"].iter().position(|c| *c == n));
        assert_eq!(k, vec![0]);
        assert_eq!(d, "PRIMARY (id)");
        // Schlüsselspalte fehlt im Ergebnis → Vollvergleich
        let (k, _) = pick_key_cols(&keys, |n| (n == "n").then_some(0));
        assert!(k.is_empty());
    }
}
