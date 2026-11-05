//! SQL-Editor-Tab mit Highlighting, Ausführen (alles/Auswahl), Ergebnis-Tabs.

use crate::cellview::{self, CellKey, CellView};
use crate::export::{self, ExportDialog};
use crate::queries;
use crate::icons::{self, Icon};
use crate::datatab::fmt_dur;
use crate::db::{qi, split_sql, ConnConfig, Mode, ResultSet, Stmt, StmtOut, Worker};
use crate::grid::{self, EditTarget, GridEdits, GridState};
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
    edits: GridEdits,
    /// Ursprungstabelle (Datenbank, Tabelle) oder Grund, warum das Ergebnis schreibgeschützt ist.
    target: Result<(String, String), String>,
    /// Originalspaltenname je Ergebnisspalte (Aliasse aufgelöst).
    names: Vec<String>,
    /// Spalten für den Vollvergleich ohne Schlüssel: nicht binär, aus der Ursprungstabelle, je
    /// Originalspalte nur das erste Vorkommen. Nur diese Spalten sind bearbeitbar.
    cmp_cols: Vec<usize>,
    key_cols: Vec<usize>,
    key_desc: String,
    /// Schlüsselabfrage (`SHOW KEYS`) läuft noch; bis dahin schreibgeschützt.
    tag_keys: u64,
    keys_loaded: bool,
}

impl ResView {
    fn new(title: String, rs: ResultSet) -> Self {
        let mut target: Option<(&str, &str)> = None;
        let mut multi = false;
        for c in &rs.columns {
            if c.table.is_empty() || c.org.is_empty() {
                continue;
            }
            match target {
                None => target = Some((&c.schema, &c.table)),
                Some(t) if t == (c.schema.as_str(), c.table.as_str()) => {}
                Some(_) => multi = true,
            }
        }
        let names: Vec<String> = rs.columns.iter().map(|c| c.org.clone()).collect();
        let mut cmp_cols = Vec::new();
        let mut edits = GridEdits::default();
        edits.clear(rs.rows.len());
        let target = match target {
            _ if multi => Err("schreibgeschützt – Spalten aus mehreren Tabellen".to_string()),
            None => Err("schreibgeschützt – keine Ursprungstabelle".to_string()),
            Some((s, t)) => {
                let mut seen = HashSet::new();
                for (k, c) in rs.columns.iter().enumerate() {
                    if !c.binary && !c.org.is_empty() && c.table == t && seen.insert(c.org.to_lowercase()) {
                        cmp_cols.push(k);
                    }
                }
                if cmp_cols.is_empty() {
                    Err("schreibgeschützt – keine vergleichbaren Spalten".to_string())
                } else {
                    Ok((s.to_string(), t.to_string()))
                }
            }
        };
        edits.readonly_cols = (0..rs.columns.len()).filter(|c| !cmp_cols.contains(c)).collect();
        edits.allow_insert = target.is_ok();
        Self {
            title,
            rs,
            grid: GridState::default(),
            edits,
            target,
            names,
            cmp_cols,
            key_cols: Vec::new(),
            key_desc: String::new(),
            tag_keys: 0,
            keys_loaded: false,
        }
    }

    /// Qualifizierter Name der Ursprungstabelle.
    fn fq(&self) -> Option<String> {
        let (s, t) = self.target.as_ref().ok()?;
        Some(if s.is_empty() { qi(t) } else { format!("{}.{}", qi(s), qi(t)) })
    }

    fn editable(&self) -> bool {
        self.target.is_ok() && self.keys_loaded
    }

    fn dirty(&self) -> bool {
        self.edits.is_dirty(self.rs.rows.len())
    }

    /// Offene Änderungen inkl. leerer neuer Zeilen (Übernehmen/Verwerfen möglich).
    fn pending(&self) -> bool {
        self.edits.has_pending(self.rs.rows.len())
    }

    fn add_row(&mut self, c: usize) {
        self.grid.editing = None;
        self.rs.rows.push(vec![None; self.rs.columns.len()]);
        let r = self.rs.rows.len() - 1;
        self.grid.sel = Some((r, c.min(self.rs.columns.len().saturating_sub(1))));
        self.grid.scroll_to = Some(r);
    }

    fn set_keys(&mut self, keys: &ResultSet) {
        let (names, cmp) = (&self.names, &self.cmp_cols);
        (self.key_cols, self.key_desc) =
            grid::pick_key_cols(keys, |n| cmp.iter().copied().find(|&c| names[c].eq_ignore_ascii_case(n)));
        if self.key_cols.is_empty() {
            self.key_desc = "keiner – Vollvergleich über die Spalten im Ergebnis (LIMIT 1)".into();
        }
        self.keys_loaded = true;
    }

