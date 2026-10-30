//! SQL-Editor-Tab mit Highlighting, Ausführen (alles/Auswahl), Ergebnis-Tabs.

use crate::cellview::{self, CellKey, CellView};
use crate::export::{self, ExportDialog};
use crate::queries;
use crate::icons::{self, Icon};
use crate::datatab::fmt_dur;
use crate::db::{qi, split_sql, ConnConfig, Mode, ResultSet, Stmt, Worker};
use crate::grid::{self, GridState};
use crate::sqlhl;
use eframe::egui::{self, Key, Modifiers};
use std::collections::HashSet;
use std::time::Instant;

/// Abfrage, die einen gekürzten Binärwert vollständig aus seiner Ursprungstabelle lädt.
/// Die Zeile wird über alle übrigen (nicht binären) Spalten derselben Tabelle im Ergebnis
/// bestimmt; `None`, wenn die Spalte keine Ursprungstabelle hat oder keine Vergleichsspalte da ist.
fn blob_fetch_stmt(rs: &ResultSet, r: usize, c: usize) -> Option<Stmt> {
    let col = rs.columns.get(c)?;
    let row = rs.rows.get(r)?;
    if col.schema.is_empty() || col.table.is_empty() || col.org.is_empty() {
        return None;
    }
    let mut seen = HashSet::new();
    let (mut parts, mut params) = (Vec::new(), Vec::new());
    for (k, kc) in rs.columns.iter().enumerate() {
        if k == c || kc.binary || kc.org.is_empty() || kc.schema != col.schema || kc.table != col.table {
            continue;
        }
        if seen.insert(kc.org.as_str()) {
            parts.push(format!("{} <=> ?", qi(&kc.org)));
            params.push(row[k].clone());
        }
    }
    if parts.is_empty() {
        return None;
    }
    let sql = format!(
        "SELECT HEX({}) FROM {}.{} WHERE {} LIMIT 1",
        qi(&col.org),
        qi(&col.schema),
        qi(&col.table),
        parts.join(" AND ")
    );
    Some(Stmt::with(sql, params))
}

struct ResView {
    title: String,
    rs: ResultSet,
    grid: GridState,
}

pub struct QueryTab {
    pub id: u64,
    pub title: String,
    pub db: Option<String>,
    worker: Worker,
    pub sql: String,
    results: Vec<ResView>,
    messages: String,
    active: usize, // 0 = Meldungen, 1.. = Ergebnisse
    tag: u64,
    running: bool,
    started: Instant,
    continue_on_error: bool,
    max_rows: usize,
    last_selection: Option<(usize, usize)>,
    cell: CellView,
    /// Nachladen eines gekürzten Binärwerts für den Detailbereich.
    tag_blob: u64,
    blob_key: Option<CellKey>,
    /// Name der Verbindung (Ablageort gespeicherter Abfragen).
    pub conn_name: String,
    /// Gespeichert als (Datenbank, Name).
    pub saved: Option<(String, String)>,
    pub saved_sql: String,
    /// Speichern-Dialog: (Name, Datenbank)
    save_dlg: Option<(String, String)>,
    just_saved: Option<String>,
    export_dlg: Option<ExportDialog>,
    note: String,
}

impl QueryTab {
    pub fn new(id: u64, cfg: &ConnConfig, ctx: &egui::Context, db: Option<String>, sql: String, max_rows: usize) -> Self {
        Self {
            id,
            title: format!("Abfrage {id}"),
            db,
            worker: Worker::spawn(cfg.clone(), ctx.clone()),
            sql,
            results: Vec::new(),
            messages: String::new(),
            active: 0,
            tag: 0,
            running: false,
            started: Instant::now(),
            continue_on_error: false,
            max_rows,
            last_selection: None,
            cell: CellView::default(),
            tag_blob: 0,
            blob_key: None,
            conn_name: cfg.name.clone(),
            saved: None,
            saved_sql: String::new(),
            save_dlg: None,
            just_saved: None,
            export_dlg: None,
            note: String::new(),
        }
    }

    /// Ungespeicherte Änderungen an einer gespeicherten Abfrage?
    pub fn dirty(&self) -> bool {
        self.saved.is_some() && self.sql != self.saved_sql
    }

