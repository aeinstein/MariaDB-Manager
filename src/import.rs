//! Import-Assistent für CSV/TXT: Codierung, Zeilen- und Spaltentrenner werden
//! erkannt, Vorschau der ersten 20 Zeilen, Ziel neue oder vorhandene Tabelle,
//! Feldzuordnung mit Überspringen. Der eigentliche Import läuft auf einer eigenen
//! Verbindung in einem Hintergrund-Thread (Mehrzeilen-INSERT in einer Transaktion).

use crate::db::{self, qi, ConnConfig, Mode, Stmt, Worker};
use eframe::egui::{self, RichText};
use egui_extras::{Column, TableBuilder};
use mysql::prelude::*;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};

const PREVIEW_ROWS: usize = 20;

// ------------------------------------------------------------------ Codierung

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Enc {
    Utf8,
    Utf16Le,
    Utf16Be,
    Win1252,
    Latin1,
}

impl Enc {
    const ALL: [Enc; 5] = [Enc::Utf8, Enc::Utf16Le, Enc::Utf16Be, Enc::Win1252, Enc::Latin1];
    fn label(self) -> &'static str {
        match self {
            Enc::Utf8 => "UTF-8",
            Enc::Utf16Le => "UTF-16 LE",
            Enc::Utf16Be => "UTF-16 BE",
            Enc::Win1252 => "Windows-1252 (ANSI)",
            Enc::Latin1 => "ISO-8859-1",
        }
    }
}

/// Erkennt die Codierung anhand von BOM bzw. gültigem UTF-8.
pub fn detect_enc(b: &[u8]) -> Enc {
    if b.starts_with(&[0xEF, 0xBB, 0xBF]) {
        Enc::Utf8
    } else if b.starts_with(&[0xFF, 0xFE]) {
        Enc::Utf16Le
    } else if b.starts_with(&[0xFE, 0xFF]) {
        Enc::Utf16Be
    } else if std::str::from_utf8(b).is_ok() {
        Enc::Utf8
    } else {
        // Ohne BOM: viele Nullbytes an ungeraden/geraden Stellen deuten auf UTF-16
        let n = b.len().min(4096);
        let odd = b[..n].iter().skip(1).step_by(2).filter(|&&x| x == 0).count();
        let even = b[..n].iter().step_by(2).filter(|&&x| x == 0).count();
        if odd > n / 4 {
            Enc::Utf16Le
        } else if even > n / 4 {
            Enc::Utf16Be
        } else {
            Enc::Win1252
        }
    }
}

pub fn decode(b: &[u8], enc: Enc) -> String {
    match enc {
        Enc::Utf8 => String::from_utf8_lossy(b.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(b)).into_owned(),
        Enc::Utf16Le | Enc::Utf16Be => {
            let b = b.strip_prefix(if enc == Enc::Utf16Le { &[0xFF, 0xFE] } else { &[0xFE, 0xFF] }).unwrap_or(b);
            let u: Vec<u16> = b
                .chunks_exact(2)
                .map(|c| if enc == Enc::Utf16Le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
                .collect();
            String::from_utf16_lossy(&u)
        }
        Enc::Win1252 => b.iter().map(|&c| crate::queries::cp1252(c)).collect(),
        Enc::Latin1 => b.iter().map(|&c| c as char).collect(),
    }
}

// ------------------------------------------------------------------ Parser

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Eol {
    Auto,
    CrLf,
    Lf,
    Cr,
}

impl Eol {
    fn label(self) -> &'static str {
        match self {
            Eol::Auto => "Automatisch",
            Eol::CrLf => "CRLF (Windows)",
            Eol::Lf => "LF (Unix)",
            Eol::Cr => "CR (Mac alt)",
        }
    }
}

/// Häufigstes Zeilenende im Text.
pub fn detect_eol(t: &str) -> Eol {
    let crlf = t.matches("\r\n").count();
    let lf = t.matches('\n').count() - crlf;
    let cr = t.matches('\r').count() - crlf;
    if crlf >= lf && crlf >= cr && crlf > 0 {
        Eol::CrLf
    } else if cr > lf {
        Eol::Cr
    } else {
        Eol::Lf
    }
}

