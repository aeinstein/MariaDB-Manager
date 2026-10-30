//! Tabellendaten-Tab: Paging, Filter, Sortierung, Bearbeiten.

use crate::cellview::{self, CellKey, CellView};
use crate::export::{self, ExportDialog, ExportJob};
use crate::icons::{self, Icon};
use crate::db::{qi, ConnConfig, Mode, ResultSet, Stmt, Worker};
use crate::grid::{self, GridEdits, GridState};
use eframe::egui::{self, Key, RichText};
use std::time::Duration;

pub struct DataTab {
    pub db: String,
    pub table: String,
    pub is_view: bool,
    worker: Worker,
    cfg: ConnConfig,
    export_dlg: Option<ExportDialog>,
    export_job: Option<ExportJob>,
    tag_load: u64,
    tag_commit: u64,
    tag_count: u64,
    tag_blob: u64,
    blob_key: Option<CellKey>,
    cell: CellView,
    rs: ResultSet,
    key_cols: Vec<usize>,
    key_desc: String,
    grid: GridState,
    edits: GridEdits,
    filter: String,
    applied_filter: String,
    sort: Option<(String, bool)>,
    offset: usize,
    page_size: usize,
    loading: bool,
    error: Option<String>,
    status: String,
    total: Option<String>,
    confirm_discard: Option<Nav>,
}

#[derive(Clone, Copy)]
enum Nav {
    Reload,
    First,
    Prev,
    Next,
}

impl DataTab {
    pub fn new(cfg: &ConnConfig, ctx: &egui::Context, db: &str, table: &str, is_view: bool, page_size: usize) -> Self {
        let mut t = Self {
            db: db.into(),
            table: table.into(),
            is_view,
            worker: Worker::spawn(cfg.clone(), ctx.clone()),
            cfg: cfg.clone(),
            export_dlg: None,
            export_job: None,
            tag_load: 0,
            tag_commit: 0,
            tag_count: 0,
            tag_blob: 0,
            blob_key: None,
            cell: CellView::default(),
            rs: ResultSet::default(),
            key_cols: Vec::new(),
            key_desc: String::new(),
            grid: GridState::default(),
            edits: GridEdits::default(),
            filter: String::new(),
            applied_filter: String::new(),
            sort: None,
            offset: 0,
            page_size: page_size.max(1),
            loading: false,
            error: None,
            status: String::new(),
            total: None,
            confirm_discard: None,
        };
        t.load();
        t
    }

    pub fn title(&self) -> String {
        let dirty = if self.dirty() { " •" } else { "" };
        format!("{} @{}{}", self.table, self.db, dirty)
    }

    pub fn dirty(&self) -> bool {
        self.edits.is_dirty(self.rs.rows.len())
    }

    fn fq(&self) -> String {
        format!("{}.{}", qi(&self.db), qi(&self.table))
    }

    fn where_clause(&self) -> String {
        let f = self.applied_filter.trim();
        if f.is_empty() {
            String::new()
        } else {
            format!(" WHERE {f}")
        }
    }

    /// SELECT mit aktuellem Filter und Sortierung (ohne LIMIT).
    fn select_sql(&self) -> String {
        let mut sql = format!("SELECT * FROM {}{}", self.fq(), self.where_clause());
        if let Some((c, asc)) = &self.sort {
            sql.push_str(&format!(" ORDER BY {} {}", qi(c), if *asc { "ASC" } else { "DESC" }));
        }
        sql
    }

    fn load(&mut self) {
        let mut sql = self.select_sql();
        sql.push_str(&format!(" LIMIT {} OFFSET {}", self.page_size, self.offset));
        let keys = format!("SHOW KEYS FROM {}", self.fq());
        self.tag_load = self.worker.send(
            None,
            vec![Stmt::plain(sql), Stmt::plain(keys)],
            Mode::Continue,
            self.page_size,
        );
        self.loading = true;
        self.error = None;
    }

    fn count(&mut self) {
        let sql = format!("SELECT COUNT(*) FROM {}{}", self.fq(), self.where_clause());
        self.tag_count = self.worker.query(None, &[&sql]);
        self.total = Some("…".into());
    }

