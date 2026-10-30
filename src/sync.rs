//! Strukturabgleich zwischen zwei Datenbanken (auch über verschiedene Verbindungen):
//! Tabellen, Ansichten, Prozeduren, Funktionen und Ereignisse vergleichen, auswählen und das Skript
//! (`ALTER`/`CREATE`/`DROP`) erzeugen, das das Ziel an die Quelle angleicht. Das Skript lässt
//! sich kopieren, im SQL-Editor des Ziels öffnen oder direkt ausführen; erfolgreich
//! angeglichene Objekte verschwinden aus der Liste.

use crate::db::{qi, ConnConfig, JobResult, Mode, Stmt, Worker};
use crate::designtab::{self, SchemaInfo};
use crate::icons::{self, Icon};
use crate::sqlhl;
use eframe::egui::{self, Color32, RichText, Sense};
use egui_extras::{Column, TableBuilder};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};

/// Art eines abgeglichenen Objekts (Reihenfolge = Reihenfolge in Liste und Skript).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Kind {
    Table,
    View,
    Procedure,
    Function,
    Event,
}

impl Kind {
    fn sql(self) -> &'static str {
        match self {
            Kind::Table => "TABLE",
            Kind::View => "VIEW",
            Kind::Procedure => "PROCEDURE",
            Kind::Function => "FUNCTION",
            Kind::Event => "EVENT",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Kind::Table => "Tabelle",
            Kind::View => "Ansicht",
            Kind::Procedure => "Prozedur",
            Kind::Function => "Funktion",
            Kind::Event => "Ereignis",
        }
    }
    fn icon(self) -> Icon {
        match self {
            Kind::Table => Icon::Table,
            Kind::View => Icon::View,
            Kind::Procedure => Icon::Procedure,
            Kind::Function => Icon::Function,
            Kind::Event => Icon::Event,
        }
    }
}

/// Objekt-Schlüssel: Art + Name (Prozeduren und Funktionen haben getrennte Namensräume).
type Key = (Kind, String);

/// `CREATE`-Anweisung (ohne DEFINER) bzw. Fehlermeldung je Ansicht/Routine/Ereignis.
type Routines = BTreeMap<Key, Result<String, String>>;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Verbindung, wie sie dem Tab bekannt ist.
#[derive(Clone)]
pub struct ConnRef {
    pub uid: u64,
    pub name: String,
    pub cfg: ConnConfig,
}

#[derive(Default)]
struct Side {
    conn: Option<usize>,
    db: String,
    dbs: Vec<String>,
    worker: Option<Worker>,
    tag_dbs: u64,
    tag_schema: u64,
    schema: Option<SchemaInfo>,
    /// Liste der Routinen/Ereignisse, danach je ein `SHOW CREATE …`.
    tag_list: u64,
    tag_rout: u64,
    rout_keys: Vec<Key>,
    routines: Option<Routines>,
    error: Option<String>,
}

impl Side {
    fn set_conn(&mut self, i: usize, conns: &[ConnRef], ctx: &egui::Context) {
        if self.conn == Some(i) {
            return;
        }
        let mut w = Worker::spawn(conns[i].cfg.clone(), ctx.clone());
        self.tag_dbs = w.query(None, &["SHOW DATABASES"]);
        *self = Side { conn: Some(i), worker: Some(w), tag_dbs: self.tag_dbs, ..Default::default() };
    }

    /// Datenbankliste neu lesen (z. B. nach dem Anlegen einer Datenbank).
    fn reload_dbs(&mut self) {
        if let Some(w) = &mut self.worker {
            self.tag_dbs = w.query(None, &["SHOW DATABASES"]);
        }
    }

    fn load(&mut self) {
        self.schema = None;
        self.routines = None;
        self.error = None;
        if let Some(w) = &mut self.worker {
            self.tag_schema = w.send(None, designtab::schema_stmts(&self.db), Mode::StopOnError, usize::MAX);
            let p = || vec![Some(self.db.clone())];
            self.tag_list = w.send(
                None,
                vec![
                    Stmt::with(
                        "SELECT ROUTINE_TYPE, ROUTINE_NAME FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = ?",
                        p(),
                    ),
                    Stmt::with("SELECT EVENT_NAME FROM information_schema.EVENTS WHERE EVENT_SCHEMA = ?", p()),
                    Stmt::with("SELECT TABLE_NAME FROM information_schema.VIEWS WHERE TABLE_SCHEMA = ?", p()),
                ],
                Mode::StopOnError,
                usize::MAX,
            );
        }
    }