/// Zerlegt Text in Datensätze/Felder; Zeilenumbrüche innerhalb von Anführungszeichen
/// bleiben erhalten, doppelte Anführungszeichen stehen für ein einzelnes.
pub fn parse(t: &str, delim: char, quote: Option<char>, eol: Eol, limit: usize) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut in_q = false;
    let mut was_quoted = false;
    let mut it = t.chars().peekable();
    let end_row = |row: &mut Vec<String>, field: &mut String, rows: &mut Vec<Vec<String>>| {
        row.push(std::mem::take(field));
        let r = std::mem::take(row);
        // Leere Zeilen auslassen
        if !(r.len() == 1 && r[0].is_empty()) {
            rows.push(r);
        }
    };
    while let Some(c) = it.next() {
        if rows.len() >= limit {
            return rows;
        }
        if in_q {
            if Some(c) == quote {
                if it.peek() == quote.as_ref() {
                    field.push(c);
                    it.next();
                } else {
                    in_q = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        if Some(c) == quote && field.is_empty() && !was_quoted {
            in_q = true;
            was_quoted = true;
            continue;
        }
        if c == delim {
            row.push(std::mem::take(&mut field));
            was_quoted = false;
            continue;
        }
        let is_eol = match eol {
            Eol::Auto => c == '\n' || c == '\r',
            Eol::Lf => c == '\n',
            Eol::Cr => c == '\r',
            Eol::CrLf => c == '\r' && it.peek() == Some(&'\n'),
        };
        if is_eol {
            if c == '\r' && it.peek() == Some(&'\n') && eol != Eol::Cr {
                it.next();
            }
            end_row(&mut row, &mut field, &mut rows);
            was_quoted = false;
            continue;
        }
        if eol == Eol::CrLf && c == '\n' {
            // einzelnes LF bei CRLF-Dateien: als Inhalt übernehmen
        }
        field.push(c);
    }
    if !field.is_empty() || !row.is_empty() {
        end_row(&mut row, &mut field, &mut rows);
    }
    rows
}

const DELIMS: [char; 5] = [';', ',', '\t', '|', ' '];

/// Spaltentrenner, der in den ersten Zeilen die gleichmäßigste Spaltenzahl > 1 liefert.
pub fn detect_delim(t: &str, quote: Option<char>) -> char {
    let mut best = (';', 0.0f64);
    for d in DELIMS {
        let rows = parse(t, d, quote, Eol::Auto, 50);
        if rows.is_empty() {
            continue;
        }
        let mut counts: Vec<usize> = rows.iter().map(|r| r.len()).collect();
        counts.sort_unstable();
        let mode = counts[counts.len() / 2];
        if mode < 2 {
            continue;
        }
        let same = counts.iter().filter(|&&c| c == mode).count() as f64 / counts.len() as f64;
        // Gleichmäßigkeit zählt mehr als Spaltenzahl; Leerzeichen nur als letzte Wahl
        let score = same * 100.0 + (mode as f64).min(50.0) - if d == ' ' { 60.0 } else { 0.0 };
        if score > best.1 {
            best = (d, score);
        }
    }
    best.0
}

// ------------------------------------------------------------------ Werte und Typen

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Text,
    Number,
    Date,
    DateTime,
}

pub fn kind_of(data_type: &str) -> Kind {
    let t = data_type.to_lowercase();
    let t = t.split('(').next().unwrap_or("").trim();
    match t {
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "decimal" | "numeric" | "float" | "double" | "real" => {
            Kind::Number
        }
        "date" => Kind::Date,
        "datetime" | "timestamp" => Kind::DateTime,
        _ => Kind::Text,
    }
}

/// Deutsche Zahlen (`1.234,56`) → `1234.56`; `None`, wenn keine Zahl.
pub fn norm_number(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let t = if s.contains(',') {
        if s.rfind(',') > s.rfind('.') {
            s.replace('.', "").replace(',', ".")
        } else {
            s.replace(',', "")
        }
    } else {
        s.to_string()
    };
    t.parse::<f64>().ok().filter(|f| f.is_finite()).map(|_| t)
}

/// `31.12.2024[ 13:45[:10]]` bzw. ISO → `2024-12-31[ 13:45:10]`.
pub fn norm_date(s: &str) -> Option<(String, bool)> {
    let s = s.trim();
    let (d, time) = match s.split_once([' ', 'T']) {
        Some((d, t)) => (d, Some(t.trim())),
        None => (s, None),
    };
    let p: Vec<&str> = d.split(['.', '-', '/']).collect();
    if p.len() != 3 || !p.iter().all(|x| !x.is_empty() && x.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    let (y, m, dd) = if p[0].len() == 4 {
        (p[0], p[1], p[2])
    } else if p[2].len() == 4 && d.contains('.') {
        (p[2], p[1], p[0])
    } else {
        return None;
    };
    let (m, dd): (u32, u32) = (m.parse().ok()?, dd.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&dd) {
        return None;
    }
    let date = format!("{y}-{m:02}-{dd:02}");
    match time {
        None => Some((date, false)),
        Some(t) => {
            let tp: Vec<&str> = t.split(':').collect();
            if !(2..=3).contains(&tp.len()) {
                return None;
            }
            let h: u32 = tp[0].parse().ok()?;
            let mi: u32 = tp[1].parse().ok()?;
            let sec: f64 = tp.get(2).map_or(Some(0.0), |x| x.parse().ok())?;
            if h > 23 || mi > 59 || sec >= 60.0 {
                return None;
            }
            Some((format!("{date} {h:02}:{mi:02}:{:02}", sec as u32), true))
        }
    }
}

/// Wert für die Zielspalte aufbereiten.
pub fn convert(v: &str, kind: Kind, trim: bool, empty_null: bool) -> Option<String> {
    let v = if trim { v.trim() } else { v };
    if v.is_empty() && (empty_null || kind != Kind::Text) {
        return None;
    }
    Some(match kind {
        Kind::Number => norm_number(v).unwrap_or_else(|| v.to_string()),
        Kind::Date | Kind::DateTime => norm_date(v).map(|d| d.0).unwrap_or_else(|| v.to_string()),
        Kind::Text => v.to_string(),
    })
}

/// Spaltentyp für eine neue Tabelle aus den Daten schätzen.
pub fn guess_type(vals: impl Iterator<Item = String>) -> String {
    let (mut any, mut int, mut num, mut date, mut dt) = (false, true, true, true, true);
    let (mut max_len, mut max_i, mut max_f, mut big) = (0usize, 1usize, 0usize, false);
    for v in vals {
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        any = true;
        max_len = max_len.max(v.chars().count());
        match norm_number(v) {
            Some(n) => {
                let n = n.trim_start_matches(['-', '+']);
                let (i, f) = n.split_once('.').unwrap_or((n, ""));
                if !f.is_empty() || n.contains(['e', 'E']) {
                    int = false;
                }
                max_i = max_i.max(i.trim_start_matches('0').len().max(1));
                max_f = max_f.max(f.len());
                if n.parse::<i64>().map_or(true, |x| x > i32::MAX as i64) && int {
                    big = true;
                }
            }
            None => {
                int = false;
                num = false;
            }
        }
        match norm_date(v) {
            Some((_, true)) => date = false,
            Some((_, false)) => {}
            None => {
                date = false;
                dt = false;
            }
        }
    }
    if !any {
        "VARCHAR(255)".into()
    } else if int {
        if big { "BIGINT" } else { "INT" }.into()
    } else if num && max_i + max_f <= 65 {
        format!("DECIMAL({},{})", (max_i + max_f).max(1), max_f.min(30))
    } else if date {
        "DATE".into()
    } else if dt {
        "DATETIME".into()
    } else if max_len <= 255 {
        "VARCHAR(255)".into()
    } else if max_len <= 16_000 {
        "TEXT".into()
    } else {
        "LONGTEXT".into()
    }
}

/// Feldnamen für eine neue Tabelle: leer → `spalte_N`, doppelte erhalten Suffix.
fn column_names(header: Option<&Vec<String>>, n: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for i in 0..n {
        let base = header.and_then(|h| h.get(i)).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or(format!("spalte_{}", i + 1));
        let base: String = base.chars().take(60).collect();
        let mut name = base.clone();
        let mut k = 2;
        while out.iter().any(|o| o.eq_ignore_ascii_case(&name)) {
            name = format!("{base}_{k}");
            k += 1;
        }
        out.push(name);
    }
    out
}

// ------------------------------------------------------------------ Import-Thread

/// Was der Thread ausführen soll.
struct Plan {
    db: String,
    table: String,
    create: Option<String>,
    clear: bool,
    /// (Quellspalte, Zielspalte, Art)
    cols: Vec<(usize, String, Kind)>,
    trim: bool,
    empty_null: bool,
}

struct Job {
    rows: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
    rx: mpsc::Receiver<Result<u64, String>>,
}

fn start(cfg: ConnConfig, plan: Plan, data: Vec<Vec<String>>, ctx: egui::Context) -> Job {
    let rows = Arc::new(AtomicU64::new(0));
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let (r2, c2) = (rows.clone(), cancel.clone());
    std::thread::Builder::new()
        .name("csv-import".into())
        .spawn(move || {
            let res = run(&cfg, &plan, &data, &r2, &c2, &ctx);
            let _ = tx.send(res);
            ctx.request_repaint();
        })
        .expect("Thread konnte nicht gestartet werden");
    Job { rows, cancel, rx }
}

fn run(cfg: &ConnConfig, plan: &Plan, data: &[Vec<String>], rows: &AtomicU64, cancel: &AtomicBool, ctx: &egui::Context) -> Result<u64, String> {
    let mut c = db::connect(cfg).map_err(|e| format!("Verbindung fehlgeschlagen: {e}"))?;
    c.query_drop(format!("USE {}", qi(&plan.db))).map_err(|e| e.to_string())?;
    if let Some(sql) = &plan.create {
        c.query_drop(sql).map_err(|e| format!("Tabelle anlegen: {e}"))?;
    }
    let ncols = plan.cols.len().max(1);
    let chunk = (60_000 / ncols).clamp(1, 1000);
    let head = format!(
        "INSERT INTO {} ({}) VALUES ",
        qi(&plan.table),
        plan.cols.iter().map(|c| qi(&c.1)).collect::<Vec<_>>().join(", ")
    );
    let one = format!("({})", vec!["?"; plan.cols.len()].join(", "));
    c.query_drop("START TRANSACTION").map_err(|e| e.to_string())?;
    let res = (|| -> Result<u64, String> {
        if plan.clear {
            c.query_drop(format!("DELETE FROM {}", qi(&plan.table))).map_err(|e| format!("Daten löschen: {e}"))?;
        }
        let mut n = 0u64;
        for (k, part) in data.chunks(chunk).enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return Err("Import abgebrochen – keine Daten übernommen.".into());
            }
            let sql = format!("{head}{}", vec![one.as_str(); part.len()].join(", "));
            let mut params: Vec<mysql::Value> = Vec::with_capacity(part.len() * plan.cols.len());
            for r in part {
                for (src, _, kind) in &plan.cols {
                    let v = r.get(*src).and_then(|v| convert(v, *kind, plan.trim, plan.empty_null));
                    params.push(v.map(mysql::Value::from).unwrap_or(mysql::Value::NULL));
                }
            }
            c.exec_drop(&sql, params).map_err(|e| {
                let from = k * chunk + 1;
                format!("Fehler in Datenzeilen {from}–{}: {e}", from + part.len() - 1)
            })?;
            n += part.len() as u64;
            rows.store(n, Ordering::Relaxed);
            ctx.request_repaint();
        }
        Ok(n)
    })();
    match res {
        Ok(n) => {
            c.query_drop("COMMIT").map_err(|e| e.to_string())?;
            Ok(n)
        }
        Err(e) => {
            let _ = c.query_drop("ROLLBACK");
            Err(e)
        }
    }
}