    /// Liefert einmalig die Datenbank, unter der gerade gespeichert wurde.
    pub fn take_saved(&mut self) -> Option<String> {
        self.just_saved.take()
    }

    fn save(&mut self, as_new: bool) {
        match self.saved.clone() {
            Some((db, name)) if !as_new => self.write(&db, &name),
            other => {
                let (db, name) = other.unwrap_or_else(|| (self.db.clone().unwrap_or_default(), String::new()));
                self.save_dlg = Some((name, db));
            }
        }
    }

    fn write(&mut self, db: &str, name: &str) {
        match queries::save(&self.conn_name, db, name, &self.sql) {
            Ok(()) => {
                self.saved = Some((db.to_string(), name.to_string()));
                self.saved_sql = self.sql.clone();
                self.title = name.to_string();
                self.just_saved = Some(db.to_string());
                self.note = format!("Gespeichert: {db} / {name}");
            }
            Err(e) => self.note = format!("Speichern fehlgeschlagen: {e}"),
        }
    }

    fn execute(&mut self, text: &str) {
        let stmts: Vec<Stmt> = split_sql(text).into_iter().map(Stmt::plain).collect();
        if stmts.is_empty() {
            return;
        }
        let mode = if self.continue_on_error { Mode::Continue } else { Mode::StopOnError };
        self.tag = self.worker.send(self.db.clone(), stmts, mode, self.max_rows);
        self.running = true;
        self.started = Instant::now();
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Some(res) = self.worker.poll() {
            if res.tag == self.tag_blob {
                let Some(key) = self.blob_key.take() else { continue };
                let err = res.error.clone().or_else(|| res.outs.iter().find_map(|o| o.error.clone()));
                match (err, res.set(0).and_then(|s| s.rows.first())) {
                    (Some(e), _) => self.cell.set_error(key, format!("Nachladen fehlgeschlagen: {e}")),
                    (None, Some(row)) => {
                        let hex = row[0].clone().unwrap_or_default();
                        self.cell.set_full(ctx, key, cellview::decode_hex(&hex));
                    }
                    (None, None) => self.cell.set_error(key, "Nachladen fehlgeschlagen: Zeile nicht mehr gefunden.".into()),
                }
                continue;
            }
            if res.tag != self.tag {
                continue;
            }
            self.running = false;
            self.results.clear();
            self.cell = CellView::default();
            self.blob_key = None;
            let mut msg = String::new();
            for o in res.outs {
                let head: String = o.sql.chars().take(300).collect();
                msg.push_str(&head);
                if o.sql.chars().count() > 300 {
                    msg.push('…');
                }
                msg.push('\n');
                match &o.error {
                    Some(e) => msg.push_str(&format!("> FEHLER: {e}\n")),
                    None => {
                        if o.sets.is_empty() {
                            msg.push_str(&format!("> Betroffene Zeilen: {}\n", o.affected));
                            if let Some(id) = o.last_insert_id {
                                msg.push_str(&format!("> Letzte Insert-ID: {id}\n"));
                            }
                        } else {
                            for s in &o.sets {
                                msg.push_str(&format!(
                                    "> {} Zeile(n){}\n",
                                    s.rows.len(),
                                    if s.truncated { " (gekürzt auf Maximum)" } else { "" }
                                ));
                            }
                        }
                        if o.warnings > 0 {
                            msg.push_str(&format!("> Warnungen: {}\n", o.warnings));
                        }
                        if !o.info.is_empty() {
                            msg.push_str(&format!("> {}\n", o.info));
                        }
                    }
                }
                msg.push_str(&format!("> Zeit: {}\n\n", fmt_dur(o.elapsed)));
                for rs in o.sets {
                    let n = self.results.len() + 1;
                    self.results.push(ResView { title: format!("Ergebnis {n}"), rs, grid: GridState::default() });
                }
            }
            if let Some(e) = res.error {
                if !msg.contains(&e) {
                    msg.push_str(&format!("FEHLER: {e}\n"));
                }
            }
            msg.push_str(&format!("Gesamtzeit: {}", fmt_dur(res.elapsed)));
            let had_error = msg.contains("FEHLER");
            self.messages = msg;
            self.active = if had_error || self.results.is_empty() { 0 } else { 1 };
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, dbs: &[String]) {
        self.poll(&ui.ctx().clone());

        // Vor dem Editor konsumieren, damit Strg+Enter keinen Zeilenumbruch einfügt.
        let (run_all, run_sel) = ui.input_mut(|i| {
            let ctrl_enter = i.consume_key(Modifiers::COMMAND, Key::Enter);
            let ctrl_shift_r = i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::R);
            let ctrl_r = i.consume_key(Modifiers::COMMAND, Key::R);
            let f9 = i.consume_key(Modifiers::NONE, Key::F9);
            (ctrl_r || f9, ctrl_enter || ctrl_shift_r)
        });
        if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::S)) {
            self.save(false);
        }

        ui.horizontal(|ui| {
            let busy = self.running;
            if ui.add_enabled(!busy, icons::button(Icon::Run, "Ausführen")).on_hover_text("Strg+R / F9").clicked() || (run_all && !busy) {
                let sql = self.sql.clone();
                self.execute(&sql);
            }
            let sel = self.selected_text();
            let has_sel = sel.as_ref().is_some_and(|s| !s.trim().is_empty());
            let label = if has_sel { "Auswahl ausführen" } else { "Aktuelles Statement" };
            if ui.add_enabled(!busy, icons::button(Icon::Run, label)).on_hover_text("Strg+Enter").clicked() || (run_sel && !busy) {
                let text = if has_sel { sel.unwrap_or_default() } else { self.current_statement() };
                self.execute(&text);
            }
            if ui.add_enabled(busy, egui::Button::new("⏹ Stopp")).clicked() {
                self.worker.cancel();
            }
            if busy {
                ui.spinner();
                ui.label(fmt_dur(self.started.elapsed()));
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
            ui.separator();
            ui.label("Datenbank:");
            let cur = self.db.clone().unwrap_or_else(|| "(keine)".into());
            egui::ComboBox::from_id_salt(("qdb", self.id)).selected_text(cur).width(160.0).show_ui(ui, |ui| {
                for d in dbs {
                    if ui.selectable_label(self.db.as_deref() == Some(d.as_str()), d.as_str()).clicked() {
                        self.db = Some(d.clone());
                    }
                }
            });
            ui.separator();
            ui.checkbox(&mut self.continue_on_error, "Bei Fehler fortfahren");
            ui.separator();
            if ui.button("Formatieren").on_hover_text("Schlüsselwörter in Großbuchstaben").clicked() {
                self.sql = uppercase_keywords(&self.sql);
            }
            ui.separator();
            if ui.add(icons::button(Icon::Save, "Speichern")).on_hover_text("Strg+S").clicked() {
                self.save(false);
            }
            if ui.add_enabled(self.saved.is_some(), egui::Button::new("Speichern unter …")).clicked() {
                self.save(true);
            }
            let res = self.active.checked_sub(1).and_then(|i| self.results.get(i));
            if ui.add_enabled(res.is_some(), icons::button(Icon::Export, "Exportieren")).on_hover_text("Aktuelles Ergebnis als CSV").clicked() {
                if let Some(r) = res {
                    let note = r.rs.truncated.then(|| format!("Das Ergebnis wurde auf {} Zeilen begrenzt (Einstellung „Max. Zeilen“).", r.rs.rows.len()));
                    self.export_dlg = Some(ExportDialog::new(&self.title, note));
                }
            }
            if !self.note.is_empty() {
                ui.weak(&self.note);
            }
        });

        // Speichern-Dialog
        if let Some((name, db)) = self.save_dlg.as_mut() {
            let mut choice = 0;
            egui::Modal::new(egui::Id::unique(("qsave", self.id))).show(ui.ctx(), |ui| {
                ui.heading("Abfrage speichern");
                egui::Grid::new("qsave_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Name:");
                    let r = ui.add(egui::TextEdit::singleline(name).desired_width(260.0));
                    if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && !name.trim().is_empty() && !db.is_empty() {
                        choice = 1;
                    }
                    ui.end_row();
                    ui.label("Datenbank:");
                    egui::ComboBox::from_id_salt("qsave_db").selected_text(db.as_str()).width(260.0).show_ui(ui, |ui| {
                        for d in dbs {
                            ui.selectable_value(db, d.clone(), d.as_str());
                        }
                    });
                    ui.end_row();
                });
                if !name.trim().is_empty() && queries::exists(&self.conn_name, db, name.trim()) {
                    ui.colored_label(ui.visuals().warn_fg_color, "Eine Abfrage mit diesem Namen existiert bereits und wird überschrieben.");
                }
                ui.horizontal(|ui| {
                    if ui.add_enabled(!name.trim().is_empty() && !db.is_empty(), egui::Button::new("Speichern")).clicked() {
                        choice = 1;
                    }
                    if ui.button("Abbrechen").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        choice = 2;
                    }
                });
            });
            if choice == 1 {
                let (name, db) = self.save_dlg.take().unwrap();
                self.write(&db, name.trim());
            } else if choice == 2 {
                self.save_dlg = None;
            }
        }

        // Export-Dialog
        if let Some(dlg) = self.export_dlg.as_mut() {
            if let Some(r) = dlg.show(ui.ctx(), ("query", self.id)) {
                self.export_dlg = None;
                if let (Some((path, bom)), Some(res)) = (r, self.active.checked_sub(1).and_then(|i| self.results.get(i))) {
                    self.note = match export::write_result(&res.rs, &path, bom) {
                        Ok(n) => format!("{n} Zeilen exportiert: {}", path.display()),
                        Err(e) => format!("Export fehlgeschlagen: {e}"),
                    };
                }
            }
        }

        let editor_id = egui::Id::unique(("qedit", self.id));
        egui::Panel::top(egui::Id::unique(("qtop", self.id)))
            .resizable(true)
            .default_size(260.0)
            .min_size(80.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt(("qscroll", self.id)).auto_shrink([false, false]).show(ui, |ui| {
                    let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, w: f32| sqlhl::layouter(ui, buf, w);
                    let out = egui::TextEdit::multiline(&mut self.sql)
                        .id(editor_id)
                        .code_editor()
                        .desired_width(f32::INFINITY)
                        .desired_rows(12)
                        .lock_focus(true)
                        .min_size(egui::vec2(0.0, ui.available_height()))
                        .layouter(&mut layouter)
                        .show(ui);
                    if let Some(r) = out.cursor_range {
                        let rg = r.as_sorted_char_range();
                        self.last_selection = Some((rg.start.0, rg.end.0));
                    }
                });
            });

        // Ergebnisbereich
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.active, 0, "Meldungen");
            for (i, r) in self.results.iter().enumerate() {
                let t = format!("{} ({})", r.title, r.rs.rows.len());
                ui.selectable_value(&mut self.active, i + 1, t);
            }
        });
        ui.separator();
        if self.active == 0 {
            egui::ScrollArea::both().id_salt(("qmsg", self.id)).auto_shrink([false, false]).show(ui, |ui| {
                let font = egui::TextStyle::Monospace.resolve(ui.style());
                let (txt, weak, err) = (ui.visuals().text_color(), ui.visuals().weak_text_color(), ui.visuals().error_fg_color);
                let mut job = egui::text::LayoutJob::default();
                for line in self.messages.lines() {
                    let c = if line.contains("FEHLER") {
                        err
                    } else if line.starts_with("> ") || line.starts_with("Gesamtzeit") {
                        weak
                    } else {
                        txt
                    };
                    job.append(line, 0.0, egui::TextFormat::simple(font.clone(), c));
                    job.append("\n", 0.0, egui::TextFormat::simple(font.clone(), c));
                }
                ui.add(egui::Label::new(job).selectable(true));
            });
        } else if let Some(r) = self.results.get_mut(self.active - 1) {
            // Detailbereich für mehrzeilige/binäre Zellen
            let sel = r.grid.sel.filter(|&(row, c)| row < r.rs.rows.len() && c < r.rs.columns.len());
            let (value, binary, col) = match sel {
                Some((row, c)) => (r.rs.rows[row][c].clone(), r.rs.columns[c].binary, r.rs.columns[c].name.clone()),
                None => (None, false, String::new()),
            };
            let ctx = ui.ctx().clone();
            let can_fetch = sel.is_some_and(|(_, c)| !r.rs.columns[c].table.is_empty());
            let key = sel.map(|(row, c)| (self.active, row, c));
            if self.cell.update(&ctx, key, value.as_deref(), binary, &col, can_fetch) {
                if let (Some(key), Some((row, c))) = (key, sel) {
                    match blob_fetch_stmt(&r.rs, row, c) {
                        Some(stmt) => {
                            self.tag_blob = self.worker.send(None, vec![stmt], Mode::StopOnError, 1);
                            self.blob_key = Some(key);
                        }
                        None => self.cell.set_error(key, "Nachladen nicht möglich: keine Vergleichsspalten der Ursprungstabelle im Ergebnis.".into()),
                    }
                }
            }
            self.cell.ui(ui, ("query", self.id));
            let id = format!("qres_{}_{}", self.id, self.active);
            grid::show(ui, &id, &r.rs, &mut r.grid, None, None);
            if r.grid.sel != sel {
                ctx.request_repaint();
            }
        }
    }

    fn selected_text(&self) -> Option<String> {
        let (a, b) = self.last_selection?;
        if a == b {
            return None;
        }
        Some(self.sql.chars().skip(a).take(b - a).collect())
    }

    /// Statement unter dem Cursor.
    fn current_statement(&self) -> String {
        let cursor_char = self.last_selection.map(|s| s.0).unwrap_or(0);
        let cursor: usize = self.sql.char_indices().nth(cursor_char).map(|(i, _)| i).unwrap_or(self.sql.len());
        let stmts = split_sql(&self.sql);
        // Statement-Positionen im Text suchen
        let mut pos = 0;
        let mut best = stmts.first().cloned().unwrap_or_default();
        for s in &stmts {
            if let Some(p) = self.sql[pos..].find(s.as_str()) {
                let start = pos + p;
                let end = start + s.len();
                best = s.clone();
                if cursor <= end + 1 {
                    break;
                }
                pos = end;
            }
        }
        best
    }
}