    fn navigate(&mut self, nav: Nav) {
        if self.dirty() {
            self.confirm_discard = Some(nav);
            return;
        }
        self.do_nav(nav);
    }

    fn do_nav(&mut self, nav: Nav) {
        match nav {
            Nav::Reload => {}
            Nav::First => self.offset = 0,
            Nav::Prev => self.offset = self.offset.saturating_sub(self.page_size),
            Nav::Next => self.offset += self.page_size,
        }
        self.load();
    }

    fn add_row(&mut self) {
        self.rs.rows.push(vec![None; self.rs.columns.len()]);
        let r = self.rs.rows.len() - 1;
        self.grid.sel = Some((r, 0));
        self.grid.scroll_to = Some(r);
    }

    fn discard(&mut self) {
        self.rs.rows.truncate(self.edits.new_from);
        self.edits.clear(self.rs.rows.len());
        self.grid.editing = None;
    }

    fn key_where(&self, r: usize, params: &mut Vec<Option<String>>) -> String {
        let cols: Vec<usize> = if self.key_cols.is_empty() {
            (0..self.rs.columns.len()).filter(|c| !self.rs.columns[*c].binary).collect()
        } else {
            self.key_cols.clone()
        };
        let mut parts = Vec::new();
        for c in cols {
            parts.push(format!("{} <=> ?", qi(&self.rs.columns[c].name)));
            params.push(self.rs.rows[r][c].clone());
        }
        let limit = if self.key_cols.is_empty() { " LIMIT 1" } else { "" };
        format!(" WHERE {}{}", parts.join(" AND "), limit)
    }

    /// Lädt einen (im Grid gekürzten) Binärwert vollständig als HEX nach.
    fn fetch_blob(&mut self, r: usize, c: usize) {
        let key = (0, r, c);
        if r >= self.edits.new_from || r >= self.rs.rows.len() {
            self.cell.set_error(key, "Neue Zeile – Inhalt noch nicht gespeichert.".into());
            return;
        }
        let mut p = Vec::new();
        let w = self.key_where(r, &mut p);
        let sql = format!("SELECT HEX({}) FROM {}{w}", qi(&self.rs.columns[c].name), self.fq());
        self.tag_blob = self.worker.send(None, vec![Stmt::with(sql, p)], Mode::StopOnError, 1);
        self.blob_key = Some(key);
    }