// ------------------------------------------------------------------ Assistent

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    File,
    Target,
    Fields,
    Run,
}

/// Feld für eine neue Tabelle.
struct NewCol {
    take: bool,
    name: String,
    typ: String,
}

pub struct ImportWizard {
    cfg: ConnConfig,
    pub db: String,
    worker: Worker,
    tag_tables: u64,
    tag_cols: u64,
    step: Step,
    // Datei & Format
    path: String,
    bytes: Vec<u8>,
    enc: Enc,
    enc_detected: Enc,
    eol: Eol,
    eol_detected: Eol,
    delim: char,
    custom_delim: String,
    quote: Option<char>,
    header: bool,
    text: String,
    preview: Vec<Vec<String>>,
    file_error: Option<String>,
    // Ziel
    new_table: bool,
    new_name: String,
    tables: Vec<String>,
    table: String,
    clear: bool,
    trim: bool,
    empty_null: bool,
    add_id: bool,
    // Felder
    data: Vec<Vec<String>>,
    src_names: Vec<String>,
    new_cols: Vec<NewCol>,
    /// Zielspalten der vorhandenen Tabelle: (Name, Datentyp)
    target_cols: Vec<(String, String)>,
    /// Zuordnung Quelle → Index in `target_cols` (None = überspringen)
    map: Vec<Option<usize>>,
    loading_cols: bool,
    // Import
    job: Option<Job>,
    result: Option<Result<u64, String>>,
    done: Option<String>,
}