fn uppercase_keywords(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let b = sql.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\'' || c == b'"' || c == b'`' {
            let s = i;
            i += 1;
            while i < b.len() && b[i] != c {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(b.len());
            out.push_str(&sql[s..i]);
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let s = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let w = &sql[s..i];
            if sqlhl::is_keyword(w) {
                out.push_str(&w.to_ascii_uppercase());
            } else {
                out.push_str(w);
            }
        } else if c == b'-' && b.get(i + 1) == Some(&b'-') {
            let s = i;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            out.push_str(&sql[s..i]);
        } else {
            let s = i;
            i += 1;
            while i < b.len() && !sql.is_char_boundary(i) {
                i += 1;
            }
            out.push_str(&sql[s..i]);
        }
    }
    out
}

#[cfg(test)]
mod blob_tests {
    use super::*;
    use crate::db::Col;

    #[test]
    fn blob_fetch_where() {
        let col = |name: &str, table: &str, org: &str, binary: bool| Col {
            name: name.into(),
            binary,
            schema: if table.is_empty() { String::new() } else { "ds".into() },
            table: table.into(),
            org: org.into(),
            ..Default::default()
        };
        let rs = ResultSet {
            columns: vec![
                col("id", "elements", "t_indx", false),
                col("code", "elements", "t_code", true),
                col("x", "", "", false),
                col("other", "users", "u_id", false),
            ],
            rows: vec![vec![Some("57".into()), Some("0x52…".into()), Some("1".into()), None]],
            truncated: false,
        };
        let s = blob_fetch_stmt(&rs, 0, 1).unwrap();
        assert_eq!(s.sql, "SELECT HEX(`t_code`) FROM `ds`.`elements` WHERE `t_indx` <=> ? LIMIT 1");
        assert_eq!(s.params, Some(vec![Some("57".into())]));
        // berechnete Spalte: keine Ursprungstabelle
        assert!(blob_fetch_stmt(&rs, 0, 2).is_none());
    }
}