    fn stmts(&self) -> Vec<Stmt> {
        let Some(fq) = self.fq() else { return Vec::new() };
        EditTarget { fq: &fq, names: &self.names, key_cols: &self.key_cols, cmp_cols: &self.cmp_cols }
            .stmts(&self.rs, &self.edits)
    }

    /// Übernommene Änderungen in die angezeigten Daten einspielen (die Abfrage wird nicht neu
    /// ausgeführt). Doppelt angezeigte Spalten derselben Originalspalte werden mit aktualisiert.
    /// `outs` sind die Ergebnisse der Statements aus `stmts`: Bei neuen Zeilen wird ein einspaltiger,
    /// nicht angegebener Schlüssel mit der vergebenen Insert-ID (AUTO_INCREMENT) gefüllt.
    fn apply_edits(&mut self, outs: &[StmtOut]) {
        let ncols = self.rs.columns.len();
        let ins = self.edits.insert_rows(self.rs.rows.len(), ncols);
        let ins_outs = &outs[outs.len().saturating_sub(ins.len())..];
        if let ([k], true) = (self.key_cols.as_slice(), ins_outs.len() == ins.len()) {
            for (&r, o) in ins.iter().zip(ins_outs) {
                if let (Some(id), false) = (o.last_insert_id, self.edits.changed.contains_key(&(r, *k))) {
                    self.edits.changed.insert((r, *k), Some(id.to_string()));
                }
            }
        }
        // Gelöschte und leer gebliebene neue Zeilen entfernen
        let mut del: Vec<usize> = self.edits.deleted.drain().collect();
        del.extend((self.edits.new_from..self.rs.rows.len()).filter(|r| self.edits.is_empty_new(*r, ncols)));
        let changed: Vec<((usize, usize), Option<String>)> = self.edits.changed.drain().collect();
        for ((r, c), v) in changed {
            let Some(row) = self.rs.rows.get_mut(r) else { continue };
            for k in 0..self.rs.columns.len() {
                let col = &self.rs.columns[k];
                if k == c || (!col.binary && col.table == self.rs.columns[c].table && self.names[k].eq_ignore_ascii_case(&self.names[c])) {
                    row[k] = v.clone();
                }
            }
        }
        del.sort_unstable_by(|a, b| b.cmp(a));
        del.dedup();
        for r in del {
            if r < self.rs.rows.len() {
                self.rs.rows.remove(r);
            }
        }
        // Eingefügte Zeilen gehören jetzt zum Bestand
        self.reset_edits();
    }

    fn reset_edits(&mut self) {
        self.edits.clear(self.rs.rows.len());
        self.grid.editing = None;
        if self.grid.sel.is_some_and(|(r, _)| r >= self.rs.rows.len()) {
            self.grid.sel = None;
        }
    }