impl ImportWizard {
    pub fn new(cfg: &ConnConfig, ctx: &egui::Context, db: &str, table: Option<&str>) -> Self {
        let mut worker = Worker::spawn(cfg.clone(), ctx.clone());
        let tag_tables = worker.send(
            None,
            vec![Stmt::with(
                "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'BASE TABLE' ORDER BY TABLE_NAME",
                vec![Some(db.to_string())],
            )],
            Mode::StopOnError,
            usize::MAX,
        );
        let mut w = Self {
            cfg: cfg.clone(),
            db: db.to_string(),
            worker,
            tag_tables,
            tag_cols: 0,
            step: Step::File,
            path: String::new(),
            bytes: Vec::new(),
            enc: Enc::Utf8,
            enc_detected: Enc::Utf8,
            eol: Eol::Auto,
            eol_detected: Eol::Lf,
            delim: ';',
            custom_delim: String::new(),
            quote: Some('"'),
            header: true,
            text: String::new(),
            preview: Vec::new(),
            file_error: None,
            new_table: table.is_none(),
            new_name: String::new(),
            tables: Vec::new(),
            table: table.unwrap_or_default().to_string(),
            clear: false,
            trim: true,
            empty_null: true,
            add_id: false,
            data: Vec::new(),
            src_names: Vec::new(),
            new_cols: Vec::new(),
            target_cols: Vec::new(),
            map: Vec::new(),
            loading_cols: false,
            job: None,
            result: None,
            done: None,
        };
        if !w.table.is_empty() {
            w.load_cols();
        }
        w
    }

    /// Liefert einmalig die Datenbank nach erfolgreichem Import (Objektliste neu laden).
    pub fn take_done(&mut self) -> Option<String> {
        self.done.take()
    }

    fn load_cols(&mut self) {
        self.tag_cols = self.worker.send(
            None,
            vec![Stmt::with(
                "SELECT COLUMN_NAME, COLUMN_TYPE FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
                vec![Some(self.db.clone()), Some(self.table.clone())],
            )],
            Mode::StopOnError,
            usize::MAX,
        );
        self.loading_cols = true;
        self.target_cols.clear();
    }

    fn poll(&mut self) {
        while let Some(res) = self.worker.poll() {
            let rows: Vec<Vec<String>> = res
                .set(0)
                .map(|s| s.rows.iter().map(|r| r.iter().map(|v| v.clone().unwrap_or_default()).collect()).collect())
                .unwrap_or_default();
            if res.tag == self.tag_tables {
                self.tables = rows.into_iter().filter_map(|r| r.into_iter().next()).collect();
            } else if res.tag == self.tag_cols {
                self.loading_cols = false;
                self.target_cols = rows.into_iter().map(|r| (r[0].clone(), r.get(1).cloned().unwrap_or_default())).collect();
                self.auto_map();
            }
        }
    }

    fn load_file(&mut self) {
        self.file_error = None;
        match std::fs::read(self.path.trim()) {
            Ok(b) => {
                self.bytes = b;
                self.enc_detected = detect_enc(&self.bytes);
                self.enc = self.enc_detected;
                self.text = decode(&self.bytes, self.enc);
                self.eol_detected = detect_eol(&self.text);
                self.eol = Eol::Auto;
                self.delim = detect_delim(&self.text, self.quote);
                self.custom_delim.clear();
                if self.new_name.is_empty() {
                    self.new_name = Path::new(self.path.trim())
                        .file_stem()
                        .map(|s| crate::queries::sanitize(&s.to_string_lossy()).replace([' ', '-', '.'], "_"))
                        .unwrap_or_default();
                }
                self.reparse();
                // Kopfzeile: aus, wenn die erste Zeile Zahlen/Datumswerte enthält
                self.header = self
                    .preview
                    .first()
                    .is_some_and(|r| r.iter().all(|v| v.trim().is_empty() || (norm_number(v).is_none() && norm_date(v).is_none())));
            }
            Err(e) => {
                self.file_error = Some(format!("{}: {e}", self.path.trim()));
                self.bytes.clear();
                self.text.clear();
                self.preview.clear();
            }
        }
    }

    fn reparse(&mut self) {
        self.preview = parse(&self.text, self.delim, self.quote, self.eol, PREVIEW_ROWS + 1);
    }

    fn ncols(&self) -> usize {
        self.data.iter().take(1000).map(|r| r.len()).max().unwrap_or(0).max(self.preview.iter().map(|r| r.len()).max().unwrap_or(0))
    }

    /// Ganze Datei einlesen und Feldvorschläge erzeugen (Schritt „Felder“).
    fn prepare_fields(&mut self) {
        let mut rows = parse(&self.text, self.delim, self.quote, self.eol, usize::MAX);
        let header = if self.header && !rows.is_empty() { Some(rows.remove(0)) } else { None };
        self.data = rows;
        let n = self.ncols().max(header.as_ref().map(|h| h.len()).unwrap_or(0));
        self.src_names = (0..n)
            .map(|i| header.as_ref().and_then(|h| h.get(i)).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or(format!("Spalte {}", i + 1)))
            .collect();
        let names = column_names(header.as_ref(), n);
        self.new_cols = (0..n)
            .map(|i| NewCol { take: true, name: names[i].clone(), typ: guess_type(self.data.iter().filter_map(|r| r.get(i).cloned())) })
            .collect();
        self.auto_map();
    }

