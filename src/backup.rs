//! Export/Import der Einstellungen: Verbindungen und gespeicherte Abfragen als eine JSON-Datei,
//! z. B. zum Umzug auf einen anderen Rechner.

use crate::db::ConnConfig;
use crate::queries;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Kennung im Dateikopf, damit fremde JSON-Dateien abgewiesen werden.
const FORMAT: &str = "mariadb-manager-settings";

#[derive(Serialize, Deserialize)]
pub struct Bundle {
    pub format: String,
    pub version: u32,
    pub connections: Vec<ConnConfig>,
    #[serde(default)]
    pub queries: Vec<SavedQuery>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SavedQuery {
    pub conn: String,
    pub db: String,
    pub name: String,
    pub sql: String,
}

/// Alle gespeicherten Abfragen einer Verbindung (aus `queries/<Verbindung>/<Datenbank>/*.sql`).
fn conn_queries(conn: &str) -> Result<Vec<SavedQuery>, String> {
    let mut out = Vec::new();
    let Some(dir) = queries::root().map(|r| r.join(queries::sanitize(conn))) else { return Ok(out) };
    let Ok(rd) = std::fs::read_dir(&dir) else { return Ok(out) };
    for db_dir in rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()) {
        let db = db_dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        for q in queries::list(conn, &db) {
            out.push(SavedQuery { conn: conn.to_string(), db: db.clone(), sql: queries::load(conn, &db, &q.name)?, name: q.name });
        }
    }
    out.sort_by(|a, b| (&a.db, &a.name).cmp(&(&b.db, &b.name)));
    Ok(out)
}

/// Schreibt die gewählten Verbindungen samt Abfragen; liefert die Anzahl exportierter Abfragen.
pub fn export(path: &Path, conns: &[ConnConfig], passwords: bool) -> Result<usize, String> {
    let mut b = Bundle { format: FORMAT.into(), version: 1, connections: Vec::new(), queries: Vec::new() };
    for c in conns {
        let mut c = c.clone();
        if !passwords || !c.save_password {
            c.password.clear();
        }
        b.queries.extend(conn_queries(&c.name)?);
        b.connections.push(c);
    }
    let json = serde_json::to_string_pretty(&b).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(b.queries.len())
}

pub fn read(path: &Path) -> Result<Bundle, String> {
    let s = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let b: Bundle = serde_json::from_str(&s).map_err(|e| format!("Keine gültige Einstellungsdatei: {e}"))?;
    if b.format != FORMAT {
        return Err("Keine Einstellungsdatei des MariaDB Managers.".into());
    }
    Ok(b)
}

#[derive(Default)]
pub struct Report {
    pub added: Vec<String>,
    pub skipped: Vec<String>,
    pub queries: usize,
    pub queries_skipped: usize,
    pub errors: Vec<String>,
}

impl Report {
    pub fn text(&self) -> String {
        let mut s = format!("{} Verbindung(en) importiert, {} Abfrage(n) übernommen.", self.added.len(), self.queries);
        if !self.skipped.is_empty() {
            s.push_str(&format!("\nBereits vorhanden (Verbindungsdaten unverändert): {}", self.skipped.join(", ")));
        }
        if self.queries_skipped > 0 {
            s.push_str(&format!("\n{} vorhandene Abfrage(n) nicht überschrieben.", self.queries_skipped));
        }
        for e in &self.errors {
            s.push_str(&format!("\n{e}"));
        }
        s
    }
}

/// Übernimmt die gewählten Verbindungen (vorhandene gleichen Namens bleiben unverändert)
/// und schreibt ihre Abfragen. Liefert die neuen Verbindungen.
pub fn import(b: &Bundle, sel: &[bool], existing: &[String], overwrite: bool) -> (Vec<ConnConfig>, Report) {
    let mut rep = Report::default();
    let mut new = Vec::new();
    for (c, _) in b.connections.iter().zip(sel).filter(|(_, s)| **s) {
        if existing.iter().chain(new.iter().map(|n: &ConnConfig| &n.name)).any(|e| e.eq_ignore_ascii_case(&c.name)) {
            rep.skipped.push(c.name.clone());
        } else {
            rep.added.push(c.name.clone());
            new.push(c.clone());
        }
        for q in b.queries.iter().filter(|q| q.conn == c.name) {
            if !overwrite && queries::exists(&q.conn, &q.db, &q.name) {
                rep.queries_skipped += 1;
                continue;
            }
            match queries::save(&q.conn, &q.db, &q.name, &q.sql) {
                Ok(()) => rep.queries += 1,
                Err(e) => rep.errors.push(e),
            }
        }
    }
    (new, rep)
}

// ------------------------------------------------------------------ Dialog

pub enum Outcome {
    /// Import ausgeführt: neue Verbindungen (Abfragen sind bereits geschrieben).
    Imported(Vec<ConnConfig>),
    Close,
}

pub struct BackupDlg {
    import: bool,
    path: String,
    /// Export: alle Verbindungen; Import: Inhalt der geladenen Datei.
    conns: Vec<ConnConfig>,
    /// Abfragen je Verbindung (nur zur Anzeige).
    counts: Vec<usize>,
    sel: Vec<bool>,
    passwords: bool,
    overwrite: bool,
    bundle: Option<Bundle>,
    error: Option<String>,
    report: Option<String>,
}

impl BackupDlg {
    pub fn export(conns: Vec<ConnConfig>) -> Self {
        let counts = conns.iter().map(|c| conn_queries(&c.name).map(|q| q.len()).unwrap_or(0)).collect();
        let sel = vec![true; conns.len()];
        let path = crate::export::default_path_ext("mariadb-manager-einstellungen", "json");
        Self { import: false, path, conns, counts, sel, passwords: false, overwrite: false, bundle: None, error: None, report: None }
    }