    fn commit(&mut self) {
        if let Some((r, c, buf, _)) = self.grid.editing.take() {
            self.edits.changed.insert((r, c), Some(buf));
        }
        let mut stmts = Vec::new();
        let fq = self.fq();
        let mut deleted: Vec<usize> = self.edits.deleted.iter().copied().collect();
        deleted.sort_unstable();
        for &r in &deleted {
            if r >= self.edits.new_from {
                continue;
            }
            let mut p = Vec::new();
            let w = self.key_where(r, &mut p);
            stmts.push(Stmt::with(format!("DELETE FROM {fq}{w}"), p));
        }
        let ncols = self.rs.columns.len();
        for r in 0..self.edits.new_from {
            if self.edits.deleted.contains(&r) {
                continue;
            }
            let changed: Vec<usize> = (0..ncols).filter(|c| self.edits.changed.contains_key(&(r, *c))).collect();
            if changed.is_empty() {
                continue;
            }
            let mut p = Vec::new();
            let sets: Vec<String> = changed
                .iter()
                .map(|c| {
                    p.push(self.edits.changed[&(r, *c)].clone());
                    format!("{} = ?", qi(&self.rs.columns[*c].name))
                })
                .collect();
            let w = self.key_where(r, &mut p);
            stmts.push(Stmt::with(format!("UPDATE {fq} SET {}{w}", sets.join(", ")), p));
        }
        for r in self.edits.new_from..self.rs.rows.len() {
            if self.edits.deleted.contains(&r) {
                continue;
            }
            let changed: Vec<usize> = (0..ncols).filter(|c| self.edits.changed.contains_key(&(r, *c))).collect();
            let names: Vec<String> = changed.iter().map(|c| qi(&self.rs.columns[*c].name)).collect();
            let qm: Vec<&str> = changed.iter().map(|_| "?").collect();
            let p: Vec<Option<String>> = changed.iter().map(|c| self.edits.changed[&(r, *c)].clone()).collect();
            stmts.push(Stmt::with(format!("INSERT INTO {fq} ({}) VALUES ({})", names.join(", "), qm.join(", ")), p));
        }
        if stmts.is_empty() {
            self.discard();
            return;
        }
        self.tag_commit = self.worker.send(None, stmts, Mode::Transaction, 0);
        self.loading = true;
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Some(mut res) = self.worker.poll() {
            if res.tag == self.tag_blob {
                let Some(key) = self.blob_key.take() else { continue };
                match res.error {
                    Some(e) => self.cell.set_error(key, format!("Nachladen fehlgeschlagen: {e}")),
                    None => {
                        let hex = res.set(0).and_then(|s| s.rows.first()).and_then(|r| r[0].clone()).unwrap_or_default();
                        self.cell.set_full(ctx, key, cellview::decode_hex(&hex));
                    }
                }
                continue;
            }
            if res.tag == self.tag_load {
                self.loading = false;
                // Neue Daten: alte Detailanzeige verwerfen
                self.cell = CellView::default();
                if let Some(err) = res.outs.first().and_then(|o| o.error.clone()).or(res.error.clone()) {
                    self.error = Some(err);
                    continue;
                }
                let keys = res.take_set(1).unwrap_or_default();
                if let Some(rs) = res.take_set(0) {
                    self.rs = rs;
                }
                self.edits.clear(self.rs.rows.len());
                self.edits.readonly_cols =
                    (0..self.rs.columns.len()).filter(|c| self.rs.columns[*c].binary).collect();
                self.pick_keys(&keys);
                self.grid.editing = None;
                if let Some((r, c)) = self.grid.sel {
                    if r >= self.rs.rows.len() || c >= self.rs.columns.len() {
                        self.grid.sel = None;
                    }
                }
                self.status = format!(
                    "{} Zeilen geladen in {}",
                    self.rs.rows.len(),
                    fmt_dur(res.elapsed)
                );
            } else if res.tag == self.tag_commit {
                self.loading = false;
                if let Some(e) = res.error {
                    self.error = Some(e);
                } else {
                    let n: u64 = res.outs.iter().map(|o| o.affected).sum();
                    self.status = format!("{n} Zeile(n) übernommen in {}", fmt_dur(res.elapsed));
                    self.load();
                }
            } else if res.tag == self.tag_count {
                self.total = Some(match res.error {
                    Some(_) => "?".into(),
                    None => res.set(0).map(|s| s.get(0, "COUNT(*)").to_string()).unwrap_or_default(),
                });
            }
        }
    }