    /// Quelle → Ziel nach Namen (ohne Kopfzeile nach Position).
    fn auto_map(&mut self) {
        let n = self.src_names.len();
        self.map = (0..n)
            .map(|i| {
                if self.header {
                    let s = self.src_names[i].to_lowercase();
                    self.target_cols.iter().position(|(t, _)| t.to_lowercase() == s)
                } else {
                    (i < self.target_cols.len()).then_some(i)
                }
            })
            .collect();
    }

    fn plan(&self) -> Result<Plan, String> {
        let (table, create, cols) = if self.new_table {
            let name = self.new_name.trim();
            if name.is_empty() {
                return Err("Bitte einen Tabellennamen angeben.".into());
            }
            if self.tables.iter().any(|t| t.eq_ignore_ascii_case(name)) {
                return Err(format!("Die Tabelle „{name}“ existiert bereits."));
            }
            let used: Vec<(usize, &NewCol)> = self.new_cols.iter().enumerate().filter(|(_, c)| c.take).collect();
            if used.is_empty() {
                return Err("Es ist kein Feld zum Importieren ausgewählt.".into());
            }
            let mut seen = std::collections::HashSet::new();
            for (_, c) in &used {
                if c.name.trim().is_empty() || c.typ.trim().is_empty() {
                    return Err("Jedes übernommene Feld braucht Name und Typ.".into());
                }
                if !seen.insert(c.name.trim().to_lowercase()) {
                    return Err(format!("Feldname „{}“ ist doppelt.", c.name.trim()));
                }
            }
            let mut defs: Vec<String> = Vec::new();
            if self.add_id {
                if seen.contains("id") {
                    return Err("Es gibt bereits ein Feld „id“.".into());
                }
                defs.push("`id` INT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY".into());
            }
            defs.extend(used.iter().map(|(_, c)| format!("{} {} NULL", qi(c.name.trim()), c.typ.trim())));
            let create = format!(
                "CREATE TABLE {} (\n  {}\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
                qi(name),
                defs.join(",\n  ")
            );
            let cols = used.iter().map(|(i, c)| (*i, c.name.trim().to_string(), kind_of(&c.typ))).collect();
            (name.to_string(), Some(create), cols)
        } else {
            if self.table.is_empty() {
                return Err("Bitte eine Zieltabelle wählen.".into());
            }
            let cols: Vec<(usize, String, Kind)> = self
                .map
                .iter()
                .enumerate()
                .filter_map(|(i, m)| m.map(|t| (i, self.target_cols[t].0.clone(), kind_of(&self.target_cols[t].1))))
                .collect();
            if cols.is_empty() {
                return Err("Es ist kein Feld zugeordnet.".into());
            }
            let mut seen = std::collections::HashSet::new();
            for c in &cols {
                if !seen.insert(c.1.clone()) {
                    return Err(format!("Zielfeld „{}“ ist mehrfach zugeordnet.", c.1));
                }
            }
            (self.table.clone(), None, cols)
        };
        Ok(Plan {
            db: self.db.clone(),
            table,
            create,
            clear: self.clear && !self.new_table,
            cols,
            trim: self.trim,
            empty_null: self.empty_null,
        })
    }