    /// Strukturen werden gelesen.
    fn loading(&self) -> bool {
        self.tag_schema != 0 || self.tag_list != 0 || self.tag_rout != 0
    }

    fn ready(&self) -> bool {
        self.schema.is_some() && self.routines.is_some()
    }

    fn busy(&self) -> bool {
        self.tag_dbs != 0 || self.loading()
    }

    /// Routinen/Ereignisse aufgelistet → `SHOW CREATE` für jedes (Continue: fehlende Rechte
    /// betreffen nur das einzelne Objekt).
    fn list_done(&mut self, res: &JobResult) {
        let mut keys = Vec::new();
        if let Some(rs) = res.set(0) {
            for r in 0..rs.rows.len() {
                let kind = if rs.get(r, "ROUTINE_TYPE") == "FUNCTION" { Kind::Function } else { Kind::Procedure };
                keys.push((kind, rs.get(r, "ROUTINE_NAME").to_string()));
            }
        }
        if let Some(rs) = res.set(1) {
            keys.extend(rs.rows.iter().filter_map(|r| r[0].clone()).map(|n| (Kind::Event, n)));
        }
        if let Some(rs) = res.set(2) {
            keys.extend(rs.rows.iter().filter_map(|r| r[0].clone()).map(|n| (Kind::View, n)));
        }
        let Some(w) = self.worker.as_mut().filter(|_| !keys.is_empty()) else {
            self.routines = Some(Routines::new());
            return;
        };
        let stmts = keys
            .iter()
            .map(|(k, n)| Stmt::plain(format!("SHOW CREATE {} {}.{}", k.sql(), qi(&self.db), qi(n))))
            .collect();
        self.tag_rout = w.send(None, stmts, Mode::Continue, usize::MAX);
        self.rout_keys = keys;
    }

    fn routines_done(&mut self, res: &JobResult) {
        let mut m = Routines::new();
        for (i, key) in std::mem::take(&mut self.rout_keys).into_iter().enumerate() {
            let out = res.outs.get(i);
            let v = match out.and_then(|o| o.error.clone()) {
                Some(e) => Err(e),
                None => {
                    let rs = out.and_then(|o| o.sets.first());
                    let col = rs.and_then(|rs| rs.columns.iter().position(|c| c.name.starts_with("Create ")));
                    match rs.zip(col).and_then(|(rs, c)| rs.rows.first().and_then(|r| r[c].clone())) {
                        Some(sql) => {
                            let sql = crate::dump::strip_definer(sql.trim());
                            // Ansichten: MariaDB qualifiziert jede Tabelle mit der eigenen Datenbank
                            Ok(if key.0 == Kind::View { unqualify(&sql, &self.db) } else { sql })
                        }
                        None => Err(format!("{} {}: Definition nicht lesbar (fehlende Rechte?)", key.0.label(), key.1)),
                    }
                }
            };
            m.insert(key, v);
        }
        self.routines = Some(m);
    }

    /// Fremde Ergebnisse (z. B. von „Ausführen“) landen in `other`.
    fn poll(&mut self, other: &mut Vec<JobResult>) -> bool {
        let mut done = false;
        while let Some(res) = self.worker.as_mut().and_then(|w| w.poll()) {
            let err = res.error.clone().or_else(|| res.outs.iter().find_map(|o| o.error.clone()));
            if res.tag == self.tag_dbs {
                self.tag_dbs = 0;
                self.error = err;
                if let Some(rs) = res.set(0) {
                    self.dbs = rs.rows.iter().filter_map(|r| r[0].clone()).collect();
                }
            } else if res.tag == self.tag_schema {
                self.tag_schema = 0;
                match err {
                    Some(e) => self.error = Some(e),
                    None => self.schema = Some(designtab::parse_schema(&res, &self.db)),
                }
                done = true;
            } else if res.tag == self.tag_list {
                self.tag_list = 0;
                match err {
                    Some(e) => {
                        self.error = Some(e);
                        self.routines = Some(Routines::new());
                    }
                    None => {
                        self.list_done(&res);
                    }
                }
                done = true;
            } else if res.tag == self.tag_rout {
                self.tag_rout = 0;
                self.routines_done(&res);
                done = true;
            } else {
                other.push(res);
            }
        }
        done
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Equal,
    Diff,
    OnlySrc,
    OnlyTgt,
    Error,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Equal => "gleich",
            Status::Diff => "unterschiedlich",
            Status::OnlySrc => "nur in Quelle (CREATE)",
            Status::OnlyTgt => "nur im Ziel (DROP)",
            Status::Error => "Fehler",
        }
    }
    fn color(self, ui: &egui::Ui) -> Color32 {
        match self {
            Status::Equal => ui.visuals().weak_text_color(),
            Status::Diff => Color32::from_rgb(0xd8, 0x8a, 0x10),
            Status::OnlySrc => Color32::from_rgb(0x2e, 0xa0, 0x43),
            Status::OnlyTgt | Status::Error => Color32::from_rgb(0xd0, 0x30, 0x30),
        }
    }
}

