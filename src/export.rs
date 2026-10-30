//! CSV-Export: Semikolon-getrennt, LF als Zeilenende, alle Werte in Anführungszeichen
//! (NULL = leeres Feld ohne Anführungszeichen), optional mit UTF-8-BOM.
//!
//! Tabellen werden auf einer eigenen Verbindung direkt vom Server in die Datei
//! gestreamt; Abfrageergebnisse werden aus dem Speicher geschrieben.

use crate::db::{self, ConnConfig, ResultSet};
use eframe::egui;
use mysql::prelude::*;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

/// Zuletzt gewählte BOM-Einstellung (wird von der App in den Einstellungen gespeichert).
static BOM: AtomicBool = AtomicBool::new(true);

pub fn bom() -> bool {
    BOM.load(Ordering::Relaxed)
}

pub fn set_bom(v: bool) {
    BOM.store(v, Ordering::Relaxed);
}

fn field(v: Option<&str>) -> String {
    match v {
        None => String::new(),
        Some(s) => format!("\"{}\"", s.replace('"', "\"\"")),
    }
}

/// Eine CSV-Zeile inkl. abschließendem LF.
pub fn csv_line<'a>(vals: impl IntoIterator<Item = Option<&'a str>>) -> String {
    let mut s = vals.into_iter().map(field).collect::<Vec<_>>().join(";");
    s.push('\n');
    s
}

fn create(path: &Path, bom: bool) -> Result<BufWriter<std::fs::File>, String> {
    let f = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut w = BufWriter::with_capacity(1 << 16, f);
    if bom {
        w.write_all(b"\xEF\xBB\xBF").map_err(|e| e.to_string())?;
    }
    Ok(w)
}

/// Ergebnis aus dem Speicher schreiben; liefert die Zeilenzahl.
pub fn write_result(rs: &ResultSet, path: &Path, bom: bool) -> Result<u64, String> {
    let mut w = create(path, bom)?;
    let io = |e: std::io::Error| e.to_string();
    w.write_all(csv_line(rs.columns.iter().map(|c| Some(c.name.as_str()))).as_bytes()).map_err(io)?;
    for row in &rs.rows {
        w.write_all(csv_line(row.iter().map(|v| v.as_deref())).as_bytes()).map_err(io)?;
    }
    w.flush().map_err(io)?;
    Ok(rs.rows.len() as u64)
}

/// Fortschritt eines laufenden Exports (wird vom Export-Thread geschrieben).
#[derive(Clone)]
pub struct Progress {
    rows: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
    stage: Arc<Mutex<String>>,
    ctx: egui::Context,
}

impl Progress {
    /// Zeilenzahl melden; liefert `Err`, wenn abgebrochen wurde.
    pub fn rows(&self, n: u64) -> Result<(), String> {
        self.rows.store(n, Ordering::Relaxed);
        self.ctx.request_repaint();
        if self.cancel.load(Ordering::Relaxed) {
            Err("Export abgebrochen.".into())
        } else {
            Ok(())
        }
    }
    /// Aktueller Arbeitsschritt (z. B. Tabellenname) für die Statusleiste.
    pub fn stage(&self, s: impl Into<String>) {
        *self.stage.lock().unwrap() = s.into();
        self.ctx.request_repaint();
    }
}

/// Laufender Export im Hintergrund.
pub struct ExportJob {
    pub path: PathBuf,
    p: Progress,
    rx: mpsc::Receiver<Result<u64, String>>,
}

impl ExportJob {
    pub fn rows(&self) -> u64 {
        self.p.rows.load(Ordering::Relaxed)
    }
    pub fn stage(&self) -> String {
        self.p.stage.lock().unwrap().clone()
    }
    pub fn cancel(&self) {
        self.p.cancel.store(true, Ordering::Relaxed);
    }
    /// `Some`, sobald der Export beendet ist.
    pub fn poll(&self) -> Option<Result<u64, String>> {
        self.rx.try_recv().ok()
    }
}

/// Startet `work` in einem eigenen Thread; bei Fehler/Abbruch wird die Datei gelöscht.
pub fn spawn(
    path: PathBuf,
    ctx: egui::Context,
    work: impl FnOnce(&Path, &Progress) -> Result<u64, String> + Send + 'static,
) -> ExportJob {
    let p = Progress {
        rows: Arc::new(AtomicU64::new(0)),
        cancel: Arc::new(AtomicBool::new(false)),
        stage: Arc::new(Mutex::new(String::new())),
        ctx: ctx.clone(),
    };
    let (tx, rx) = mpsc::channel();
    let (p2, path2) = (p.clone(), path.clone());
    std::thread::Builder::new()
        .name("export".into())
        .spawn(move || {
            let res = work(&path2, &p2);
            if res.is_err() {
                let _ = std::fs::remove_file(&path2);
            }
            let _ = tx.send(res);
            ctx.request_repaint();
        })
        .expect("Thread konnte nicht gestartet werden");
    ExportJob { path, p, rx }
}

/// Führt `sql` auf einer eigenen Verbindung aus und schreibt das erste Ergebnis als CSV.
pub fn export_query(cfg: ConnConfig, sql: String, path: PathBuf, bom: bool, ctx: egui::Context) -> ExportJob {
    spawn(path, ctx, move |path, p| run(&cfg, &sql, path, bom, p))
}