    /// Zeichnet den Assistenten; `false` = geschlossen.
    pub fn show(&mut self, ctx: &egui::Context) -> bool {
        self.poll();
        if let Some(r) = self.job.as_ref().and_then(|j| j.rx.try_recv().ok()) {
            self.job = None;
            if r.is_ok() {
                self.done = Some(self.db.clone());
            }
            self.result = Some(r);
        }
        let mut open = true;
        let mut close = false;
        egui::Window::new(format!("Import-Assistent – {}", self.db))
            .id(egui::Id::unique("import_wizard"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([900.0, 620.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (s, t) in [(Step::File, "1. Datei & Format"), (Step::Target, "2. Ziel"), (Step::Fields, "3. Felder"), (Step::Run, "4. Import")] {
                        let txt = RichText::new(t);
                        ui.label(if self.step == s { txt.strong().underline() } else { txt.weak() });
                    }
                });
                ui.separator();
                let footer_h = 34.0;
                let body_h = (ui.available_height() - footer_h).max(200.0);
                ui.allocate_ui(egui::vec2(ui.available_width(), body_h), |ui| match self.step {
                    Step::File => self.ui_file(ui),
                    Step::Target => self.ui_target(ui),
                    Step::Fields => self.ui_fields(ui),
                    Step::Run => self.ui_run(ui),
                });
                ui.separator();
                ui.horizontal(|ui| {
                    let running = self.job.is_some();
                    let back = match self.step {
                        Step::File => None,
                        Step::Target => Some(Step::File),
                        Step::Fields => Some(Step::Target),
                        Step::Run => Some(Step::Fields),
                    };
                    if ui.add_enabled(back.is_some() && !running && self.result.as_ref().is_none_or(|r| r.is_err()), egui::Button::new("◀ Zurück")).clicked() {
                        self.result = None;
                        self.step = back.unwrap();
                    }
                    match self.step {
                        Step::File => {
                            if ui.add_enabled(!self.preview.is_empty(), egui::Button::new("Weiter ▶")).clicked() {
                                self.step = Step::Target;
                            }
                        }
                        Step::Target => {
                            let ok = if self.new_table { !self.new_name.trim().is_empty() } else { !self.table.is_empty() };
                            if ui.add_enabled(ok, egui::Button::new("Weiter ▶")).clicked() {
                                self.prepare_fields();
                                self.step = Step::Fields;
                            }
                        }
                        Step::Fields => {
                            let plan = self.plan();
                            if let Err(e) = &plan {
                                ui.colored_label(ui.visuals().warn_fg_color, e);
                            }
                            if ui.add_enabled(plan.is_ok(), egui::Button::new("Weiter ▶")).clicked() {
                                self.result = None;
                                self.step = Step::Run;
                            }
                        }
                        Step::Run => {}
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add_enabled(!running, egui::Button::new("Schließen")).clicked() {
                            close = true;
                        }
                    });
                });
            });
        if self.job.is_some() {
            // Während des Imports nicht schließen
            return true;
        }
        open && !close
    }

    fn ui_file(&mut self, ui: &mut egui::Ui) {
        let mut reload = false;
        let mut reparse = false;
        egui::Grid::new("imp_file").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label("Datei:");
            ui.horizontal(|ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(520.0).hint_text("Pfad zu .csv oder .txt"));
                reload |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                #[cfg(any(windows, target_os = "macos"))]
                if ui.button("…").on_hover_text("Datei wählen").clicked() {
                    if let Some(f) = rfd::FileDialog::new().add_filter("Text/CSV", &["csv", "txt", "tsv"]).add_filter("Alle Dateien", &["*"]).pick_file() {
                        self.path = f.display().to_string();
                        reload = true;
                    }
                }
                if ui.button("Laden").clicked() {
                    reload = true;
                }
            });
            ui.end_row();
            ui.label("Codierung:");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("imp_enc").selected_text(self.enc.label()).width(200.0).show_ui(ui, |ui| {
                    for e in Enc::ALL {
                        if ui.selectable_value(&mut self.enc, e, e.label()).changed() {
                            self.text = decode(&self.bytes, self.enc);
                            reparse = true;
                        }
                    }
                });
                if !self.bytes.is_empty() {
                    ui.weak(format!("erkannt: {}", self.enc_detected.label()));
                }
            });
            ui.end_row();
            ui.label("Zeilentrenner:");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("imp_eol").selected_text(self.eol.label()).width(200.0).show_ui(ui, |ui| {
                    for e in [Eol::Auto, Eol::CrLf, Eol::Lf, Eol::Cr] {
                        reparse |= ui.selectable_value(&mut self.eol, e, e.label()).changed();
                    }
                });
                if !self.bytes.is_empty() {
                    ui.weak(format!("erkannt: {}", self.eol_detected.label()));
                }
            });
            ui.end_row();
            ui.label("Spaltentrenner:");
            ui.horizontal(|ui| {
                for (c, t) in [(';', "Semikolon"), (',', "Komma"), ('\t', "Tab"), ('|', "|"), (' ', "Leerzeichen")] {
                    if ui.radio(self.delim == c && self.custom_delim.is_empty(), t).clicked() {
                        self.delim = c;
                        self.custom_delim.clear();
                        reparse = true;
                    }
                }
                ui.label("Andere:");
                if ui.add(egui::TextEdit::singleline(&mut self.custom_delim).desired_width(30.0).char_limit(1)).changed() {
                    if let Some(c) = self.custom_delim.chars().next() {
                        self.delim = c;
                    }
                    reparse = true;
                }
            });
            ui.end_row();
            ui.label("Textbegrenzer:");
            ui.horizontal(|ui| {
                for (q, t) in [(Some('"'), "\" (doppelt)"), (Some('\''), "' (einfach)"), (None, "keiner")] {
                    if ui.radio(self.quote == q, t).clicked() {
                        self.quote = q;
                        reparse = true;
                    }
                }
            });
            ui.end_row();
            ui.label("");
            ui.checkbox(&mut self.header, "Erste Zeile enthält Feldnamen");
            ui.end_row();
        });
        if reload {
            self.load_file();
        } else if reparse {
            self.reparse();
        }
        if let Some(e) = &self.file_error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        ui.separator();
        if self.preview.is_empty() {
            ui.weak("Datei wählen, um eine Vorschau zu sehen.");
            return;
        }
        ui.label(RichText::new(format!("Vorschau (erste {PREVIEW_ROWS} Zeilen)")).strong());
        preview_table(ui, "imp_prev", &self.preview, self.header);
    }

    fn ui_target(&mut self, ui: &mut egui::Ui) {
        ui.label(format!("Datenbank: {}", self.db));
        ui.add_space(6.0);
        ui.radio_value(&mut self.new_table, true, "In eine neue Tabelle importieren");
        ui.add_enabled_ui(self.new_table, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(24.0);
                ui.label("Tabellenname:");
                ui.add(egui::TextEdit::singleline(&mut self.new_name).desired_width(260.0));
            });
            ui.horizontal(|ui| {
                ui.add_space(24.0);
                ui.checkbox(&mut self.add_id, "Primärschlüssel „id“ (AUTO_INCREMENT) hinzufügen");
            });
        });
        ui.add_space(6.0);
        ui.radio_value(&mut self.new_table, false, "In eine vorhandene Tabelle importieren");
        let mut changed = false;
        ui.add_enabled_ui(!self.new_table, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(24.0);
                ui.label("Tabelle:");
                egui::ComboBox::from_id_salt("imp_table").selected_text(self.table.as_str()).width(260.0).height(400.0).show_ui(ui, |ui| {
                    for t in &self.tables {
                        changed |= ui.selectable_value(&mut self.table, t.clone(), t.as_str()).changed();
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.add_space(24.0);
                ui.checkbox(&mut self.clear, "Vorhandene Daten vorher löschen");
            });
        });
        if changed {
            self.load_cols();
        }
        ui.add_space(10.0);
        ui.separator();
        ui.checkbox(&mut self.trim, "Leerzeichen am Anfang/Ende entfernen");
        ui.checkbox(&mut self.empty_null, "Leere Felder als NULL übernehmen");
        ui.weak("Zahlen im deutschen Format (1.234,56) und Datumswerte (31.12.2024) werden für Zahlen-/Datumsfelder automatisch umgewandelt.");
    }

    fn ui_fields(&mut self, ui: &mut egui::Ui) {
        ui.label(format!(
            "{} Datenzeilen, {} Quellfelder → {}",
            self.data.len(),
            self.src_names.len(),
            if self.new_table { format!("neue Tabelle „{}“", self.new_name.trim()) } else { format!("Tabelle „{}“", self.table) }
        ));
        if !self.new_table && self.loading_cols {
            ui.spinner();
            return;
        }
        let samples: Vec<String> = (0..self.src_names.len())
            .map(|i| self.data.iter().filter_map(|r| r.get(i)).find(|v| !v.trim().is_empty()).cloned().unwrap_or_default())
            .collect();
        let n = self.src_names.len();
        let src = &self.src_names;
        let new_table = self.new_table;
        let new_cols = &mut self.new_cols;
        let map = &mut self.map;
        let targets = &self.target_cols;
        let mut tb = TableBuilder::new(ui)
            .id_salt("imp_fields")
            .striped(true)
            .resizable(true)
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::initial(180.0).at_least(80.0).clip(true))
            .column(Column::initial(200.0).at_least(80.0).clip(true));
        if new_table {
            tb = tb.column(Column::initial(70.0)).column(Column::initial(200.0).at_least(80.0)).column(Column::remainder().at_least(120.0));
        } else {
            tb = tb.column(Column::remainder().at_least(160.0));
        }
        tb.header(22.0, |mut h| {
            let heads: &[&str] = if new_table { &["Quellfeld", "Beispiel", "Übernehmen", "Feldname", "Typ"] } else { &["Quellfeld", "Beispiel", "Zielfeld"] };
            for t in heads {
                h.col(|ui| {
                    ui.strong(*t);
                });
            }
        })
        .body(|body| {
            body.rows(24.0, n, |mut row| {
                let i = row.index();
                row.col(|ui| {
                    ui.label(&src[i]);
                });
                row.col(|ui| {
                    ui.weak(samples[i].chars().take(60).collect::<String>());
                });
                if new_table {
                    let c = &mut new_cols[i];
                    row.col(|ui| {
                        ui.checkbox(&mut c.take, "");
                    });
                    row.col(|ui| {
                        ui.add_enabled(c.take, egui::TextEdit::singleline(&mut c.name).desired_width(f32::INFINITY));
                    });
                    row.col(|ui| {
                        ui.add_enabled(c.take, egui::TextEdit::singleline(&mut c.typ).desired_width(f32::INFINITY));
                    });
                } else {
                    row.col(|ui| {
                        let sel = match map[i] {
                            Some(t) => format!("{}  ({})", targets[t].0, targets[t].1),
                            None => "(überspringen)".into(),
                        };
                        egui::ComboBox::from_id_salt(("imp_map", i)).selected_text(sel).width(ui.available_width() - 6.0).height(400.0).show_ui(ui, |ui| {
                            ui.selectable_value(&mut map[i], None, "(überspringen)");
                            for (k, (name, typ)) in targets.iter().enumerate() {
                                ui.selectable_value(&mut map[i], Some(k), format!("{name}  ({typ})"));
                            }
                        });
                    });
                }
            });
        });
    }

    fn ui_run(&mut self, ui: &mut egui::Ui) {
        let plan = match self.plan() {
            Ok(p) => p,
            Err(e) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
        };
        ui.label(format!("{} Datenzeilen in „{}.{}“ importieren.", self.data.len(), plan.db, plan.table));
        if let Some(sql) = &plan.create {
            ui.label("Die Tabelle wird neu angelegt:");
            ui.label(RichText::new(sql).monospace().weak());
        }
        if plan.clear {
            ui.colored_label(ui.visuals().warn_fg_color, "Vorhandene Daten der Tabelle werden vorher gelöscht.");
        }
        ui.label(format!(
            "Zuordnung: {}",
            plan.cols.iter().map(|(s, t, _)| format!("{} → {t}", self.src_names[*s])).collect::<Vec<_>>().join(", ")
        ));
        ui.weak("Der Import läuft in einer Transaktion – bei einem Fehler werden keine Zeilen übernommen.");
        ui.add_space(8.0);
        match (&self.job, &self.result) {
            (Some(j), _) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("Importiere … {} von {} Zeilen", j.rows.load(Ordering::Relaxed), self.data.len()));
                    if ui.button("Abbrechen").clicked() {
                        j.cancel.store(true, Ordering::Relaxed);
                    }
                });
                let p = j.rows.load(Ordering::Relaxed) as f32 / self.data.len().max(1) as f32;
                ui.add(egui::ProgressBar::new(p).show_percentage());
            }
            (None, Some(Ok(n))) => {
                ui.colored_label(egui::Color32::from_rgb(0x2e, 0x9e, 0x44), format!("✔ {n} Zeilen importiert."));
            }
            (None, res) => {
                if let Some(Err(e)) = res {
                    ui.colored_label(ui.visuals().error_fg_color, RichText::new(e).monospace());
                }
                if ui.button(RichText::new("Import starten").strong()).clicked() {
                    self.result = None;
                    self.job = Some(start(self.cfg.clone(), plan, self.data.clone(), ui.ctx().clone()));
                }
            }
        }
    }
}