/// Zeile der Strukturdifferenz: gleich, nur im Ziel (−), nur in der Quelle (+).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Dl {
    Same,
    Del,
    Add,
}

/// Zeilenweise Differenz (LCS) von `a` (alt = Ziel) nach `b` (neu = Quelle).
fn line_diff(a: &[String], b: &[String]) -> Vec<(Dl, String)> {
    let (n, m) = (a.len(), b.len());
    let mut l = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            l[i][j] = if a[i] == b[j] { l[i + 1][j + 1] + 1 } else { l[i + 1][j].max(l[i][j + 1]) };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::with_capacity(n.max(m));
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            out.push((Dl::Same, a[i].clone()));
            i += 1;
            j += 1;
        } else if i < n && (j == m || l[i + 1][j] >= l[i][j + 1]) {
            out.push((Dl::Del, a[i].clone()));
            i += 1;
        } else {
            out.push((Dl::Add, b[j].clone()));
            j += 1;
        }
    }
    out
}

/// Verweise `` `db`. `` auf die eigene Datenbank entfernen (Ansichten aus `SHOW CREATE VIEW`).
fn unqualify(sql: &str, db: &str) -> String {
    sql.replace(&format!("{}.", qi(db)), "")
}

/// SQL für eine Ansicht/Routine/ein Ereignis: neu anlegen, löschen oder ersetzen
/// (Ansichten per `CREATE OR REPLACE`, sonst DROP + CREATE).
fn routine_sql(kind: Kind, name: &str, src: Option<&String>, tgt: Option<&String>) -> Vec<String> {
    let norm = |s: &str| s.lines().map(str::trim_end).collect::<Vec<_>>().join("\n");
    match (src, tgt) {
        (None, None) => Vec::new(),
        (Some(s), None) => vec![s.clone()],
        (None, Some(_)) => vec![format!("DROP {} {}", kind.sql(), qi(name))],
        (Some(s), Some(t)) if norm(s) == norm(t) => Vec::new(),
        (Some(s), Some(_)) if kind == Kind::View => {
            vec![s.strip_prefix("CREATE ").map_or_else(|| s.clone(), |r| format!("CREATE OR REPLACE {r}"))]
        }
        (Some(s), Some(_)) => vec![format!("DROP {} IF EXISTS {}", kind.sql(), qi(name)), s.clone()],
    }
}

struct Entry {
    kind: Kind,
    name: String,
    status: Status,
    /// Anweisungen bzw. Fehlermeldung.
    sql: Result<Vec<String>, String>,
    checked: bool,
    /// Strukturdifferenz Ziel → Quelle.
    diff: Vec<(Dl, String)>,
    /// Fehler beim direkten Ausführen.
    exec_error: Option<String>,
}

impl Entry {
    fn is(&self, k: &Key) -> bool {
        self.kind == k.0 && self.name == k.1
    }
    fn key(&self) -> Key {
        (self.kind, self.name.clone())
    }
}

/// Status aus Ergebnis und Vorkommen in Quelle (`a`) und Ziel (`b`).
fn status_of(sql: &Result<Vec<String>, String>, a: bool, b: bool) -> Status {
    match (sql, a, b) {
        (Err(_), _, _) => Status::Error,
        (_, true, false) => Status::OnlySrc,
        (_, false, true) => Status::OnlyTgt,
        (Ok(v), _, _) if v.is_empty() => Status::Equal,
        _ => Status::Diff,
    }
}

pub struct SyncTab {
    id: u64,
    ctx: egui::Context,
    conns: Vec<ConnRef>,
    src: Side,
    tgt: Side,
    entries: Vec<Entry>,
    compared: Option<(String, String)>,
    hide_equal: bool,
    filter: String,
    focus: Option<usize>,
    open_query: Option<(u64, String, String)>,
    /// Detailbereich zeigt die Strukturdifferenz statt des SQL.
    show_diff: bool,
    /// Laufende Ausführungen: Tag → Objekt.
    exec: HashMap<u64, Key>,
    /// Objekte, deren Ausführung noch bestätigt werden muss.
    confirm: Option<Vec<Key>>,
}