fn run(cfg: &ConnConfig, sql: &str, path: &Path, bom: bool, p: &Progress) -> Result<u64, String> {
    let mut conn = db::connect(cfg).map_err(|e| format!("Verbindung fehlgeschlagen: {e}"))?;
    let mut w = create(path, bom)?;
    let io = |e: std::io::Error| e.to_string();
    let mut q = conn.query_iter(sql).map_err(|e| e.to_string())?;
    let mut n = 0u64;
    if let Some(set) = q.iter() {
        let bin: Vec<bool> = set.columns().as_ref().iter().map(db::is_binary_col).collect();
        let names: Vec<String> = set.columns().as_ref().iter().map(|c| c.name_str().into_owned()).collect();
        w.write_all(csv_line(names.iter().map(|s| Some(s.as_str()))).as_bytes()).map_err(io)?;
        for row in set {
            let row = row.map_err(|e| e.to_string())?;
            let vals: Vec<Option<String>> =
                row.unwrap().into_iter().enumerate().map(|(i, v)| db::value_full(v, bin.get(i).copied().unwrap_or(false))).collect();
            w.write_all(csv_line(vals.iter().map(|v| v.as_deref())).as_bytes()).map_err(io)?;
            n += 1;
            if n % 2000 == 0 {
                p.rows(n)?;
            }
        }
    }
    w.flush().map_err(io)?;
    p.rows(n)?;
    Ok(n)
}

/// Standardpfad: Dokumente-Ordner + Dateiname.
pub fn default_path(file: &str) -> String {
    default_path_ext(file, "csv")
}

pub fn default_path_ext(file: &str, ext: &str) -> String {
    let dir = directories::UserDirs::new()
        .and_then(|u| u.document_dir().map(|d| d.to_path_buf()).or_else(|| Some(u.home_dir().to_path_buf())))
        .unwrap_or_default();
    dir.join(format!("{}.{ext}", crate::queries::sanitize(file))).display().to_string()
}

/// Export-Dialog (Dateiname + BOM). Liefert `Some(Some((Pfad, BOM)))` bei „Exportieren“,
/// `Some(None)` bei „Abbrechen“, sonst `None` (Dialog bleibt offen).
pub struct ExportDialog {
    pub path: String,
    pub bom: bool,
    pub note: Option<String>,
}

impl ExportDialog {
    pub fn new(file: &str, note: Option<String>) -> Self {
        Self { path: default_path(file), bom: bom(), note }
    }

    pub fn show(&mut self, ctx: &egui::Context, id: impl std::hash::Hash + std::fmt::Debug) -> Option<Option<(PathBuf, bool)>> {
        let mut out = None;
        egui::Modal::new(egui::Id::unique(("export_dlg", id))).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.heading("CSV-Export");
            ui.weak("Semikolon-getrennt, Zeilenende LF, Werte in Anführungszeichen, NULL als leeres Feld.");
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Datei:");
                ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(400.0));
                #[cfg(any(windows, target_os = "macos"))]
                if ui.button("…").on_hover_text("Speicherort wählen").clicked() {
                    let p = PathBuf::from(&self.path);
                    let mut d = rfd::FileDialog::new().add_filter("CSV-Datei", &["csv"]);
                    if let Some(dir) = p.parent().filter(|d| d.is_dir()) {
                        d = d.set_directory(dir);
                    }
                    if let Some(name) = p.file_name() {
                        d = d.set_file_name(name.to_string_lossy());
                    }
                    if let Some(f) = d.save_file() {
                        self.path = f.display().to_string();
                    }
                }
            });
            ui.checkbox(&mut self.bom, "UTF-8 mit BOM (für Excel)");
            if let Some(n) = &self.note {
                ui.colored_label(ui.visuals().warn_fg_color, n);
            }
            let exists = Path::new(&self.path).exists();
            if exists {
                ui.colored_label(ui.visuals().warn_fg_color, "Die Datei existiert bereits und wird überschrieben.");
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.path.trim().is_empty(), egui::Button::new("Exportieren")).clicked() {
                    set_bom(self.bom);
                    out = Some(Some((PathBuf::from(self.path.trim()), self.bom)));
                }
                if ui.button("Abbrechen").clicked() {
                    out = Some(None);
                }
            });
        });
        if out.is_none() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            out = Some(None);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_format() {
        assert_eq!(csv_line([Some("a"), None, Some("x\"y;z\nw")]), "\"a\";;\"x\"\"y;z\nw\"\n");
    }

    #[test]
    fn write_result_file() {
        let rs = ResultSet {
            columns: vec![
                db::Col { name: "id".into(), numeric: true, ..Default::default() },
                db::Col { name: "name".into(), ..Default::default() },
            ],
            rows: vec![vec![Some("1".into()), Some("Müller".into())], vec![Some("2".into()), None]],
            truncated: false,
        };
        let p = std::env::temp_dir().join(format!("mdbm_export_test_{}.csv", std::process::id()));
        assert_eq!(write_result(&rs, &p, true).unwrap(), 2);
        let b = std::fs::read(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        assert_eq!(b, "\u{FEFF}\"id\";\"name\"\n\"1\";\"Müller\"\n\"2\";\n".as_bytes());
    }
}