fn preview_table(ui: &mut egui::Ui, id: &str, rows: &[Vec<String>], header: bool) {
    let n = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    egui::ScrollArea::horizontal().id_salt((id, "h")).auto_shrink([false, false]).show(ui, |ui| {
        let mut tb = TableBuilder::new(ui)
            .id_salt(id)
            .striped(true)
            .resizable(true)
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(30.0));
        for _ in 0..n {
            tb = tb.column(Column::initial(120.0).at_least(30.0).clip(true));
        }
        let (head, body) = if header && !rows.is_empty() { (Some(&rows[0]), &rows[1..]) } else { (None, rows) };
        tb.header(22.0, |mut h| {
            h.col(|ui| {
                ui.weak("#");
            });
            for i in 0..n {
                h.col(|ui| {
                    let t = head.and_then(|r| r.get(i)).cloned().unwrap_or_else(|| format!("Spalte {}", i + 1));
                    ui.strong(t);
                });
            }
        })
        .body(|b| {
            b.rows(20.0, body.len().min(PREVIEW_ROWS), |mut row| {
                let r = row.index();
                row.col(|ui| {
                    ui.weak((r + 1).to_string());
                });
                for i in 0..n {
                    row.col(|ui| {
                        let v = body[r].get(i).map(|s| s.replace(['\r', '\n'], "↵")).unwrap_or_default();
                        ui.add(egui::Label::new(v).truncate().selectable(false));
                    });
                }
            });
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_quotes_and_newlines() {
        let t = "a;\"b;c\";\"x\"\"y\"\r\n1;\"mehr\nzeilig\";3\r\n\r\n";
        let r = parse(t, ';', Some('"'), Eol::Auto, usize::MAX);
        assert_eq!(r, vec![vec!["a", "b;c", "x\"y"], vec!["1", "mehr\nzeilig", "3"]]);
        assert_eq!(detect_eol(t), Eol::CrLf);
    }

    #[test]
    fn delimiter_detection() {
        assert_eq!(detect_delim("a;b;c\n1;2;3\n4;5;6\n", Some('"')), ';');
        assert_eq!(detect_delim("a,b,c\n1,\"2;x\",3\n", Some('"')), ',');
        assert_eq!(detect_delim("a\tb\n1\t2\n", Some('"')), '\t');
        assert_eq!(detect_delim("name;preis\nApfel;1,50\nBirne;2,10\n", Some('"')), ';');
    }

    #[test]
    fn encoding_detection() {
        assert_eq!(detect_enc("Grüße".as_bytes()), Enc::Utf8);
        assert_eq!(detect_enc(b"Gr\xFC\xDFe"), Enc::Win1252);
        assert_eq!(decode(b"Gr\xFC\xDFe \x80", Enc::Win1252), "Grüße €");
        let le: Vec<u8> = [0xFF, 0xFE].into_iter().chain("ä;b".encode_utf16().flat_map(|u| u.to_le_bytes())).collect();
        assert_eq!(detect_enc(&le), Enc::Utf16Le);
        assert_eq!(decode(&le, Enc::Utf16Le), "ä;b");
    }

    #[test]
    fn value_conversion() {
        assert_eq!(norm_number("1.234,56").as_deref(), Some("1234.56"));
        assert_eq!(norm_number("1,5").as_deref(), Some("1.5"));
        assert_eq!(norm_number("1,234.5").as_deref(), Some("1234.5"));
        assert_eq!(norm_number("abc"), None);
        assert_eq!(norm_date("31.12.2024"), Some(("2024-12-31".into(), false)));
        assert_eq!(norm_date("1.2.2024 8:05"), Some(("2024-02-01 08:05:00".into(), true)));
        assert_eq!(norm_date("2024-02-01"), Some(("2024-02-01".into(), false)));
        assert_eq!(norm_date("12.13"), None);
        assert_eq!(convert(" ", Kind::Text, true, true), None);
        assert_eq!(convert("", Kind::Text, true, false).as_deref(), Some(""));
        assert_eq!(convert("3,5", Kind::Number, true, true).as_deref(), Some("3.5"));
    }

    #[test]
    fn type_guessing() {
        let g = |v: &[&str]| guess_type(v.iter().map(|s| s.to_string()));
        assert_eq!(g(&["1", "2", ""]), "INT");
        assert_eq!(g(&["1", "9999999999"]), "BIGINT");
        assert_eq!(g(&["1,5", "12,25"]), "DECIMAL(4,2)");
        assert_eq!(g(&["31.12.2024", "01.01.2025"]), "DATE");
        assert_eq!(g(&["31.12.2024 10:00", "01.01.2025"]), "DATETIME");
        assert_eq!(g(&["abc", "1"]), "VARCHAR(255)");
        assert_eq!(g(&[]), "VARCHAR(255)");
    }

    #[test]
    fn unique_column_names() {
        let h = vec!["Name".to_string(), "".into(), "name".into()];
        assert_eq!(column_names(Some(&h), 4), vec!["Name", "spalte_2", "name_2", "spalte_4"]);
    }

    /// Gegen echten Server: neue Tabelle aus CSV anlegen und befüllen.
    #[test]
    fn itest_import() {
        use crate::db::itest::cfg;
        let Some(cfg) = cfg() else { return };
        let text = "id;name;preis;datum\n1;Apfel;1,50;31.12.2024\n2;\"Birne; groß\";2,10;01.01.2025\n";
        let mut rows = parse(text, ';', Some('"'), Eol::Auto, usize::MAX);
        rows.remove(0);
        let mut c = db::connect(&cfg).unwrap();
        c.query_drop("DROP TABLE IF EXISTS shop.mdbm_import_test").unwrap();
        let plan = Plan {
            db: "shop".into(),
            table: "mdbm_import_test".into(),
            create: Some("CREATE TABLE mdbm_import_test (id INT, name VARCHAR(50), preis DECIMAL(5,2), datum DATE)".into()),
            clear: false,
            cols: vec![(0, "id".into(), Kind::Number), (1, "name".into(), Kind::Text), (2, "preis".into(), Kind::Number), (3, "datum".into(), Kind::Date)],
            trim: true,
            empty_null: true,
        };
        let n = run(&cfg, &plan, &rows, &AtomicU64::new(0), &AtomicBool::new(false), &egui::Context::default()).unwrap();
        assert_eq!(n, 2);
        let r: Vec<(String, String, String)> =
            c.query("SELECT name, CAST(preis AS CHAR), CAST(datum AS CHAR) FROM shop.mdbm_import_test ORDER BY id").unwrap();
        assert_eq!(r[1], ("Birne; groß".into(), "2.10".into(), "2025-01-01".into()));
        c.query_drop("DROP TABLE shop.mdbm_import_test").unwrap();
    }
}