impl SyncTab {
    /// `pre` = vorbelegte Quelle (Verbindungsindex in `conns`, Datenbank).
    pub fn new(conns: Vec<ConnRef>, ctx: &egui::Context, pre: Option<(usize, String)>) -> Self {
        let mut t = Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            ctx: ctx.clone(),
            conns,
            src: Side::default(),
            tgt: Side::default(),
            entries: Vec::new(),
            compared: None,
            hide_equal: true,
            filter: String::new(),
            focus: None,
            open_query: None,
            show_diff: false,
            exec: HashMap::new(),
            confirm: None,
        };
        if let Some((ci, db)) = pre.filter(|p| p.0 < t.conns.len()) {
            t.src.set_conn(ci, &t.conns, ctx);
            t.src.db = db;
            t.tgt.set_conn(ci, &t.conns, ctx);
        }
        t
    }

    pub fn title(&self) -> String {
        match &self.compared {
            Some((s, t)) => format!("Abgleich {s} → {t}"),
            None => "Strukturabgleich".into(),
        }
    }

    /// Gewählte Quelle (Verbindungsname, Datenbank) – zum Speichern der Ansicht.
    pub fn source(&self) -> Option<(String, String)> {
        self.src.conn.map(|i| (self.conns[i].name.clone(), self.src.db.clone()))
    }

    /// Anfrage „Skript im SQL-Editor öffnen“: (Verbindungs-UID, Datenbank, SQL).
    pub fn take_open_query(&mut self) -> Option<(u64, String, String)> {
        self.open_query.take()
    }

    fn side_label(&self, s: &Side) -> String {
        let c = s.conn.map(|i| self.conns[i].name.as_str()).unwrap_or("?");
        format!("{c}/{}", s.db)
    }

    fn compare(&mut self) {
        self.entries.clear();
        self.focus = None;
        self.compared = None;
        self.src.load();
        self.tgt.load();
    }

    fn build(&mut self) {
        let (Some(s), Some(t)) = (&self.src.schema, &self.tgt.schema) else { return };
        let (Some(rs), Some(rt)) = (&self.src.routines, &self.tgt.routines) else { return };
        let mut names: Vec<&String> = s.tables.keys().chain(t.tables.keys()).collect();
        names.sort_by_key(|n| n.to_lowercase());
        names.dedup();
        let tdb = self.tgt.db.clone();
        self.entries = names
            .into_iter()
            .map(|n| {
                let (a, b) = (s.tables.get(n), t.tables.get(n));
                let cs = |c: &str| t.charset_of(c).or_else(|| s.charset_of(c));
                let sql = designtab::sync_sql(a, b, n, &tdb, &cs);
                let lines = |d: Option<&designtab::TableDef>, db: &str| {
                    d.map(|d| designtab::struct_lines(d, n, db, &cs)).unwrap_or_default()
                };
                let diff = line_diff(&lines(b, &tdb), &lines(a, &self.src.db));
                let status = status_of(&sql, a.is_some(), b.is_some());
                Entry {
                    kind: Kind::Table,
                    name: n.clone(),
                    status,
                    sql,
                    checked: matches!(status, Status::Diff | Status::OnlySrc),
                    diff,
                    exec_error: None,
                }
            })
            .collect();

        let mut keys: Vec<&Key> = rs.keys().chain(rt.keys()).collect();
        keys.sort_by_key(|k| (k.0, k.1.to_lowercase()));
        keys.dedup();
        for k in keys {
            let (a, b) = (rs.get(k), rt.get(k));
            let sql = match (a, b) {
                (Some(Err(e)), _) | (_, Some(Err(e))) => Err(e.clone()),
                _ => Ok(routine_sql(k.0, &k.1, a.and_then(|r| r.as_ref().ok()), b.and_then(|r| r.as_ref().ok()))),
            };
            let lines = |r: Option<&Result<String, String>>| -> Vec<String> {
                r.and_then(|r| r.as_ref().ok()).map(|s| s.lines().map(|l| l.trim_end().to_string()).collect()).unwrap_or_default()
            };
            let status = status_of(&sql, a.is_some(), b.is_some());
            self.entries.push(Entry {
                kind: k.0,
                name: k.1.clone(),
                status,
                sql,
                checked: matches!(status, Status::Diff | Status::OnlySrc),
                diff: line_diff(&lines(b), &lines(a)),
                exec_error: None,
            });
        }
        self.compared = Some((self.side_label(&self.src), self.side_label(&self.tgt)));
    }

    /// Angehakte Tabellen (bzw. nur die fokussierte).
    fn selected(&self) -> Vec<&Entry> {
        match self.focus {
            Some(i) => self.entries.get(i).into_iter().collect(),
            None => self.entries.iter().filter(|e| e.checked).collect(),
        }
    }

    /// Skript für die angehakten Tabellen (bzw. nur die fokussierte).
    fn script(&self) -> String {
        let mut out = String::new();
        let list = self.selected();
        if let Some((s, t)) = &self.compared {
            out += &format!(
                "-- Strukturabgleich: Quelle {s} → Ziel {t}\n-- Erstellt: {} (UTC)\n\nSET FOREIGN_KEY_CHECKS = 0;\n\n",
                crate::queries::fmt_time(std::time::SystemTime::now())
            );
        }
        for e in list {
            out += &format!("-- {} {} ({})\n", e.kind.label(), e.name, e.status.label());
            match &e.sql {
                Ok(v) if v.is_empty() => out += "-- keine Änderungen\n",
                Ok(v) => {
                    for s in v {
                        // Anweisungen mit eigenem ';' (Trigger) brauchen einen anderen Trenner
                        if s.contains(';') {
                            out += &format!("DELIMITER ;;\n{s} ;;\nDELIMITER ;\n");
                        } else {
                            out += &format!("{s};\n");
                        }
                    }
                }
                Err(err) => out += &format!("-- Fehler: {err}\n"),
            }
            out += "\n";
        }
        out += "SET FOREIGN_KEY_CHECKS = 1;\n";
        out
    }

    /// Führt das SQL der Objekte `keys` auf dem Ziel aus – je Objekt ein eigener Job,
    /// damit nur erfolgreich angeglichene aus der Liste verschwinden.
    fn execute(&mut self, keys: Vec<Key>) {
        let Some(w) = &mut self.tgt.worker else { return };
        for key in keys {
            let Some(e) = self.entries.iter_mut().find(|e| e.is(&key)) else { continue };
            let Ok(v) = &e.sql else { continue };
            if v.is_empty() {
                continue;
            }
            let mut stmts = vec![Stmt::plain("SET FOREIGN_KEY_CHECKS = 0")];
            stmts.extend(v.iter().map(Stmt::plain));
            stmts.push(Stmt::plain("SET FOREIGN_KEY_CHECKS = 1"));
            e.exec_error = None;
            let tag = w.send(Some(self.tgt.db.clone()), stmts, Mode::StopOnError, 0);
            self.exec.insert(tag, key);
        }
    }

    fn exec_done(&mut self, results: Vec<JobResult>) {
        let focused = self.focus.and_then(|i| self.entries.get(i)).map(Entry::key);
        for res in results {
            let Some(key) = self.exec.remove(&res.tag) else { continue };
            let err = res.error.clone().or_else(|| res.outs.iter().find_map(|o| o.error.clone()));
            match err {
                None => self.entries.retain(|e| !e.is(&key)),
                Some(err) => {
                    if let Some(e) = self.entries.iter_mut().find(|e| e.is(&key)) {
                        e.exec_error = Some(err);
                    }
                }
            }
        }
        self.focus = focused.and_then(|k| self.entries.iter().position(|e| e.is(&k)));
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let mut other = Vec::new();
        let a = self.src.poll(&mut Vec::new());
        let b = self.tgt.poll(&mut other);
        if (a || b) && self.src.ready() && self.tgt.ready() {
            self.build();
        }
        if !other.is_empty() {
            self.exec_done(other);
        }
        self.ui_confirm(ui);

        self.ui_sides(ui);
        for e in [&self.src.error, &self.tgt.error].into_iter().flatten() {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        ui.separator();
        if self.entries.is_empty() {
            if self.src.loading() || self.tgt.loading() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Lese Strukturen …");
                });
            } else {
                ui.weak("Quelle und Ziel wählen, dann „Vergleichen“. Das Skript gleicht das Ziel an die Quelle an.");
            }
            return;
        }
        self.ui_result(ui);
    }

    fn ui_sides(&mut self, ui: &mut egui::Ui) {
        let mut go = false;
        let mut swap = false;
        egui::Grid::new(("sync_sides", self.id)).num_columns(4).spacing([8.0, 6.0]).show(ui, |ui| {
            for (k, label) in [(0, "Quelle:"), (1, "Ziel:")] {
                ui.strong(label);
                let conns = &self.conns;
                let side = if k == 0 { &mut self.src } else { &mut self.tgt };
                let cur = side.conn.map(|i| conns[i].name.clone()).unwrap_or_else(|| "Verbindung wählen …".into());
                let mut pick = None;
                egui::ComboBox::from_id_salt(("sync_conn", self.id, k)).selected_text(cur).width(220.0).show_ui(ui, |ui| {
                    for (i, c) in conns.iter().enumerate() {
                        if ui.selectable_label(side.conn == Some(i), &c.name).clicked() {
                            pick = Some(i);
                        }
                    }
                });
                if let Some(i) = pick {
                    side.set_conn(i, conns, &self.ctx);
                }
                let cur_db = if side.db.is_empty() { "Datenbank wählen …".to_string() } else { side.db.clone() };
                ui.add_enabled_ui(side.conn.is_some(), |ui| {
                    egui::ComboBox::from_id_salt(("sync_db", self.id, k)).selected_text(cur_db).width(220.0).show_ui(ui, |ui| {
                        for d in &side.dbs {
                            if ui.selectable_label(&side.db == d, d).clicked() {
                                side.db = d.clone();
                                side.schema = None;
                                side.routines = None;
                            }
                        }
                    });
                });
                if side.busy() {
                    ui.spinner();
                } else if ui
                    .add_enabled(side.conn.is_some(), egui::Button::new(icons::atom(Icon::Refresh, 16.0)))
                    .on_hover_text("Datenbankliste aktualisieren")
                    .clicked()
                {
                    side.reload_dbs();
                }
                ui.end_row();
            }
        });
        ui.horizontal(|ui| {
            let ready = !self.src.db.is_empty() && !self.tgt.db.is_empty();
            let same = self.src.conn == self.tgt.conn && self.src.db == self.tgt.db;
            let busy = self.src.loading() || self.tgt.loading() || !self.exec.is_empty();
            if ui.add_enabled(ready && !same && !busy, icons::button(Icon::Run, "Vergleichen")).clicked() {
                go = true;
            }
            if ui.add_enabled(!busy, egui::Button::new("⇅ Quelle/Ziel tauschen")).clicked() {
                swap = true;
            }
            if same && ready {
                ui.colored_label(ui.visuals().warn_fg_color, "Quelle und Ziel sind identisch.");
            }
        });
        if swap {
            std::mem::swap(&mut self.src, &mut self.tgt);
            self.entries.clear();
            self.compared = None;
            if self.src.ready() && self.tgt.ready() {
                self.build();
            }
        }
        if go {
            self.compare();
        }
    }

    fn ui_result(&mut self, ui: &mut egui::Ui) {
        let count = |s: Status| self.entries.iter().filter(|e| e.status == s).count();
        let summary = format!(
            "{} gleich · {} unterschiedlich · {} nur in Quelle · {} nur im Ziel{}",
            count(Status::Equal),
            count(Status::Diff),
            count(Status::OnlySrc),
            count(Status::OnlyTgt),
            match count(Status::Error) {
                0 => String::new(),
                n => format!(" · {n} Fehler"),
            }
        );
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Objekte filtern").desired_width(180.0));
            ui.checkbox(&mut self.hide_equal, "Gleiche ausblenden");
            if ui.button("Alle").clicked() {
                self.set_all(true);
            }
            if ui.button("Keine").clicked() {
                self.set_all(false);
            }
            ui.weak(summary);
        });

        let f = self.filter.to_lowercase();
        let vis: Vec<usize> = (0..self.entries.len())
            .filter(|&i| {
                let e = &self.entries[i];
                !(self.hide_equal && e.status == Status::Equal) && (f.is_empty() || e.name.to_lowercase().contains(&f))
            })
            .collect();

        egui::Panel::left(egui::Id::unique(("sync_list", self.id)))
            .resizable(true)
            .default_size(380.0)
            .min_size(220.0)
            .show(ui, |ui| {
                let mut focus = None;
                TableBuilder::new(ui)
                    .id_salt(("sync_tbl", self.id))
                    .striped(true)
                    .sense(Sense::click())
                    .auto_shrink([false, false])
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .column(Column::exact(24.0))
                    .column(Column::initial(200.0).at_least(80.0).clip(true))
                    .column(Column::remainder().at_least(80.0).clip(true))
                    .header(22.0, |mut h| {
                        h.col(|_| {});
                        h.col(|ui| {
                            ui.strong("Objekt");
                        });
                        h.col(|ui| {
                            ui.strong("Status");
                        });
                    })
                    .body(|body| {
                        body.rows(20.0, vis.len(), |mut row| {
                            let i = vis[row.index()];
                            row.set_selected(self.focus == Some(i));
                            let e = &mut self.entries[i];
                            let (_, r0) = row.col(|ui| {
                                ui.add_enabled(e.status != Status::Equal && e.status != Status::Error, egui::Checkbox::without_text(&mut e.checked));
                            });
                            let (_, r1) = row.col(|ui| {
                                icons::show(ui, e.kind.icon(), 16.0).on_hover_text(e.kind.label());
                                ui.add(egui::Label::new(&e.name).selectable(false).truncate());
                            });
                            let st = e.status;
                            let exec_err = e.exec_error.clone();
                            let (_, r2) = row.col(|ui| match exec_err {
                                Some(err) => {
                                    let t = RichText::new("Ausführung fehlgeschlagen").color(Status::Error.color(ui));
                                    ui.add(egui::Label::new(t).selectable(false).truncate()).on_hover_text(err);
                                }
                                None => {
                                    ui.add(egui::Label::new(RichText::new(st.label()).color(st.color(ui))).selectable(false).truncate());
                                }
                            });
                            if r0.clicked() || r1.clicked() || r2.clicked() {
                                focus = Some(i);
                            }
                        });
                    });
                if let Some(i) = focus {
                    self.focus = if self.focus == Some(i) { None } else { Some(i) };
                }
            });

        // Rechts: Skript
        let script = self.script();
        let n = self.entries.iter().filter(|e| e.checked).count();
        let runnable: Vec<Key> =
            self.selected().iter().filter(|e| matches!(&e.sql, Ok(v) if !v.is_empty())).map(|e| e.key()).collect();
        let mut show_diff = self.show_diff;
        ui.horizontal(|ui| {
            match self.focus.and_then(|i| self.entries.get(i)) {
                Some(e) => {
                    ui.strong(format!("{} {}", e.kind.label(), e.name));
                    ui.selectable_value(&mut show_diff, false, "SQL");
                    ui.selectable_value(&mut show_diff, true, "Unterschied");
                    if ui.button("Gesamtes Skript anzeigen").clicked() {
                        self.focus = None;
                    }
                }
                None => {
                    ui.strong(format!("Skript für {n} Objekt(e)"));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !self.exec.is_empty() {
                    ui.spinner();
                } else if ui
                    .add_enabled(!runnable.is_empty() && self.tgt.worker.is_some(), icons::button(Icon::Run, "Ausführen"))
                    .on_hover_text("Führt das SQL direkt auf der Zieldatenbank aus; erfolgreich angeglichene Objekte verschwinden aus der Liste.")
                    .clicked()
                {
                    self.confirm = Some(runnable.clone());
                }
                let tgt_uid = self.tgt.conn.map(|i| self.conns[i].uid);
                if ui
                    .add_enabled(tgt_uid.is_some(), icons::button(Icon::NewQuery, "Im SQL-Editor (Ziel) öffnen"))
                    .on_hover_text("Öffnet das Skript in einer neuen Abfrage auf der Zieldatenbank – ausgeführt wird erst dort.")
                    .clicked()
                {
                    self.open_query = tgt_uid.map(|u| (u, self.tgt.db.clone(), script.clone()));
                }
                if ui.add(icons::button(Icon::Copy, "Kopieren")).clicked() {
                    ui.ctx().copy_text(script.clone());
                }
            });
        });
        self.show_diff = show_diff;

        let focused = self.focus.and_then(|i| self.entries.get(i));
        if let Some(err) = focused.and_then(|e| e.exec_error.as_ref()) {
            ui.colored_label(ui.visuals().error_fg_color, format!("Ausführung fehlgeschlagen: {err}"));
        }
        if let (Some(e), true) = (focused, show_diff) {
            Self::ui_diff(ui, self.id, &e.diff);
            return;
        }
        egui::ScrollArea::both().id_salt(("sync_sql", self.id)).auto_shrink([false, false]).show(ui, |ui| {
            let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, w: f32| sqlhl::layouter(ui, buf, w);
            let mut t = script.as_str();
            ui.add(egui::TextEdit::multiline(&mut t).code_editor().desired_width(f32::INFINITY).layouter(&mut layouter));
        });
    }

    /// Strukturdifferenz: − Zeilen nur im Ziel, + Zeilen nur in der Quelle.
    fn ui_diff(ui: &mut egui::Ui, id: u64, diff: &[(Dl, String)]) {
        let add = Color32::from_rgb(0x2e, 0xa0, 0x43);
        let del = Color32::from_rgb(0xd0, 0x30, 0x30);
        ui.horizontal(|ui| {
            ui.colored_label(del, "− nur im Ziel");
            ui.colored_label(add, "+ nur in der Quelle");
        });
        egui::ScrollArea::both().id_salt(("sync_diff", id)).auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (k, line) in diff {
                let (p, c) = match k {
                    Dl::Same => (' ', None),
                    Dl::Del => ('−', Some(del)),
                    Dl::Add => ('+', Some(add)),
                };
                let mut t = RichText::new(format!("{p} {line}")).monospace();
                if let Some(c) = c {
                    t = t.color(c).background_color(c.gamma_multiply(0.15));
                }
                ui.add(egui::Label::new(t).extend());
            }
        });
    }

    /// Bestätigung vor dem direkten Ausführen.
    fn ui_confirm(&mut self, ui: &mut egui::Ui) {
        let Some(keys) = &self.confirm else { return };
        let drops = self.entries.iter().filter(|e| e.status == Status::OnlyTgt && keys.contains(&e.key())).count();
        let (mut run, mut close) = (false, false);
        egui::Modal::new(egui::Id::unique(("sync_confirm", self.id))).show(ui.ctx(), |ui| {
            ui.heading("SQL ausführen");
            ui.label(format!("Das SQL für {} Objekt(e) wird direkt auf {} ausgeführt.", keys.len(), self.side_label(&self.tgt)));
            if drops > 0 {
                ui.colored_label(ui.visuals().error_fg_color, format!("Achtung: {drops} Objekt(e) werden gelöscht (DROP)!"));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                run = ui.button("Ausführen").clicked();
                close = ui.button("Abbrechen").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape));
            });
        });
        if run {
            if let Some(keys) = self.confirm.take() {
                self.execute(keys);
            }
        } else if close {
            self.confirm = None;
        }
    }

    fn set_all(&mut self, v: bool) {
        for e in &mut self.entries {
            if e.status != Status::Equal && e.status != Status::Error {
                e.checked = v;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_lines() {
        let v = |x: &[&str]| x.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let d = line_diff(&v(&["a", "b", "c"]), &v(&["a", "x", "c", "d"]));
        let k: Vec<(Dl, &str)> = d.iter().map(|(k, s)| (*k, s.as_str())).collect();
        assert_eq!(k, vec![(Dl::Same, "a"), (Dl::Del, "b"), (Dl::Add, "x"), (Dl::Same, "c"), (Dl::Add, "d")]);
    }

    #[test]
    fn routine_diff_sql() {
        let a = "CREATE PROCEDURE `p`()\nBEGIN SELECT 1; END".to_string();
        let b = "CREATE PROCEDURE `p`()  \nBEGIN SELECT 1; END".to_string();
        let c = "CREATE PROCEDURE `p`()\nBEGIN SELECT 2; END".to_string();
        assert!(routine_sql(Kind::Procedure, "p", Some(&a), Some(&b)).is_empty());
        assert_eq!(routine_sql(Kind::Procedure, "p", Some(&a), None), vec![a.clone()]);
        assert_eq!(routine_sql(Kind::Event, "e", None, Some(&a)), vec!["DROP EVENT `e`".to_string()]);
        assert_eq!(routine_sql(Kind::Function, "p", Some(&c), Some(&a)), vec!["DROP FUNCTION IF EXISTS `p`".to_string(), c.clone()]);
    }

    #[test]
    fn view_sql() {
        let src = unqualify("CREATE ALGORITHM=UNDEFINED SQL SECURITY DEFINER VIEW `v` AS select `a`.`t`.`id` AS `id` from `a`.`t` join `x`.`u`", "a");
        assert_eq!(src, "CREATE ALGORITHM=UNDEFINED SQL SECURITY DEFINER VIEW `v` AS select `t`.`id` AS `id` from `t` join `x`.`u`");
        let tgt = "CREATE VIEW `v` AS select 1".to_string();
        assert_eq!(
            routine_sql(Kind::View, "v", Some(&src), Some(&tgt)),
            vec!["CREATE OR REPLACE ALGORITHM=UNDEFINED SQL SECURITY DEFINER VIEW `v` AS select `t`.`id` AS `id` from `t` join `x`.`u`".to_string()]
        );
    }
}