    fn discard(&mut self) {
        self.rs.rows.truncate(self.edits.new_from);
        self.reset_edits();
    }
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
    /// Schreiben der Änderungen eines Ergebnisses (Index in `results`).
    tag_commit: u64,
    commit_view: usize,
    /// Meldung zum Übernehmen (Text, Fehler?)
    edit_msg: Option<(String, bool)>,
    /// Ausführen trotz ungespeicherter Änderungen bestätigen: auszuführender Text.
    confirm_exec: Option<String>,
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
            tag_commit: 0,
            commit_view: 0,
            edit_msg: None,
            confirm_exec: None,
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
        if self.results.iter().any(|v| v.dirty()) {
            self.confirm_exec = Some(text.to_string());
            return;
        }
        self.run(text);
    }

    /// Änderungen des Ergebnisses `i` in einer Transaktion schreiben.
    fn commit(&mut self, i: usize) {
        let Some(v) = self.results.get_mut(i) else { return };
        if let Some((r, c, buf, _)) = v.grid.editing.take() {
            v.edits.changed.insert((r, c), Some(buf));
        }
        let stmts = v.stmts();
        if stmts.is_empty() {
            v.discard();
            return;
        }
        self.tag_commit = self.worker.send(self.db.clone(), stmts, Mode::Transaction, 0);
        self.commit_view = i;
        self.edit_msg = None;
        self.running = true;
        self.started = Instant::now();
    }

    fn run(&mut self, text: &str) {
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
            if let Some(v) = self.results.iter_mut().find(|v| v.tag_keys != 0 && v.tag_keys == res.tag) {
                // Fehler (z. B. keine Rechte) → ohne Schlüssel per Vollvergleich
                v.set_keys(&res.set(0).cloned().unwrap_or_default());
                continue;
            }
            if res.tag == self.tag_commit {
                self.running = false;
                let Some(v) = self.results.get_mut(self.commit_view) else { continue };
                match res.error {
                    Some(e) => self.edit_msg = Some((format!("Übernehmen fehlgeschlagen: {e}"), true)),
                    None => {
                        let n: u64 = res.outs.iter().map(|o| o.affected).sum();
                        let missing = res.outs.iter().filter(|o| o.affected == 0).count();
                        let mut m = format!("{n} Zeile(n) übernommen in {}", fmt_dur(res.elapsed));
                        if missing > 0 {
                            m.push_str(&format!(" – {missing} Statement(s) ohne Wirkung (Zeile nicht gefunden oder unverändert)"));
                        }
                        self.edit_msg = Some((m, missing > 0));
                        v.apply_edits(&res.outs);
                        self.cell = CellView::default();
                        self.blob_key = None;
                    }
                }
                continue;
            }
            if res.tag != self.tag {
                continue;
            }
            self.running = false;
            self.results.clear();
            self.edit_msg = None;
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
                    self.results.push(ResView::new(format!("Ergebnis {n}"), rs));
                }
            }
            // Schlüssel der Ursprungstabellen holen, damit die Ergebnisse bearbeitet werden können
            for v in &mut self.results {
                if let Some(fq) = v.fq() {
                    v.tag_keys = self.worker.send(self.db.clone(), vec![Stmt::plain(format!("SHOW KEYS FROM {fq}"))], Mode::Continue, 10_000);
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

        // Rückfrage: Ausführen trotz ungespeicherter Änderungen an Ergebnissen
        if self.confirm_exec.is_some() {
            let mut choice = 0;
            egui::Modal::new(ui.scope_id().with("qconfirm_exec")).show(ui.ctx(), |ui| {
                ui.heading("Ungespeicherte Änderungen");
                ui.label("Im Ergebnis gibt es Änderungen, die noch nicht übernommen wurden.
Beim Ausführen gehen sie verloren.");
                ui.horizontal(|ui| {
                    if ui.button("Verwerfen und ausführen").clicked() {
                        choice = 1;
                    }
                    if ui.button("Abbrechen").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        choice = 2;
                    }
                });
            });
            if choice == 1 {
                if let Some(text) = self.confirm_exec.take() {
                    self.run(&text);
                }
            } else if choice == 2 {
                self.confirm_exec = None;
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
        let (mut do_commit, mut do_discard, mut do_add) = (false, false, false);
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.active, 0, "Meldungen");
            for (i, r) in self.results.iter().enumerate() {
                let t = format!("{} ({}){}", r.title, r.rs.rows.len(), if r.dirty() { " •" } else { "" });
                ui.selectable_value(&mut self.active, i + 1, t);
            }
            if let Some(r) = self.active.checked_sub(1).and_then(|i| self.results.get(i)) {
                ui.separator();
                match &r.target {
                    Err(why) => {
                        ui.weak(why);
                    }
                    Ok(_) if !r.keys_loaded => {
                        ui.spinner();
                        ui.weak("Schlüssel werden ermittelt …");
                    }
                    Ok(_) => {
                        let dirty = r.pending();
                        ui.add_enabled_ui(!self.running, |ui| {
                            do_add = ui.add(icons::button(Icon::Add, "Neu")).on_hover_text("Neue Zeile (Einfg, oder ↓ in der letzten Zeile)").clicked();
                            do_commit = ui.add_enabled(dirty, egui::Button::new("✔ Übernehmen")).on_hover_text("Änderungen in einer Transaktion schreiben").clicked();
                            do_discard = ui.add_enabled(dirty, egui::Button::new("✖ Verwerfen")).on_hover_text("Esc").clicked();
                        });
                        ui.weak(format!("Schlüssel: {}", r.key_desc));
                    }
                }
                if let Some((m, err)) = &self.edit_msg {
                    ui.separator();
                    if *err {
                        ui.colored_label(ui.visuals().error_fg_color, m);
                    } else {
                        ui.weak(m);
                    }
                }
            }
        });
        if let Some(i) = self.active.checked_sub(1) {
            // Esc im Grid (ohne offenen Zelleditor) verwirft die Änderungen
            let esc = ui.input(|inp| inp.key_pressed(Key::Escape));
            if let Some(r) = self.results.get(i) {
                if esc && r.grid.editing.is_none() && r.grid.focused && r.pending() && !self.running {
                    do_discard = true;
                }
            }
            if do_add {
                if let Some(r) = self.results.get_mut(i) {
                    r.add_row(0);
                }
            }
            if do_commit {
                self.commit(i);
            } else if do_discard {
                if let Some(r) = self.results.get_mut(i) {
                    r.discard();
                }
                self.edit_msg = None;
            }
        }
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
                Some((row, c)) => (
                    grid::cell_value(&r.rs, Some(&r.edits), row, c).cloned().flatten(),
                    r.rs.columns[c].binary,
                    r.rs.columns[c].name.clone(),
                ),
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
            let edits = (r.editable() && !self.running).then_some(&mut r.edits);
            let out = grid::show(ui, &id, &r.rs, &mut r.grid, edits, None);
            if let Some(c) = out.add_row {
                r.add_row(c);
            }
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

#[cfg(test)]
mod edit_tests {
    use super::*;
    use crate::db::Col;

    fn col(name: &str, table: &str, org: &str, binary: bool) -> Col {
        Col {
            name: name.into(),
            binary,
            schema: if table.is_empty() { String::new() } else { "shop".into() },
            table: table.into(),
            org: org.into(),
            ..Default::default()
        }
    }

    fn view(columns: Vec<Col>) -> ResView {
        let rows = vec![(0..columns.len()).map(|i| Some(i.to_string())).collect()];
        ResView::new("Ergebnis 1".into(), ResultSet { columns, rows, truncated: false })
    }

    #[test]
    fn edit_target() {
        // Alias, berechnete Spalte, doppelte und binäre Spalte → nur id und n bearbeitbar
        let mut v = view(vec![
            col("id", "kunden", "id", false),
            col("n", "kunden", "name", false),
            col("x", "", "", false),
            col("id2", "kunden", "id", false),
            col("bild", "kunden", "bild", true),
        ]);
        assert_eq!(v.target, Ok(("shop".into(), "kunden".into())));
        assert_eq!(v.cmp_cols, vec![0, 1]);
        assert_eq!(v.edits.readonly_cols, [2, 3, 4].into_iter().collect());
        assert!(!v.editable());
        v.set_keys(&ResultSet::default());
        assert!(v.editable() && v.key_cols.is_empty());

        v.edits.changed.insert((0, 0), Some("9".into()));
        let s = v.stmts();
        assert_eq!(s[0].sql, "UPDATE `shop`.`kunden` SET `id` = ? WHERE `id` <=> ? AND `name` <=> ? LIMIT 1");
        // Übernehmen aktualisiert auch die doppelt angezeigte Spalte
        v.apply_edits(&[]);
        assert_eq!(v.rs.rows[0][0].as_deref(), Some("9"));
        assert_eq!(v.rs.rows[0][3].as_deref(), Some("9"));
        assert!(!v.dirty());

        // Neue Zeilen: leere wird übergangen, ausgefüllte per INSERT, ID aus AUTO_INCREMENT
        let keys = ResultSet {
            columns: ["Key_name", "Non_unique", "Seq_in_index", "Column_name"]
                .iter()
                .map(|n| Col { name: (*n).into(), ..Default::default() })
                .collect(),
            rows: vec![vec![Some("PRIMARY".into()), Some("0".into()), Some("1".into()), Some("id".into())]],
            truncated: false,
        };
        v.set_keys(&keys);
        assert_eq!(v.key_cols, vec![0]);
        assert!(v.edits.allow_insert);
        v.add_row(1);
        assert!(!v.dirty() && v.pending());
        v.add_row(1);
        v.edits.changed.insert((2, 1), Some("Neu".into()));
        let s = v.stmts();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].sql, "INSERT INTO `shop`.`kunden` (`name`) VALUES (?)");
        let out = StmtOut { last_insert_id: Some(42), affected: 1, ..Default::default() };
        v.apply_edits(&[out]);
        assert_eq!(v.rs.rows.len(), 2);
        assert_eq!(v.rs.rows[1][0].as_deref(), Some("42"));
        assert_eq!(v.rs.rows[1][1].as_deref(), Some("Neu"));
        assert!(!v.pending());

        assert!(view(vec![col("a", "t1", "a", false), col("b", "t2", "b", false)]).target.is_err());
        assert!(view(vec![col("c", "", "", false)]).target.is_err());
    }
}