    fn pick_keys(&mut self, keys: &ResultSet) {
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
        self.key_cols.clear();
        self.key_desc = "keiner – Änderungen per Vollvergleich (LIMIT 1)".into();
        if let Some(k) = best {
            let mut cols: Vec<(u32, String)> = (0..keys.rows.len())
                .filter(|r| keys.get(*r, "Key_name") == k)
                .map(|r| (keys.get(r, "Seq_in_index").parse().unwrap_or(0), keys.get(r, "Column_name").to_string()))
                .collect();
            cols.sort();
            let idx: Option<Vec<usize>> = cols.iter().map(|(_, n)| self.rs.col(n)).collect();
            if let Some(idx) = idx {
                self.key_cols = idx;
                self.key_desc = format!("{k} ({})", cols.iter().map(|c| c.1.as_str()).collect::<Vec<_>>().join(", "));
            }
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.poll(&ui.ctx().clone());

        // Tastenkürzel
        let (save, reload, esc) = ui.input(|i| {
            (
                i.modifiers.command && i.key_pressed(Key::S),
                i.key_pressed(Key::F5),
                i.key_pressed(Key::Escape),
            )
        });
        if save && self.dirty() {
            self.commit();
        }
        if reload {
            self.navigate(Nav::Reload);
        }
        if esc && self.grid.editing.is_none() && self.dirty() && ui.ctx().memory(|m| m.focused().is_none()) {
            self.discard();
        }

        let editable = !self.is_view;
        ui.horizontal(|ui| {
            ui.add_enabled_ui(editable && !self.loading, |ui| {
                if ui.add(icons::button(Icon::Add, "Neu")).on_hover_text("Neue Zeile").clicked() {
                    self.add_row();
                }
                let can_del = self.grid.sel.is_some();
                if ui.add_enabled(can_del, egui::Button::new("➖ Löschen")).on_hover_text("Ausgewählte Zeile zum Löschen markieren").clicked() {
                    if let Some((r, _)) = self.grid.sel {
                        if !self.edits.deleted.remove(&r) {
                            self.edits.deleted.insert(r);
                        }
                    }
                }
                let dirty = self.dirty();
                if ui.add_enabled(dirty, egui::Button::new("✔ Übernehmen")).on_hover_text("Strg+S").clicked() {
                    self.commit();
                }
                if ui.add_enabled(dirty, egui::Button::new("✖ Verwerfen")).on_hover_text("Esc").clicked() {
                    self.discard();
                }
            });
            ui.separator();
            if ui.add_enabled(!self.loading, egui::Button::new(icons::atom(Icon::Refresh, 16.0))).on_hover_text("Aktualisieren (F5)").clicked() {
                self.navigate(Nav::Reload);
            }
            if self.loading {
                ui.spinner();
                if ui.button("⏹").on_hover_text("Abbrechen").clicked() {
                    self.worker.cancel();
                }
            }
            if let Some(job) = &self.export_job {
                ui.spinner();
                ui.label(format!("Exportiere … {} Zeilen", job.rows()));
                if ui.button("Abbrechen").clicked() {
                    job.cancel();
                }
            } else if ui
                .add(icons::button(Icon::Export, "Exportieren"))
                .on_hover_text("Ganze Tabelle (mit Filter und Sortierung) als CSV")
                .clicked()
            {
                self.export_dlg = Some(ExportDialog::new(&self.table, None));
            }
            ui.separator();
            ui.label("Filter:");
            let fr = ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("WHERE-Bedingung, z. B. id > 100")
                    .desired_width(260.0)
                    .font(egui::TextStyle::Monospace),
            );
            let apply = fr.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if apply || ui.button("Anwenden").clicked() {
                self.applied_filter = self.filter.clone();
                self.offset = 0;
                self.total = None;
                self.navigate(Nav::Reload);
            }
            if !self.applied_filter.is_empty() && ui.button("✖").on_hover_text("Filter entfernen").clicked() {
                self.filter.clear();
                self.applied_filter.clear();
                self.offset = 0;
                self.total = None;
                self.navigate(Nav::Reload);
            }
        });

        // Unten: Seitennavigation + Status
        egui::Panel::bottom(egui::Id::unique(("data_status", &self.db, &self.table)))
            .show_separator_line(true)
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(4, 0)).fill(ui.visuals().panel_fill))
            .exact_size(26.0)
            .show(ui, |ui| {
                // Inhalt vertikal mittig in der festen Zeilenhöhe
                ui.horizontal_centered(|ui| {
                    let n = self.rs.rows.len().min(self.edits.new_from);
                    if ui.add_enabled(self.offset > 0, egui::Button::new("⏮")).clicked() {
                        self.navigate(Nav::First);
                    }
                    if ui.add_enabled(self.offset > 0, egui::Button::new("◀")).clicked() {
                        self.navigate(Nav::Prev);
                    }
                    let range = if n == 0 {
                        "keine Zeilen".to_string()
                    } else {
                        format!("Zeilen {}–{}", self.offset + 1, self.offset + n)
                    };
                    ui.label(range);
                    if ui.add_enabled(n >= self.page_size, egui::Button::new("▶")).clicked() {
                        self.navigate(Nav::Next);
                    }
                    ui.separator();
                    match &self.total {
                        Some(t) => {
                            ui.label(format!("Gesamt: {t}"));
                        }
                        None => {
                            if ui.small_button("Zählen").on_hover_text("SELECT COUNT(*) ausführen").clicked() {
                                self.count();
                            }
                        }
                    }
                    ui.separator();
                    ui.weak(format!("Schlüssel: {}", self.key_desc));
                    ui.separator();
                    ui.weak(&self.status);
                });
            });

        if let Some(e) = &self.error {
            let mut close = false;
            egui::Frame::new().fill(ui.visuals().error_fg_color.gamma_multiply(0.15)).inner_margin(6.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(ui.visuals().error_fg_color, RichText::new(e).monospace());
                    if ui.small_button("✖").clicked() {
                        close = true;
                    }
                });
            });
            if close {
                self.error = None;
            }
        }

        // Detailbereich für mehrzeilige/binäre Zellen (unten, vor dem Grid)
        let sel = self.grid.sel.filter(|&(r, c)| r < self.rs.rows.len() && c < self.rs.columns.len());
        let (value, binary, col) = match sel {
            Some((r, c)) => (
                grid::cell_value(&self.rs, Some(&self.edits), r, c).cloned().flatten(),
                self.rs.columns[c].binary,
                self.rs.columns[c].name.clone(),
            ),
            None => (None, false, String::new()),
        };
        let ctx = ui.ctx().clone();
        if self.cell.update(&ctx, sel.map(|(r, c)| (0, r, c)), value.as_deref(), binary, &col, true) {
            if let Some((r, c)) = sel {
                self.fetch_blob(r, c);
            }
        }
        self.cell.ui(ui, ("data", &self.db, &self.table));

        let sort_idx = self.sort.as_ref().and_then(|(n, a)| self.rs.col(n).map(|i| (i, *a)));
        let id = format!("data_{}_{}", self.db, self.table);
        let out = grid::show(
            ui,
            &id,
            &self.rs,
            &mut self.grid,
            if editable { Some(&mut self.edits) } else { None },
            sort_idx,
        );
        if self.grid.sel != sel {
            ctx.request_repaint();
        }
        if let Some(c) = out.sort_clicked {
            let name = self.rs.columns[c].name.clone();
            self.sort = match &self.sort {
                Some((n, true)) if *n == name => Some((name, false)),
                Some((n, false)) if *n == name => None,
                _ => Some((name, true)),
            };
            self.offset = 0;
            self.navigate(Nav::Reload);
        }
        if let Some((c, v)) = out.filter_by {
            let col = qi(&self.rs.columns[c].name);
            self.filter = match v {
                None => format!("{col} IS NULL"),
                Some(v) => format!("{col} = {}", crate::db::qs(&v)),
            };
            self.applied_filter = self.filter.clone();
            self.offset = 0;
            self.total = None;
            self.navigate(Nav::Reload);
        }

        // CSV-Export
        if let Some(dlg) = self.export_dlg.as_mut() {
            if let Some(r) = dlg.show(ui.ctx(), ("data", &self.db, &self.table)) {
                self.export_dlg = None;
                if let Some((path, bom)) = r {
                    self.export_job = Some(export::export_query(self.cfg.clone(), self.select_sql(), path, bom, ui.ctx().clone()));
                }
            }
        }
        if let Some(res) = self.export_job.as_ref().and_then(|j| j.poll()) {
            let path = self.export_job.take().map(|j| j.path).unwrap_or_default();
            match res {
                Ok(n) => self.status = format!("{n} Zeilen exportiert: {}", path.display()),
                Err(e) => self.error = Some(e),
            }
        }

        // Rückfrage bei ungespeicherten Änderungen
        if let Some(nav) = self.confirm_discard {
            let mut choice = 0;
            egui::Modal::new(egui::Id::unique(("discard", &id))).show(ui.ctx(), |ui| {
                ui.heading("Ungespeicherte Änderungen");
                ui.label("Es gibt Änderungen, die noch nicht übernommen wurden.");
                ui.horizontal(|ui| {
                    if ui.button("Übernehmen").clicked() {
                        choice = 1;
                    }
                    if ui.button("Verwerfen").clicked() {
                        choice = 2;
                    }
                    if ui.button("Abbrechen").clicked() {
                        choice = 3;
                    }
                });
            });
            match choice {
                1 => {
                    self.confirm_discard = None;
                    self.commit();
                }
                2 => {
                    self.confirm_discard = None;
                    self.discard();
                    self.do_nav(nav);
                }
                3 => self.confirm_discard = None,
                _ => {}
            }
        }
    }
}

pub fn fmt_dur(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms < 1000.0 {
        format!("{ms:.1} ms")
    } else {
        format!("{:.3} s", ms / 1000.0)
    }
}