    pub fn import() -> Self {
        let path = crate::export::default_path_ext("mariadb-manager-einstellungen", "json");
        Self { import: true, path, conns: Vec::new(), counts: Vec::new(), sel: Vec::new(), passwords: false, overwrite: false, bundle: None, error: None, report: None }
    }

    fn load(&mut self) {
        self.bundle = None;
        self.conns.clear();
        match read(Path::new(self.path.trim())) {
            Ok(b) => {
                self.conns = b.connections.clone();
                self.counts = b.connections.iter().map(|c| b.queries.iter().filter(|q| q.conn == c.name).count()).collect();
                self.sel = vec![true; self.conns.len()];
                self.error = None;
                self.bundle = Some(b);
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// `existing` = Namen der vorhandenen Verbindungen (für die Kennzeichnung beim Import).
    pub fn show(&mut self, ctx: &egui::Context, existing: &[String]) -> Option<Outcome> {
        let mut out = None;
        let title = if self.import { "Einstellungen importieren" } else { "Einstellungen exportieren" };
        egui::Modal::new(egui::Id::unique("backup_dlg")).show(ctx, |ui| {
            ui.set_width(620.0);
            ui.heading(title);
            if let Some(r) = &self.report {
                ui.label(r);
                if ui.button("OK").clicked() {
                    out = Some(Outcome::Close);
                }
                return;
            }
            ui.weak("Verbindungen und gespeicherte Abfragen als JSON-Datei.");
            ui.add_space(6.0);
            let mut reload = false;
            ui.horizontal(|ui| {
                ui.label("Datei:");
                let r = ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(440.0));
                reload |= self.import && r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                #[cfg(any(windows, target_os = "macos"))]
                if ui.button("…").on_hover_text("Datei wählen").clicked() {
                    let p = PathBuf::from(&self.path);
                    let mut d = rfd::FileDialog::new().add_filter("JSON-Datei", &["json"]);
                    if let Some(dir) = p.parent().filter(|d| d.is_dir()) {
                        d = d.set_directory(dir);
                    }
                    let f = if self.import {
                        d.pick_file()
                    } else {
                        if let Some(name) = p.file_name() {
                            d = d.set_file_name(name.to_string_lossy());
                        }
                        d.save_file()
                    };
                    if let Some(f) = f {
                        self.path = f.display().to_string();
                        reload |= self.import;
                    }
                }
                if self.import && ui.button("Laden").clicked() {
                    reload = true;
                }
            });
            if reload {
                self.load();
            }
            if let Some(e) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            if !self.conns.is_empty() {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Verbindungen:");
                    if ui.small_button("Alle").clicked() {
                        self.sel.iter_mut().for_each(|s| *s = true);
                    }
                    if ui.small_button("Keine").clicked() {
                        self.sel.iter_mut().for_each(|s| *s = false);
                    }
                });
                egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                    for ((c, s), n) in self.conns.iter().zip(self.sel.iter_mut()).zip(&self.counts) {
                        let mut t = format!("{}  –  {}@{}:{}", c.name, c.user, c.host, c.port);
                        if *n > 0 {
                            t.push_str(&format!("  ({n} Abfragen)"));
                        }
                        if self.import && existing.iter().any(|e| e.eq_ignore_ascii_case(&c.name)) {
                            t.push_str("  [vorhanden]");
                        }
                        ui.checkbox(s, t);
                    }
                });
            }
            ui.add_space(4.0);
            if self.import {
                if self.bundle.is_some() {
                    ui.checkbox(&mut self.overwrite, "Vorhandene Abfragen gleichen Namens überschreiben");
                    ui.weak("Vorhandene Verbindungen gleichen Namens bleiben unverändert; ihre Abfragen werden ergänzt.");
                }
            } else {
                ui.checkbox(&mut self.passwords, "Passwörter mitexportieren");
                if self.passwords {
                    ui.colored_label(ui.visuals().warn_fg_color, "Die Passwörter stehen im Klartext in der Datei.");
                }
                if Path::new(self.path.trim()).exists() {
                    ui.colored_label(ui.visuals().warn_fg_color, "Die Datei existiert bereits und wird überschrieben.");
                }
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let any = self.sel.iter().any(|s| *s) && !self.path.trim().is_empty();
                let label = if self.import { "Importieren" } else { "Exportieren" };
                if ui.add_enabled(any && (!self.import || self.bundle.is_some()), egui::Button::new(label)).clicked() {
                    if let Some(b) = &self.bundle {
                        let (new, rep) = import(b, &self.sel, existing, self.overwrite);
                        self.report = Some(rep.text());
                        out = Some(Outcome::Imported(new));
                    } else {
                        let chosen: Vec<ConnConfig> = self.conns.iter().zip(&self.sel).filter(|(_, s)| **s).map(|(c, _)| c.clone()).collect();
                        let path = PathBuf::from(self.path.trim());
                        match export(&path, &chosen, self.passwords) {
                            Ok(n) => {
                                self.report = Some(format!("{} Verbindung(en) und {n} Abfrage(n) exportiert nach {}", chosen.len(), path.display()))
                            }
                            Err(e) => self.error = Some(e),
                        }
                    }
                }
                if ui.button("Abbrechen").clicked() {
                    out = Some(Outcome::Close);
                }
            });
        });
        if out.is_none() && self.report.is_none() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            out = Some(Outcome::Close);
        }
        out
    }
}
