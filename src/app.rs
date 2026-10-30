//! Hauptfenster im Navicat-8-Layout: Werkzeugleiste, Verbindungsbaum links,
//! Objektliste / Tabs rechts, Statusleiste unten.

use crate::icons::{self, Icon};
use crate::config::{OpenTab, Settings};
use crate::datatab::DataTab;
use crate::designtab::DesignTab;
use crate::import::ImportWizard;
use crate::dump::DumpDialog;
use crate::export::{self, ExportDialog, ExportJob};
use crate::navicat::{self, NavConn};
use crate::queries;
use crate::db::{qi, ConnConfig, JobResult, Mode, Stmt, Worker};
use crate::querytab::QueryTab;
use crate::monitor::MonitorTab;
use crate::sync::{ConnRef, SyncTab};
use crate::users::UsersTab;
use eframe::egui::{self, Color32, Key, RichText, Sense};
use egui_extras::{Column, TableBuilder};
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug, Hash)]
enum Cat {
    #[default]
    Tables,
    Views,
    Routines,
    Events,
    /// Gespeicherte Abfragen (Dateien, nicht auf dem Server)
    Queries,
}

impl Cat {
    const ALL: [Cat; 5] = [Cat::Tables, Cat::Views, Cat::Routines, Cat::Events, Cat::Queries];
    fn label(self) -> &'static str {
        match self {
            Cat::Tables => "Tabellen",
            Cat::Views => "Ansichten",
            Cat::Routines => "Funktionen",
            Cat::Events => "Ereignisse",
            Cat::Queries => "Abfragen",
        }
    }
    fn icon(self) -> Icon {
        match self {
            Cat::Tables => Icon::Table,
            Cat::Views => Icon::View,
            Cat::Routines => Icon::Function,
            Cat::Events => Icon::Event,
            Cat::Queries => Icon::Query,
        }
    }
    fn idx(self) -> usize {
        self as usize
    }
}

/// Große Werkzeugleisten-Schaltfläche wie in Navicat 8: Icon oben, Beschriftung unten.
fn tool_button(ui: &mut egui::Ui, icon: Icon, label: &str, active: bool, enabled: bool) -> bool {
    let size = egui::vec2(76.0, 55.0);
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    let v = ui.visuals();
    let painter = ui.painter();
    if active {
        painter.rect_filled(rect, 0.0, v.selection.bg_fill.gamma_multiply(0.45));
    } else if enabled && resp.hovered() {
        painter.rect_filled(rect, 0.0, v.widgets.hovered.weak_bg_fill);
    }
    let tc = if enabled { v.text_color() } else { v.weak_text_color().gamma_multiply(0.7) };
    let ir = egui::Rect::from_center_size(rect.center_top() + egui::vec2(0.0, 19.0), egui::vec2(30.0, 30.0));
    icons::paint(painter, ir, icon, enabled);
    painter.text(rect.center_bottom() - egui::vec2(0.0, 10.0), egui::Align2::CENTER_CENTER, label, egui::FontId::proportional(12.5), tc);
    enabled && resp.clicked()
}

#[derive(Clone, Default)]
struct Obj {
    name: String,
    engine: String,
    rows: String,
    data_len: u64,
    index_len: u64,
    auto_inc: String,
    updated: String,
    collation: String,
    comment: String,
    kind: String, // bei Routinen: FUNCTION/PROCEDURE, bei Events: Status
    extra: String,
}

struct DbNode {
    name: String,
    open: bool,
    loaded: bool,
    loading: bool,
    objs: [Vec<Obj>; 5],
    cat_open: [bool; 5],
    /// Aufgeklappte TelliMATE-Ordner (Index in `TM_FOLDERS`).
    folder_open: [bool; TM_FOLDERS.len()],
}

impl DbNode {
    fn new(name: String) -> Self {
        Self {
            name,
            open: false,
            loaded: false,
            loading: false,
            objs: Default::default(),
            cat_open: [false; 5],
            folder_open: [false; TM_FOLDERS.len()],
        }
    }
}

/// TelliMATE-Modus: Tabellen mit diesem Präfix erscheinen im Baum in einem Unterordner (Präfix, Ordnername).
const TM_FOLDERS: [(&str, &str); 3] = [("vorgaenge_", "vorgaenge"), ("helper_", "helper"), ("tarife_", "tarife")];

/// Ordner-Index für einen Tabellennamen im TelliMATE-Modus.
fn tm_folder(name: &str) -> Option<usize> {
    TM_FOLDERS.iter().position(|(p, _)| name.starts_with(p) && name.len() > p.len())
}

#[derive(PartialEq, Clone, Copy)]
enum CState {
    Closed,
    Connecting,
    Open,
    Failed,
}

enum Pending {
    Databases,
    Objects(String),
    Exec { refresh_db: Option<String>, refresh_dbs: bool, what: String },
    Ddl { db: Option<String>, name: String, delim: bool },
    /// Zeichensatz/Kollation und Tabellen einer Datenbank für den Bearbeiten-Dialog.
    DbOpts(String),
}

struct ConnNode {
    uid: u64,
    cfg: ConnConfig,
    worker: Option<Worker>,
    state: CState,
    version: String,
    dbs: Vec<DbNode>,
    open: bool,
    pending: HashMap<u64, Pending>,
    error: Option<String>,
}

impl ConnNode {
    fn new(uid: u64, cfg: ConnConfig) -> Self {
        Self {
            uid,
            cfg,
            worker: None,
            state: CState::Closed,
            version: String::new(),
            dbs: Vec::new(),
            open: false,
            pending: HashMap::new(),
            error: None,
        }
    }
    fn db_mut(&mut self, name: &str) -> Option<&mut DbNode> {
        self.dbs.iter_mut().find(|d| d.name == name)
    }
    fn db(&self, name: &str) -> Option<&DbNode> {
        self.dbs.iter().find(|d| d.name == name)
    }
    fn send(&mut self, p: Pending, db: Option<String>, stmts: Vec<Stmt>, mode: Mode) {
        if let Some(w) = self.worker.as_mut() {
            let tag = w.send(db, stmts, mode, usize::MAX);
            self.pending.insert(tag, p);
        }
    }
}

#[derive(Clone, Default, PartialEq)]
struct Sel {
    conn: Option<usize>,
    db: Option<String>,
    cat: Cat,
    obj: Option<String>,
}

enum TabKind {
    Data(Box<DataTab>),
    Query(Box<QueryTab>),
    Users(Box<UsersTab>),
    Monitor(Box<MonitorTab>),
    Sync(Box<SyncTab>),
    Design(Box<DesignTab>),
}

struct Tab {
    conn_uid: u64,
    conn_name: String,
    kind: TabKind,
}

struct Confirm {
    title: String,
    text: String,
    conn: usize,
    db: Option<String>,
    stmts: Vec<String>,
    refresh_db: Option<String>,
    refresh_dbs: bool,
}

struct ConnDialog {
    idx: Option<usize>,
    cfg: ConnConfig,
    test: Option<Worker>,
    test_tag: u64,
    test_result: Option<Result<String, String>>,
}

struct NewDbDialog {
    conn: usize,
    name: String,
    charset: String,
    collation: String,
    /// `true` = bestehende Datenbank `name` bearbeiten (`ALTER DATABASE`).
    edit: bool,
    /// Aktuelle Einstellungen werden noch geladen.
    loading: bool,
    /// Basistabellen der Datenbank (für „Tabellen konvertieren“).
    tables: Vec<String>,
    /// Bestehende Tabellen per `CONVERT TO` mit umstellen.
    convert: bool,
}

enum Act {
    /// Auswahl setzen und rechts die Objektansicht zeigen.
    Select(Sel),
    /// Wie `Select`, aber rechts als einfache Liste (Doppelklick auf eine Kategorie im Baum).
    SelectList(Sel),
    /// Nur markieren (Einfachklick im Baum) – rechte Ansicht bleibt.
    Mark(Sel),
    ToggleConn(usize),
    ToggleDb(usize, String),
    ToggleCat(usize, String, Cat),
    ToggleFolder(usize, String, usize),
    OpenObj(usize, String, Cat, String),
    NewQuery(usize, Option<String>, String),
    EditConn(usize),
    DeleteConn(usize),
    CloseConn(usize),
    RefreshConn(usize),
    RefreshDb(usize, String),
    /// Datenbank im Baum zuklappen und geladene Objekte verwerfen.
    CloseDb(usize, String),
    NewDb(usize),
    /// Datenbankeinstellungen (Zeichensatz/Kollation) bearbeiten.
    EditDb(usize, String),
    Ddl(usize, String, Cat, String, String),
    Users(usize),
    /// Servermonitor (Prozesse, Status, Variablen) öffnen.
    Monitor(usize),
    /// Strukturabgleich öffnen (optional mit vorbelegter Quelle).
    Sync(Option<(usize, String)>),
    Confirm(Confirm),
    DbListSel(String),
    /// Tabellenentwurf öffnen (`None` = neue Tabelle).
    Design(usize, String, Option<String>),
    /// Gespeicherte Abfrage löschen (Rückfrage).
    DeleteQuery(usize, String, String),
    /// Tabelle/Ansicht als CSV exportieren.
    Export(usize, String, String),
    /// Import-Assistent (DB, optional vorbelegte Zieltabelle).
    Import(usize, String, Option<String>),
    /// SQL-Dump einer Datenbank oder (mit Tabelle) einer einzelnen Tabelle/Ansicht.
    Dump(usize, String, Option<String>),
}

/// Rückfrage/Auswahl für den Navicat-Import.
struct NavicatDlg {
    found: Vec<NavConn>,
    sel: Vec<bool>,
    report: Option<String>,
}

/// Gespeicherte Abfragen einer Datenbank als Objektliste.
fn saved_objs(conn: &str, db: &str) -> Vec<Obj> {
    queries::list(conn, db)
        .into_iter()
        .map(|q| Obj { name: q.name, updated: q.modified, data_len: q.size, ..Default::default() })
        .collect()
}

pub struct App {
    settings: Settings,
    conns: Vec<ConnNode>,
    next_uid: u64,
    sel: Sel,
    tabs: Vec<Tab>,
    active: usize, // 0 = Objekte
    next_query_id: u64,
    conn_dialog: Option<ConnDialog>,
    confirm: Option<Confirm>,
    newdb: Option<NewDbDialog>,
    settings_open: bool,
    delete_conn: Option<usize>,
    obj_filter: String,
    /// Markierte Datenbank in der Datenbankliste (Verbindungsansicht).
    db_list_sel: Option<String>,
    status: String,
    error: Option<String>,
    acts: Vec<Act>,
    delete_query: Option<(usize, String, String)>,
    navicat: Option<NavicatDlg>,
    /// Export/Import von Verbindungen und Abfragen.
    backup: Option<crate::backup::BackupDlg>,
    /// Export-Dialog aus dem Kontextmenü: (Verbindung, DB, Tabelle, Dialog)
    export_dlg: Option<(usize, String, String, ExportDialog)>,
    /// Laufende Exporte (Tabellenname, Job)
    export_jobs: Vec<(String, ExportJob)>,
    dump_dlg: Option<(usize, DumpDialog)>,
    /// Objektansicht als einfache Liste (nach Doppelklick auf eine Kategorie im Baum) statt Detailliste.
    obj_list: bool,
    /// Import-Assistent: (Verbindung, Assistent)
    import: Option<(usize, ImportWizard)>,
    /// Vorhandene Systemschriften für die Auswahl in den Optionen.
    fonts: Vec<&'static str>,
    /// Pixeldichte, für die die Schriftgrößen zuletzt gerundet wurden.
    last_ppp: f32,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings = Settings::load();
        crate::install_fonts(&cc.egui_ctx, &settings.font);
        crate::grid::set_row_h(settings.grid_row_h);
        crate::apply_style(&cc.egui_ctx, &settings);
        let mut next_uid = 1;
        let conns = settings
            .connections
            .iter()
            .map(|c| {
                next_uid += 1;
                ConnNode::new(next_uid, c.clone())
            })
            .collect();
        let mut app = Self {
            settings,
            conns,
            next_uid,
            sel: Sel::default(),
            tabs: Vec::new(),
            active: 0,
            next_query_id: 1,
            conn_dialog: None,
            confirm: None,
            newdb: None,
            settings_open: false,
            delete_conn: None,
            obj_filter: String::new(),
            db_list_sel: None,
            status: String::new(),
            error: None,
            acts: Vec::new(),
            delete_query: None,
            navicat: None,
            backup: None,
            export_dlg: None,
            export_jobs: Vec::new(),
            dump_dlg: None,
            obj_list: false,
            fonts: crate::available_fonts(),
            last_ppp: 0.0,
            import: None,
        };
        crate::export::set_bom(app.settings.csv_bom);
        // Beim ersten Start Navicat-Import anbieten
        if !app.settings.navicat_asked {
            let found = navicat::find();
            if !found.is_empty() {
                app.navicat = Some(NavicatDlg::new(found));
            }
        }
        if app.conns.is_empty() && app.navicat.is_none() {
            app.conn_dialog = Some(ConnDialog::new(None, ConnConfig { name: "Lokal".into(), ..Default::default() }));
        }
        app.restore_tabs(&cc.egui_ctx);
        app
    }

    /// Offene Tabs für `settings.json` (neue, noch nicht gespeicherte Tabellenentwürfe entfallen).
    fn view_state(&self) -> (Vec<OpenTab>, usize) {
        let mut out = Vec::new();
        let mut active = 0;
        for (i, t) in self.tabs.iter().enumerate() {
            let conn = t.conn_name.clone();
            let o = match &t.kind {
                TabKind::Data(d) => OpenTab::Data { conn, db: d.db.clone(), table: d.table.clone(), view: d.is_view },
                TabKind::Query(q) => OpenTab::Query {
                    conn,
                    db: q.db.clone(),
                    title: q.title.clone(),
                    sql: q.sql.clone(),
                    saved: q.saved.clone(),
                },
                TabKind::Design(d) => match d.table() {
                    Some(table) => OpenTab::Design { conn, db: d.db.clone(), table: table.to_string() },
                    None => continue,
                },
                TabKind::Users(_) => OpenTab::Users { conn },
                TabKind::Monitor(_) => OpenTab::Monitor { conn },
                TabKind::Sync(s) => match s.source() {
                    Some((conn, db)) => OpenTab::Sync { conn, db: Some(db) },
                    None => OpenTab::Sync { conn, db: None },
                },
            };
            out.push(o);
            if self.active == i + 1 {
                active = out.len();
            }
        }
        (out, active)
    }

    /// Beim letzten Beenden offene Tabs wieder öffnen. Verbindungen ohne gespeichertes
    /// Passwort werden übersprungen.
    fn restore_tabs(&mut self, ctx: &egui::Context) {
        let saved = std::mem::take(&mut self.settings.open_tabs);
        let mut map = Vec::new(); // gespeicherter Index → Tab-Nummer
        for o in &saved {
            let conn = match o {
                OpenTab::Data { conn, .. }
                | OpenTab::Query { conn, .. }
                | OpenTab::Design { conn, .. }
                | OpenTab::Users { conn }
                | OpenTab::Monitor { conn }
                | OpenTab::Sync { conn, .. } => conn,
            };
            let Some(ci) = self.conns.iter().position(|c| &c.cfg.name == conn && c.cfg.save_password) else {
                map.push(0);
                continue;
            };
            if !matches!(o, OpenTab::Sync { .. }) && self.conns[ci].state != CState::Open {
                self.open_conn(ci, ctx);
                self.conns[ci].open = true;
            }
            let before = self.tabs.len();
            match o {
                OpenTab::Data { db, table, view, .. } => self.open_table(ci, db, table, *view, ctx),
                OpenTab::Query { db, title, sql, saved, .. } => {
                    self.new_query(ci, db.clone(), sql.clone(), ctx);
                    if let Some(Tab { kind: TabKind::Query(q), .. }) = self.tabs.last_mut() {
                        q.title = title.clone();
                        if let Some((sdb, name)) = saved {
                            q.saved_sql = queries::load(conn, sdb, name).unwrap_or_else(|_| sql.clone());
                            q.saved = saved.clone();
                        }
                    }
                }
                OpenTab::Design { db, table, .. } => self.open_design(ci, db, Some(table), ctx),
                OpenTab::Users { .. } => self.open_users(ci, ctx),
                OpenTab::Monitor { .. } => self.open_monitor(ci, ctx),
                OpenTab::Sync { db, .. } => self.open_sync(db.clone().map(|d| (ci, d)), ctx),
            }
            map.push(if self.tabs.len() > before { self.tabs.len() } else { 0 });
        }
        let a = self.settings.active_tab;
        self.active = if a == 0 { 0 } else { map.get(a - 1).copied().unwrap_or(0) };
    }

    /// Importierte Verbindungen übernehmen und Abfragelisten geladener Datenbanken aktualisieren.
    fn add_imported(&mut self, new: Vec<ConnConfig>) {
        for cfg in new {
            self.next_uid += 1;
            self.conns.push(ConnNode::new(self.next_uid, cfg));
        }
        for ci in 0..self.conns.len() {
            let dbs: Vec<String> = self.conns[ci].dbs.iter().filter(|d| d.loaded).map(|d| d.name.clone()).collect();
            for db in dbs {
                self.refresh_saved(ci, &db);
            }
        }
    }

    fn save_settings(&mut self) {
        self.settings.connections = self.conns.iter().map(|c| c.cfg.clone()).collect();
        if let Err(e) = self.settings.save() {
            self.error = Some(format!("Einstellungen konnten nicht gespeichert werden: {e}"));
        }
    }

    // ------------------------------------------------------------------ Verbindungen

    fn open_conn(&mut self, i: usize, ctx: &egui::Context) {
        let c = &mut self.conns[i];
        if c.state == CState::Connecting {
            return;
        }
        if c.worker.is_none() {
            c.worker = Some(Worker::spawn(c.cfg.clone(), ctx.clone()));
        }
        c.state = CState::Connecting;
        c.error = None;
        c.send(
            Pending::Databases,
            None,
            vec![Stmt::plain("SELECT VERSION()"), Stmt::plain("SHOW DATABASES")],
            Mode::StopOnError,
        );
    }

    fn close_conn(&mut self, i: usize) {
        let c = &mut self.conns[i];
        c.worker = None;
        c.state = CState::Closed;
        c.dbs.clear();
        c.pending.clear();
        c.open = false;
        if self.sel.conn == Some(i) {
            self.sel.db = None;
            self.sel.obj = None;
        }
    }

    fn load_objects(&mut self, i: usize, db: &str) {
        let c = &mut self.conns[i];
        if let Some(d) = c.db_mut(db) {
            if d.loading {
                return;
            }
            d.loading = true;
        }
        let p = || vec![Some(db.to_string())];
        c.send(
            Pending::Objects(db.to_string()),
            None,
            vec![
                Stmt::with(
                    "SELECT TABLE_NAME, TABLE_TYPE, ENGINE, TABLE_ROWS, DATA_LENGTH, INDEX_LENGTH, AUTO_INCREMENT, \
                     UPDATE_TIME, CREATE_TIME, TABLE_COLLATION, TABLE_COMMENT \
                     FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME",
                    p(),
                ),
                Stmt::with(
                    "SELECT ROUTINE_NAME, ROUTINE_TYPE, DTD_IDENTIFIER, LAST_ALTERED, ROUTINE_COMMENT \
                     FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = ? ORDER BY ROUTINE_NAME",
                    p(),
                ),
                Stmt::with(
                    "SELECT EVENT_NAME, STATUS, EVENT_TYPE, EXECUTE_AT, INTERVAL_VALUE, INTERVAL_FIELD, LAST_EXECUTED, EVENT_COMMENT \
                     FROM information_schema.EVENTS WHERE EVENT_SCHEMA = ? ORDER BY EVENT_NAME",
                    p(),
                ),
            ],
            Mode::Continue,
        );
    }

    fn poll_conns(&mut self, ctx: &egui::Context) {
        let mut followups: Vec<(usize, String)> = Vec::new();
        let mut open_ddl: Vec<(usize, Option<String>, String, String)> = Vec::new();
        for (ci, c) in self.conns.iter_mut().enumerate() {
            let Some(w) = c.worker.as_mut() else { continue };
            let mut results: Vec<JobResult> = Vec::new();
            while let Some(r) = w.poll() {
                results.push(r);
            }
            for mut res in results {
                let Some(p) = c.pending.remove(&res.tag) else { continue };
                match p {
                    Pending::Databases => {
                        if let Some(e) = res.error.clone() {
                            c.state = CState::Failed;
                            c.error = Some(e.clone());
                            c.worker = None;
                            self.error = Some(format!("{}: {e}", c.cfg.name));
                            break;
                        }
                        c.state = CState::Open;
                        c.open = true;
                        c.version = res.set(0).and_then(|s| s.rows.first()).and_then(|r| r[0].clone()).unwrap_or_default();
                        let names: Vec<String> =
                            res.take_set(1).map(|s| s.rows.into_iter().filter_map(|r| r[0].clone()).collect()).unwrap_or_default();
                        let mut old: HashMap<String, DbNode> = c.dbs.drain(..).map(|d| (d.name.clone(), d)).collect();
                        c.dbs = names
                            .into_iter()
                            .map(|n| {
                                old.remove(&n).map(|mut d| {
                                    d.loading = false;
                                    d
                                }).unwrap_or_else(|| DbNode::new(n))
                            })
                            .collect();
                        for d in &c.dbs {
                            if d.open {
                                followups.push((ci, d.name.clone()));
                            }
                        }
                        self.status = format!("Verbunden mit {} ({})", c.cfg.name, c.version);
                    }
                    Pending::Objects(db) => {
                        let saved = saved_objs(&c.cfg.name, &db);
                        let Some(d) = c.db_mut(&db) else { continue };
                        d.loading = false;
                        d.loaded = true;
                        let mut tables = Vec::new();
                        let mut views = Vec::new();
                        if let Some(rs) = res.set(0) {
                            for r in 0..rs.rows.len() {
                                let o = Obj {
                                    name: rs.get(r, "TABLE_NAME").into(),
                                    engine: rs.get(r, "ENGINE").into(),
                                    rows: rs.get(r, "TABLE_ROWS").into(),
                                    data_len: rs.get(r, "DATA_LENGTH").parse().unwrap_or(0),
                                    index_len: rs.get(r, "INDEX_LENGTH").parse().unwrap_or(0),
                                    auto_inc: rs.get(r, "AUTO_INCREMENT").into(),
                                    updated: {
                                        let u = rs.get(r, "UPDATE_TIME");
                                        if u.is_empty() { rs.get(r, "CREATE_TIME").into() } else { u.into() }
                                    },
                                    collation: rs.get(r, "TABLE_COLLATION").into(),
                                    comment: rs.get(r, "TABLE_COMMENT").into(),
                                    kind: rs.get(r, "TABLE_TYPE").into(),
                                    extra: String::new(),
                                };
                                if o.kind == "VIEW" {
                                    views.push(o);
                                } else {
                                    tables.push(o);
                                }
                            }
                        }
                        let mut routines = Vec::new();
                        if let Some(rs) = res.set(1) {
                            for r in 0..rs.rows.len() {
                                routines.push(Obj {
                                    name: rs.get(r, "ROUTINE_NAME").into(),
                                    kind: rs.get(r, "ROUTINE_TYPE").into(),
                                    extra: rs.get(r, "DTD_IDENTIFIER").into(),
                                    updated: rs.get(r, "LAST_ALTERED").into(),
                                    comment: rs.get(r, "ROUTINE_COMMENT").into(),
                                    ..Default::default()
                                });
                            }
                        }
                        let mut events = Vec::new();
                        if let Some(rs) = res.set(2) {
                            for r in 0..rs.rows.len() {
                                let sched = if rs.get(r, "EVENT_TYPE") == "ONE TIME" {
                                    format!("AT {}", rs.get(r, "EXECUTE_AT"))
                                } else {
                                    format!("EVERY {} {}", rs.get(r, "INTERVAL_VALUE"), rs.get(r, "INTERVAL_FIELD"))
                                };
                                events.push(Obj {
                                    name: rs.get(r, "EVENT_NAME").into(),
                                    kind: rs.get(r, "STATUS").into(),
                                    extra: sched,
                                    updated: rs.get(r, "LAST_EXECUTED").into(),
                                    comment: rs.get(r, "EVENT_COMMENT").into(),
                                    ..Default::default()
                                });
                            }
                        }
                        d.objs = [tables, views, routines, events, saved];
                        if let Some(e) = res.outs.first().and_then(|o| o.error.clone()) {
                            self.error = Some(e);
                        }
                    }
                    Pending::Exec { refresh_db, refresh_dbs, what } => {
                        let err = res.error.or_else(|| res.outs.iter().find_map(|o| o.error.clone()));
                        if let Some(e) = err {
                            self.error = Some(format!("{what}: {e}"));
                        } else {
                            self.status = format!("{what} – erledigt ({})", crate::datatab::fmt_dur(res.elapsed));
                        }
                        if refresh_dbs {
                            c.send(
                                Pending::Databases,
                                None,
                                vec![Stmt::plain("SELECT VERSION()"), Stmt::plain("SHOW DATABASES")],
                                Mode::StopOnError,
                            );
                        }
                        if let Some(db) = refresh_db {
                            followups.push((ci, db));
                        }
                    }
                    Pending::Ddl { db, name, delim } => {
                        if let Some(e) = res.error {
                            self.error = Some(e);
                            continue;
                        }
                        let Some(rs) = res.set(0) else { continue };
                        let col = rs.columns.iter().position(|c| c.name.starts_with("Create "));
                        let ddl = col.and_then(|ci| rs.rows.first().and_then(|r| r[ci].clone())).unwrap_or_default();
                        let text = if delim { format!("DELIMITER $$\n{ddl}$$\nDELIMITER ;\n") } else { format!("{ddl};\n") };
                        open_ddl.push((ci, db, name, text));
                    }
                    Pending::DbOpts(db) => {
                        let Some(nd) = self.newdb.as_mut().filter(|nd| nd.edit && nd.conn == ci && nd.name == db) else {
                            continue;
                        };
                        if let Some(e) = res.error {
                            self.error = Some(e);
                            self.newdb = None;
                            continue;
                        }
                        nd.loading = false;
                        if let Some(rs) = res.set(0) {
                            nd.charset = rs.get(0, "DEFAULT_CHARACTER_SET_NAME").into();
                            nd.collation = rs.get(0, "DEFAULT_COLLATION_NAME").into();
                        }
                        if let Some(rs) = res.set(1) {
                            nd.tables = rs.rows.iter().filter_map(|r| r[0].clone()).collect();
                        }
                    }
                }
            }
        }
        for (ci, db) in followups {
            if let Some(d) = self.conns[ci].db_mut(&db) {
                d.loading = false;
            }
            self.load_objects(ci, &db);
        }
        for (ci, db, name, text) in open_ddl {
            self.new_query(ci, db, text, ctx);
            if let Some(Tab { kind: TabKind::Query(q), .. }) = self.tabs.last_mut() {
                q.title = format!("DDL {name}");
            }
        }
    }

    // ------------------------------------------------------------------ Tabs

    fn new_query(&mut self, ci: usize, db: Option<String>, sql: String, ctx: &egui::Context) {
        let c = &self.conns[ci];
        let id = self.next_query_id;
        self.next_query_id += 1;
        let q = QueryTab::new(id, &c.cfg, ctx, db, sql, self.settings.max_query_rows);
        self.tabs.push(Tab { conn_uid: c.uid, conn_name: c.cfg.name.clone(), kind: TabKind::Query(Box::new(q)) });
        self.active = self.tabs.len();
    }

    fn open_table(&mut self, ci: usize, db: &str, table: &str, is_view: bool, ctx: &egui::Context) {
        let uid = self.conns[ci].uid;
        if let Some(pos) = self.tabs.iter().position(|t| {
            t.conn_uid == uid && matches!(&t.kind, TabKind::Data(d) if d.db == db && d.table == table)
        }) {
            self.active = pos + 1;
            return;
        }
        let c = &self.conns[ci];
        let d = DataTab::new(&c.cfg, ctx, db, table, is_view, self.settings.page_size);
        self.tabs.push(Tab { conn_uid: uid, conn_name: c.cfg.name.clone(), kind: TabKind::Data(Box::new(d)) });
        self.active = self.tabs.len();
    }

    fn open_users(&mut self, ci: usize, ctx: &egui::Context) {
        let uid = self.conns[ci].uid;
        if let Some(pos) = self.tabs.iter().position(|t| t.conn_uid == uid && matches!(t.kind, TabKind::Users(_))) {
            self.active = pos + 1;
            return;
        }
        let c = &self.conns[ci];
        let u = UsersTab::new(&c.cfg, ctx);
        self.tabs.push(Tab { conn_uid: uid, conn_name: c.cfg.name.clone(), kind: TabKind::Users(Box::new(u)) });
        self.active = self.tabs.len();
    }

    fn open_monitor(&mut self, ci: usize, ctx: &egui::Context) {
        let uid = self.conns[ci].uid;
        if let Some(pos) = self.tabs.iter().position(|t| t.conn_uid == uid && matches!(t.kind, TabKind::Monitor(_))) {
            self.active = pos + 1;
            return;
        }
        let c = &self.conns[ci];
        let m = MonitorTab::new(&c.cfg, ctx);
        self.tabs.push(Tab { conn_uid: uid, conn_name: c.cfg.name.clone(), kind: TabKind::Monitor(Box::new(m)) });
        self.active = self.tabs.len();
    }

    fn open_sync(&mut self, pre: Option<(usize, String)>, ctx: &egui::Context) {
        let conns: Vec<ConnRef> =
            self.conns.iter().map(|c| ConnRef { uid: c.uid, name: c.cfg.name.clone(), cfg: c.cfg.clone() }).collect();
        let Some(first) = self.conns.first() else { return };
        let (uid, name) = match &pre {
            Some((ci, _)) => (self.conns[*ci].uid, self.conns[*ci].cfg.name.clone()),
            None => (first.uid, first.cfg.name.clone()),
        };
        let t = SyncTab::new(conns, ctx, pre);
        self.tabs.push(Tab { conn_uid: uid, conn_name: name, kind: TabKind::Sync(Box::new(t)) });
        self.active = self.tabs.len();
    }

    fn open_saved_query(&mut self, ci: usize, db: &str, name: &str, ctx: &egui::Context) {
        let uid = self.conns[ci].uid;
        let key = Some((db.to_string(), name.to_string()));
        if let Some(pos) = self.tabs.iter().position(|t| t.conn_uid == uid && matches!(&t.kind, TabKind::Query(q) if q.saved == key)) {
            self.active = pos + 1;
            return;
        }
        match queries::load(&self.conns[ci].cfg.name, db, name) {
            Ok(text) => {
                if self.conns[ci].state != CState::Open {
                    self.open_conn(ci, ctx);
                }
                self.new_query(ci, Some(db.to_string()), text.clone(), ctx);
                if let Some(Tab { kind: TabKind::Query(q), .. }) = self.tabs.last_mut() {
                    q.title = name.to_string();
                    q.saved = key;
                    q.saved_sql = text;
                }
            }
            Err(e) => self.error = Some(format!("Abfrage konnte nicht geladen werden: {e}")),
        }
    }

    /// Liste gespeicherter Abfragen einer Datenbank neu einlesen.
    fn refresh_saved(&mut self, ci: usize, db: &str) {
        let list = saved_objs(&self.conns[ci].cfg.name, db);
        if let Some(d) = self.conns[ci].db_mut(db) {
            d.objs[Cat::Queries.idx()] = list;
        }
    }

    fn open_design(&mut self, ci: usize, db: &str, table: Option<&str>, ctx: &egui::Context) {
        let uid = self.conns[ci].uid;
        if table.is_some() {
            if let Some(pos) = self.tabs.iter().position(|t| {
                t.conn_uid == uid && matches!(&t.kind, TabKind::Design(d) if d.db == db && d.table() == table)
            }) {
                self.active = pos + 1;
                return;
            }
        }
        let c = &self.conns[ci];
        let d = DesignTab::new(&c.cfg, ctx, db, table);
        self.tabs.push(Tab { conn_uid: uid, conn_name: c.cfg.name.clone(), kind: TabKind::Design(Box::new(d)) });
        self.active = self.tabs.len();
    }

    fn close_tab(&mut self, idx: usize) {
        if idx == 0 || idx > self.tabs.len() {
            return;
        }
        self.tabs.remove(idx - 1);
        // Beim Schließen des aktiven Tabs zum linken Nachbarn wechseln
        if self.active >= idx {
            self.active -= 1;
        }
    }

    // ------------------------------------------------------------------ Aktionen

    fn apply_acts(&mut self, ctx: &egui::Context) {
        let acts = std::mem::take(&mut self.acts);
        for a in acts {
            match a {
                Act::Select(s) => {
                    self.obj_list = false;
                    self.select(s);
                }
                Act::SelectList(s) => {
                    self.obj_list = true;
                    self.select(s);
                }
                Act::Mark(s) => {
                    if let (Some(ci), Some(db)) = (s.conn, s.db.as_ref()) {
                        let need = self.conns[ci].db(db).is_some_and(|d| !d.loaded && !d.loading);
                        if need {
                            self.load_objects(ci, db);
                        }
                    }
                    self.sel = s;
                }
                Act::ToggleConn(i) => {
                    let c = &mut self.conns[i];
                    match c.state {
                        CState::Open => c.open = !c.open,
                        CState::Connecting => {}
                        _ => self.open_conn(i, ctx),
                    }
                }
                Act::ToggleDb(i, db) => {
                    let mut load = false;
                    if let Some(d) = self.conns[i].db_mut(&db) {
                        d.open = !d.open;
                        load = d.open && !d.loaded;
                    }
                    if load {
                        self.load_objects(i, &db);
                    }
                }
                Act::CloseDb(i, db) => {
                    if let Some(d) = self.conns[i].db_mut(&db) {
                        *d = DbNode::new(db.clone());
                    }
                    if self.sel.conn == Some(i) && self.sel.db.as_deref() == Some(db.as_str()) {
                        self.sel.db = None;
                        self.sel.obj = None;
                    }
                }
                Act::ToggleCat(i, db, cat) => {
                    if let Some(d) = self.conns[i].db_mut(&db) {
                        d.cat_open[cat.idx()] = !d.cat_open[cat.idx()];
                    }
                }
                Act::ToggleFolder(i, db, f) => {
                    if let Some(d) = self.conns[i].db_mut(&db) {
                        d.folder_open[f] = !d.folder_open[f];
                    }
                }
                Act::OpenObj(i, db, cat, name) => match cat {
                    Cat::Queries => self.open_saved_query(i, &db, &name, ctx),
                    Cat::Tables | Cat::Views => self.open_table(i, &db, &name, cat == Cat::Views, ctx),
                    Cat::Routines | Cat::Events => {
                        let kind = self.conns[i]
                            .db(&db)
                            .and_then(|d| d.objs[cat.idx()].iter().find(|o| o.name == name))
                            .map(|o| o.kind.clone())
                            .unwrap_or_default();
                        self.acts_ddl(i, db, cat, name, kind);
                    }
                },
                Act::NewQuery(i, db, sql) => {
                    if self.conns[i].state != CState::Open {
                        self.open_conn(i, ctx);
                    }
                    self.new_query(i, db, sql, ctx);
                }
                Act::EditConn(i) => self.conn_dialog = Some(ConnDialog::new(Some(i), self.conns[i].cfg.clone())),
                Act::DeleteConn(i) => self.delete_conn = Some(i),
                Act::CloseConn(i) => self.close_conn(i),
                Act::RefreshConn(i) => self.open_conn(i, ctx),
                Act::RefreshDb(i, db) => {
                    if let Some(d) = self.conns[i].db_mut(&db) {
                        d.loading = false;
                    }
                    self.load_objects(i, &db);
                }
                Act::NewDb(i) => {
                    self.newdb = Some(NewDbDialog {
                        conn: i,
                        name: String::new(),
                        charset: "utf8mb4".into(),
                        collation: "utf8mb4_unicode_ci".into(),
                        edit: false,
                        loading: false,
                        tables: Vec::new(),
                        convert: false,
                    })
                }
                Act::EditDb(i, db) => {
                    let p = || vec![Some(db.clone())];
                    self.conns[i].send(
                        Pending::DbOpts(db.clone()),
                        None,
                        vec![
                            Stmt::with(
                                "SELECT DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME \
                                 FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?",
                                p(),
                            ),
                            Stmt::with(
                                "SELECT TABLE_NAME FROM information_schema.TABLES \
                                 WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'BASE TABLE' ORDER BY TABLE_NAME",
                                p(),
                            ),
                        ],
                        Mode::StopOnError,
                    );
                    self.newdb = Some(NewDbDialog {
                        conn: i,
                        name: db,
                        charset: String::new(),
                        collation: String::new(),
                        edit: true,
                        loading: true,
                        tables: Vec::new(),
                        convert: false,
                    });
                }
                Act::Ddl(i, db, cat, name, kind) => self.acts_ddl(i, db, cat, name, kind),
                Act::Users(i) => {
                    if self.conns[i].state != CState::Open {
                        self.open_conn(i, ctx);
                    }
                    self.open_users(i, ctx);
                }
                Act::Sync(pre) => self.open_sync(pre, ctx),
                Act::Monitor(i) => {
                    if self.conns[i].state != CState::Open {
                        self.open_conn(i, ctx);
                    }
                    self.open_monitor(i, ctx);
                }
                Act::Confirm(c) => self.confirm = Some(c),
                Act::DbListSel(n) => self.db_list_sel = Some(n),
                Act::DeleteQuery(i, db, name) => self.delete_query = Some((i, db, name)),
                Act::Import(i, db, table) => {
                    let w = ImportWizard::new(&self.conns[i].cfg, ctx, &db, table.as_deref());
                    self.import = Some((i, w));
                }
                Act::Dump(i, db, table) => {
                    let dlg = DumpDialog::new(&db, table.as_deref(), self.settings.dump.clone());
                    self.dump_dlg = Some((i, dlg));
                }
                Act::Export(i, db, name) => {
                    let dlg = ExportDialog::new(&name, None);
                    self.export_dlg = Some((i, db, name, dlg));
                }
                Act::Design(i, db, table) => {
                    if self.conns[i].state != CState::Open {
                        self.open_conn(i, ctx);
                    }
                    self.open_design(i, &db, table.as_deref(), ctx);
                }
            }
        }
    }

    fn select(&mut self, s: Sel) {
        if let (Some(ci), Some(db)) = (s.conn, s.db.as_ref()) {
            let need = self.conns[ci].db(db).is_some_and(|d| !d.loaded && !d.loading);
            if need {
                self.load_objects(ci, db);
            }
        }
        self.sel = s;
        self.active = 0;
    }

    fn acts_ddl(&mut self, i: usize, db: String, cat: Cat, name: String, kind: String) {
        let fq = format!("{}.{}", qi(&db), qi(&name));
        let (sql, delim) = match cat {
            Cat::Tables => (format!("SHOW CREATE TABLE {fq}"), false),
            Cat::Views => (format!("SHOW CREATE VIEW {fq}"), false),
            Cat::Routines => {
                let k = if kind == "PROCEDURE" { "PROCEDURE" } else { "FUNCTION" };
                (format!("SHOW CREATE {k} {fq}"), true)
            }
            Cat::Events => (format!("SHOW CREATE EVENT {fq}"), true),
            Cat::Queries => return,
        };
        self.conns[i].send(Pending::Ddl { db: Some(db), name, delim }, None, vec![Stmt::plain(sql)], Mode::StopOnError);
    }

    // ------------------------------------------------------------------ UI-Teile

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let big = tool_button;
            if big(ui, Icon::ConnOpen, "Verbindung", false, true) {
                self.conn_dialog = Some(ConnDialog::new(None, ConnConfig::default()));
            }
            let conn = self.sel.conn;
            let open = conn.is_some_and(|i| self.conns[i].state == CState::Open);
            if big(ui, Icon::User, "Benutzer", false, conn.is_some()) {
                if let Some(i) = conn {
                    self.acts.push(Act::Users(i));
                }
            }
            if big(ui, Icon::Monitor, "Monitor", false, conn.is_some()) {
                if let Some(i) = conn {
                    self.acts.push(Act::Monitor(i));
                }
            }
            if big(ui, Icon::Sync, "Abgleich", false, !self.conns.is_empty()) {
                let pre = conn.zip(self.sel.db.clone());
                self.acts.push(Act::Sync(pre));
            }
            ui.separator();
            for cat in Cat::ALL {
                let active = self.active == 0 && self.sel.cat == cat && self.sel.db.is_some();
                if big(ui, cat.icon(), cat.label(), active, open && self.sel.db.is_some()) {
                    let mut s = self.sel.clone();
                    s.cat = cat;
                    s.obj = None;
                    self.acts.push(Act::Select(s));
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if big(ui, Icon::Options, "Optionen", self.settings_open, true) {
                    self.settings_open = !self.settings_open;
                }
            });
        });
    }

    fn statusbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(i) = self.sel.conn {
                let c = &self.conns[i];
                let color = match c.state {
                    CState::Open => Color32::from_rgb(0x2e, 0xa0, 0x43),
                    CState::Connecting => Color32::from_rgb(0xd4, 0xa0, 0x17),
                    CState::Failed => Color32::from_rgb(0xd0, 0x30, 0x30),
                    CState::Closed => Color32::GRAY,
                };
                ui.colored_label(color, "⏺");
                ui.label(format!("{}  {}@{}:{}", c.cfg.name, c.cfg.user, c.cfg.host, c.cfg.port));
                if !c.version.is_empty() {
                    ui.separator();
                    ui.label(&c.version);
                }
                if let Some(db) = &self.sel.db {
                    ui.separator();
                    ui.label(format!("Datenbank: {db}"));
                }
            }
            ui.separator();
            let mut cancel = None;
            for (i, (name, job)) in self.export_jobs.iter().enumerate() {
                ui.spinner();
                let stage = job.stage();
                if stage.is_empty() || stage == *name {
                    ui.label(format!("Exportiere {name} … {} Zeilen", job.rows()));
                } else {
                    ui.label(format!("Exportiere {name} ({stage}) … {} Zeilen", job.rows()));
                }
                if ui.small_button("Abbrechen").clicked() {
                    cancel = Some(i);
                }
                ui.separator();
            }
            if let Some(i) = cancel {
                self.export_jobs[i].1.cancel();
            }
            ui.weak(&self.status);
        });
    }

    fn tree(&mut self, ui: &mut egui::Ui) {
        #[derive(Clone)]
        enum Node {
            Conn(usize),
            Db(usize, usize),
            Cat(usize, usize, Cat),
            /// TelliMATE-Ordner (Index in `TM_FOLDERS`).
            Folder(usize, usize, usize),
            Obj(usize, usize, Cat, usize),
        }
        let tm = self.settings.tellimate;
        let mut nodes: Vec<(u8, Node)> = Vec::new();
        for (ci, c) in self.conns.iter().enumerate() {
            nodes.push((0, Node::Conn(ci)));
            if c.state != CState::Open || !c.open {
                continue;
            }
            for (di, d) in c.dbs.iter().enumerate() {
                nodes.push((1, Node::Db(ci, di)));
                if !d.open {
                    continue;
                }
                for cat in Cat::ALL {
                    nodes.push((2, Node::Cat(ci, di, cat)));
                    if !d.cat_open[cat.idx()] {
                        continue;
                    }
                    let objs = &d.objs[cat.idx()];
                    if tm && cat == Cat::Tables {
                        // Ordner zuerst, darin die Tabellen ohne Präfix; danach der Rest
                        for f in 0..TM_FOLDERS.len() {
                            let members: Vec<usize> = (0..objs.len()).filter(|&oi| tm_folder(&objs[oi].name) == Some(f)).collect();
                            if members.is_empty() {
                                continue;
                            }
                            nodes.push((3, Node::Folder(ci, di, f)));
                            if d.folder_open[f] {
                                nodes.extend(members.into_iter().map(|oi| (4, Node::Obj(ci, di, cat, oi))));
                            }
                        }
                        for oi in (0..objs.len()).filter(|&oi| tm_folder(&objs[oi].name).is_none()) {
                            nodes.push((3, Node::Obj(ci, di, cat, oi)));
                        }
                    } else {
                        for oi in 0..objs.len() {
                            nodes.push((3, Node::Obj(ci, di, cat, oi)));
                        }
                    }
                }
            }
        }

        let row_h = 20.0;
        let mut acts = Vec::new();
        // Vor show_rows setzen: es rechnet mit Zeilenhöhe + item_spacing.y. Sonst bleibt unten eine Lücke
        // (zu wenige Zeilen gezeichnet) und der Scrollbereich ist zu groß.
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::both().auto_shrink([false, false]).show_rows(ui, row_h, nodes.len(), |ui, range| {
            for (depth, node) in &nodes[range] {
                ui.horizontal(|ui| {
                    ui.set_height(row_h);
                    // Pfeil direkt vor Icon/Name, ohne Lücke
                    ui.spacing_mut().item_spacing.x = 0.0;
                    // Icon (16 px) + Innenabstand oben/unten = genau row_h, sonst stimmt die Zeilenberechnung
                    // von show_rows nicht (Zeilen höher als gedacht → letzte Einträge nicht erreichbar)
                    ui.spacing_mut().button_padding = egui::vec2(2.0, (row_h - 16.0) / 2.0);
                    ui.spacing_mut().interact_size.y = row_h;
                    ui.add_space(*depth as f32 * 14.0 + 2.0);
                    let (expandable, expanded, icon, label, selected) = match node {
                        Node::Conn(ci) => {
                            let c = &self.conns[*ci];
                            let icon = match c.state {
                                CState::Open => Icon::ConnOpen,
                                CState::Connecting => Icon::ConnBusy,
                                CState::Failed => Icon::ConnFailed,
                                CState::Closed => Icon::ConnClosed,
                            };
                            let sel = self.sel.conn == Some(*ci) && self.sel.db.is_none();
                            (true, c.open && c.state == CState::Open, icon, c.cfg.name.clone(), sel)
                        }
                        Node::Db(ci, di) => {
                            let d = &self.conns[*ci].dbs[*di];
                            let sel = self.sel.conn == Some(*ci) && self.sel.db.as_deref() == Some(&d.name) && self.sel.obj.is_none();
                            let icon = if d.open || d.loaded { Icon::Database } else { Icon::DatabaseClosed };
                            (true, d.open, icon, d.name.clone(), sel && self.active == 0)
                        }
                        Node::Cat(ci, di, cat) => {
                            let d = &self.conns[*ci].dbs[*di];
                            let n = d.objs[cat.idx()].len();
                            let sel = self.sel.conn == Some(*ci)
                                && self.sel.db.as_deref() == Some(&d.name)
                                && self.sel.cat == *cat
                                && self.sel.obj.is_none();
                            let label = if d.loaded { format!("{} ({n})", cat.label()) } else { cat.label().to_string() };
                            (n > 0, d.cat_open[cat.idx()], cat.icon(), label, sel && self.active == 0)
                        }
                        Node::Obj(ci, di, cat, oi) => {
                            let d = &self.conns[*ci].dbs[*di];
                            let o = &d.objs[cat.idx()][*oi];
                            let sel = self.sel.conn == Some(*ci)
                                && self.sel.db.as_deref() == Some(&d.name)
                                && self.sel.cat == *cat
                                && self.sel.obj.as_deref() == Some(&o.name);
                            let icon = if *cat == Cat::Routines && o.kind == "PROCEDURE" { Icon::Procedure } else { cat.icon() };
                            // Im TelliMATE-Ordner nur der Rest des Namens
                            let label = match tm_folder(&o.name).filter(|_| *depth == 4) {
                                Some(f) => o.name[TM_FOLDERS[f].0.len()..].to_string(),
                                None => o.name.clone(),
                            };
                            (false, false, icon, label, sel)
                        }
                        Node::Folder(ci, di, f) => {
                            let d = &self.conns[*ci].dbs[*di];
                            let n = d.objs[Cat::Tables.idx()].iter().filter(|o| tm_folder(&o.name) == Some(*f)).count();
                            (true, d.folder_open[*f], Icon::Open, format!("{} ({n})", TM_FOLDERS[*f].1), false)
                        }
                    };
                    let arrow = if !expandable { "" } else if expanded { "⏷" } else { "⏵" };
                    let ar = ui
                        .add_sized([12.0, row_h], egui::Label::new(RichText::new(arrow).weak()).sense(Sense::click()).selectable(false));
                    let resp = ui.add(
                        egui::Button::selectable(selected, (icons::atom(icon, 16.0), label)).frame_when_inactive(false),
                    );
                    let toggle = ar.clicked();
                    match node {
                        Node::Conn(ci) => {
                            let ci = *ci;
                            if toggle || resp.double_clicked() {
                                acts.push(Act::ToggleConn(ci));
                            }
                            let s = Sel { conn: Some(ci), ..Default::default() };
                            if resp.double_clicked() {
                                acts.push(Act::Select(s));
                            } else if resp.clicked() {
                                acts.push(Act::Mark(s));
                            }
                            if let Some(e) = &self.conns[ci].error {
                                resp.clone().on_hover_text(e);
                            }
                            let state = self.conns[ci].state;
                            resp.context_menu(|ui| {
                                if state == CState::Open {
                                    if ui.button("Verbindung schließen").clicked() {
                                        acts.push(Act::CloseConn(ci));
                                        ui.close();
                                    }
                                    if ui.add(icons::button(Icon::Refresh, "Aktualisieren")).clicked() {
                                        acts.push(Act::RefreshConn(ci));
                                        ui.close();
                                    }
                                } else if ui.button("Verbindung öffnen").clicked() {
                                    acts.push(Act::ToggleConn(ci));
                                    ui.close();
                                }
                                ui.separator();
                                if ui.add(icons::button(Icon::NewQuery, "Neue Abfrage")).clicked() {
                                    acts.push(Act::NewQuery(ci, None, String::new()));
                                    ui.close();
                                }
                                if ui.add_enabled(state == CState::Open, icons::button(Icon::NewDatabase, "Neue Datenbank …")).clicked() {
                                    acts.push(Act::NewDb(ci));
                                    ui.close();
                                }
                                if ui.add(icons::button(Icon::User, "Benutzer verwalten")).clicked() {
                                    acts.push(Act::Users(ci));
                                    ui.close();
                                }
                                if ui.add(icons::button(Icon::Monitor, "Servermonitor")).clicked() {
                                    acts.push(Act::Monitor(ci));
                                    ui.close();
                                }
                                ui.separator();
                                if ui.add(icons::button(Icon::Edit, "Verbindung bearbeiten …")).clicked() {
                                    acts.push(Act::EditConn(ci));
                                    ui.close();
                                }
                                if ui.add(icons::button(Icon::Delete, "Verbindung entfernen")).clicked() {
                                    acts.push(Act::DeleteConn(ci));
                                    ui.close();
                                }
                            });
                        }
                        Node::Db(ci, di) => {
                            let (ci, name) = (*ci, self.conns[*ci].dbs[*di].name.clone());
                            if toggle || resp.double_clicked() {
                                acts.push(Act::ToggleDb(ci, name.clone()));
                            }
                            let s = Sel { conn: Some(ci), db: Some(name.clone()), cat: self.sel.cat, obj: None };
                            if resp.double_clicked() {
                                acts.push(Act::Select(s));
                            } else if resp.clicked() {
                                acts.push(Act::Mark(s));
                            }
                            resp.context_menu(|ui| db_menu(ui, &mut acts, ci, &name));
                        }
                        Node::Cat(ci, di, cat) => {
                            let (ci, cat, name) = (*ci, *cat, self.conns[*ci].dbs[*di].name.clone());
                            if toggle || resp.double_clicked() {
                                acts.push(Act::ToggleCat(ci, name.clone(), cat));
                            }
                            let s = Sel { conn: Some(ci), db: Some(name.clone()), cat, obj: None };
                            // Einfachklick zeigt rechts die Objekte als Liste (Doppelklick klappt zusätzlich auf/zu)
                            if resp.clicked() || resp.double_clicked() {
                                acts.push(Act::SelectList(s));
                            }
                        }
                        Node::Folder(ci, di, f) => {
                            if toggle || resp.clicked() {
                                acts.push(Act::ToggleFolder(*ci, self.conns[*ci].dbs[*di].name.clone(), *f));
                            }
                        }
                        Node::Obj(ci, di, cat, oi) => {
                            let ci = *ci;
                            let d = &self.conns[ci].dbs[*di];
                            let (db, oname, kind) = (d.name.clone(), d.objs[cat.idx()][*oi].name.clone(), d.objs[cat.idx()][*oi].kind.clone());
                            let s = Sel { conn: Some(ci), db: Some(db.clone()), cat: *cat, obj: Some(oname.clone()) };
                            if resp.double_clicked() {
                                acts.push(Act::Mark(s));
                                acts.push(Act::OpenObj(ci, db.clone(), *cat, oname.clone()));
                            } else if resp.clicked() {
                                acts.push(Act::Mark(s));
                            }
                            let cat = *cat;
                            resp.context_menu(|ui| obj_menu(ui, &mut acts, ci, &db, cat, &oname, &kind));
                        }
                    }
                });
            }
        });
        self.acts.extend(acts);
    }

    fn objects_view(&mut self, ui: &mut egui::Ui) {
        let Some(ci) = self.sel.conn else {
            ui.vertical_centered(|ui| {
                ui.add_space(60.0);
                ui.heading("MariaDB Manager");
                ui.weak("Verbindung links auswählen oder neu anlegen.");
                ui.add_space(10.0);
                if ui.add(icons::button(Icon::NewConn, "Neue Verbindung …")).clicked() {
                    self.conn_dialog = Some(ConnDialog::new(None, ConnConfig::default()));
                }
            });
            return;
        };
        let c = &self.conns[ci];
        if c.state != CState::Open {
            ui.vertical_centered(|ui| {
                ui.add_space(60.0);
                ui.heading(&c.cfg.name);
                match c.state {
                    CState::Connecting => {
                        ui.spinner();
                        ui.label("Verbinde …");
                    }
                    CState::Failed => {
                        ui.colored_label(ui.visuals().error_fg_color, c.error.clone().unwrap_or_default());
                    }
                    _ => {}
                }
                if c.state != CState::Connecting && ui.button("Verbindung öffnen").clicked() {
                    self.acts.push(Act::ToggleConn(ci));
                }
            });
            return;
        }

        let Some(db) = self.sel.db.clone() else {
            // Datenbankliste
            ui.horizontal(|ui| {
                if ui.add(icons::button(Icon::NewDatabase, "Neue Datenbank")).clicked() {
                    self.acts.push(Act::NewDb(ci));
                }
                if ui.add(icons::button(Icon::Refresh, "Aktualisieren")).clicked() {
                    self.acts.push(Act::RefreshConn(ci));
                }
                ui.separator();
                ui.add(egui::TextEdit::singleline(&mut self.obj_filter).hint_text("🔍 Filter").desired_width(180.0));
            });
            ui.separator();
            let mut acts = Vec::new();
            let f = self.obj_filter.to_lowercase();
            let list: Vec<&DbNode> = c.dbs.iter().filter(|d| f.is_empty() || d.name.to_lowercase().contains(&f)).collect();
            TableBuilder::new(ui)
                .id_salt("dblist")
                .striped(true)
                .resizable(true)
                .sense(Sense::click())
                .auto_shrink([false, false])
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::initial(260.0).at_least(80.0).clip(true))
                .column(Column::remainder().clip(true))
                .header(22.0, |mut h| {
                    h.col(|ui| {
                        ui.strong("Name");
                    });
                    h.col(|ui| {
                        ui.strong("Objekte");
                    });
                })
                .body(|body| {
                    body.rows(20.0, list.len(), |mut row| {
                        let d = list[row.index()];
                        row.set_selected(self.db_list_sel.as_deref() == Some(&d.name));
                        let (_, r1) = row.col(|ui| {
                            icons::show(ui, if d.open || d.loaded { Icon::Database } else { Icon::DatabaseClosed }, 16.0);
                            ui.add(egui::Label::new(d.name.as_str()).selectable(false).truncate());
                        });
                        let (_, r2) = row.col(|ui| {
                            if d.loaded {
                                let n: usize = d.objs.iter().map(|o| o.len()).sum();
                                ui.add(egui::Label::new(n.to_string()).selectable(false));
                            }
                        });
                        let r = r1.union(r2);
                        if r.clicked() {
                            acts.push(Act::DbListSel(d.name.clone()));
                        }
                        if r.double_clicked() {
                            acts.push(Act::Select(Sel { conn: Some(ci), db: Some(d.name.clone()), cat: Cat::Tables, obj: None }));
                            if !d.open {
                                acts.push(Act::ToggleDb(ci, d.name.clone()));
                            }
                        }
                        r.context_menu(|ui| db_menu(ui, &mut acts, ci, &d.name));
                    });
                });
            self.acts.extend(acts);
            return;
        };

        let cat = self.sel.cat;
        let Some(d) = c.db(&db) else { return };
        let objs = &d.objs[cat.idx()];
        let sel_obj = self.sel.obj.clone();

        // Objekt-Werkzeugleiste
        let mut acts = Vec::new();
        ui.horizontal(|ui| {
            let has = sel_obj.is_some();
            let kind = sel_obj.as_ref().and_then(|n| objs.iter().find(|o| &o.name == n)).map(|o| o.kind.clone()).unwrap_or_default();
            match cat {
                Cat::Tables | Cat::Views | Cat::Queries => {
                    if ui.add_enabled(has, icons::button(Icon::Open, "Öffnen")).clicked() {
                        acts.push(Act::OpenObj(ci, db.clone(), cat, sel_obj.clone().unwrap()));
                    }
                }
                _ => {}
            }
            if cat == Cat::Tables {
                if ui.add_enabled(has, icons::button(Icon::Design, "Tabelle entwerfen")).clicked() {
                    acts.push(Act::Design(ci, db.clone(), sel_obj.clone()));
                }
                if ui.add(icons::button(Icon::NewTable, "Neue Tabelle")).clicked() {
                    acts.push(Act::Design(ci, db.clone(), None));
                }
                if ui.add(icons::button(Icon::Import, "Import-Assistent")).clicked() {
                    acts.push(Act::Import(ci, db.clone(), sel_obj.clone()));
                }
            }
            if cat != Cat::Queries && ui.add_enabled(has, icons::button(Icon::Ddl, "DDL / Bearbeiten")).clicked() {
                acts.push(Act::Ddl(ci, db.clone(), cat, sel_obj.clone().unwrap(), kind.clone()));
            }
            if cat == Cat::Tables
                && ui.add_enabled(has, icons::button(Icon::Truncate, "Leeren")).clicked()
            {
                let n = sel_obj.clone().unwrap();
                acts.push(Act::Confirm(Confirm {
                    title: "Tabelle leeren".into(),
                    text: format!("Alle Daten aus „{n}“ löschen (TRUNCATE)?"),
                    conn: ci,
                    db: Some(db.clone()),
                    stmts: vec![format!("TRUNCATE TABLE {}", qi(&n))],
                    refresh_db: Some(db.clone()),
                    refresh_dbs: false,
                }));
            }
            if ui.add_enabled(has, icons::button(Icon::Delete, "Löschen")).clicked() {
                let n = sel_obj.clone().unwrap();
                if cat == Cat::Queries {
                    acts.push(Act::DeleteQuery(ci, db.clone(), n));
                } else {
                    acts.push(Act::Confirm(drop_confirm(ci, &db, cat, &n, &kind)));
                }
            }
            if ui.add(icons::button(Icon::NewQuery, "Neue Abfrage")).clicked() {
                acts.push(Act::NewQuery(ci, Some(db.clone()), String::new()));
            }
            if ui.add(egui::Button::new(icons::atom(Icon::Refresh, 16.0))).on_hover_text("Aktualisieren").clicked() {
                acts.push(Act::RefreshDb(ci, db.clone()));
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut self.obj_filter).hint_text("🔍 Filter").desired_width(180.0));
            if d.loading {
                ui.spinner();
            }
        });
        ui.separator();

        let f = self.obj_filter.to_lowercase();
        let list: Vec<&Obj> = objs.iter().filter(|o| f.is_empty() || o.name.to_lowercase().contains(&f)).collect();
        if self.obj_list {
            explorer_list(ui, &list, cat, sel_obj.as_deref(), ci, &db, &mut acts);
            self.acts.extend(acts);
            return;
        }
        let headers: &[&str] = match cat {
            Cat::Tables => &["Name", "Zeilen", "Engine", "Daten", "Index", "Auto-Inc.", "Geändert", "Kollation", "Kommentar"],
            Cat::Views => &["Name", "Geändert", "Kommentar"],
            Cat::Routines => &["Name", "Typ", "Rückgabe", "Geändert", "Kommentar"],
            Cat::Events => &["Name", "Status", "Zeitplan", "Zuletzt ausgeführt", "Kommentar"],
            Cat::Queries => &["Name", "Geändert", "Größe"],
        };
        let ncols = headers.len();
        egui::ScrollArea::horizontal().auto_shrink([false, false]).show(ui, |ui| {
            let mut tb = TableBuilder::new(ui)
                .id_salt(("objlist", cat))
                .striped(true)
                .resizable(true)
                .sense(Sense::click())
                .auto_shrink([false, false])
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::initial(220.0).at_least(80.0).clip(true));
            for _ in 1..ncols {
                tb = tb.column(Column::initial(110.0).at_least(40.0).clip(true));
            }
            tb.header(22.0, |mut h| {
                for t in headers {
                    h.col(|ui| {
                        ui.strong(*t);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, list.len(), |mut row| {
                    let o = list[row.index()];
                    row.set_selected(sel_obj.as_deref() == Some(&o.name));
                    let icon = if cat == Cat::Routines && o.kind == "PROCEDURE" { Icon::Procedure } else { cat.icon() };
                    let cells: Vec<String> = match cat {
                        Cat::Tables => vec![
                            o.name.clone(),
                            o.rows.clone(),
                            o.engine.clone(),
                            fmt_size(o.data_len),
                            fmt_size(o.index_len),
                            o.auto_inc.clone(),
                            o.updated.clone(),
                            o.collation.clone(),
                            o.comment.clone(),
                        ],
                        Cat::Views => vec![o.name.clone(), o.updated.clone(), o.comment.clone()],
                        Cat::Routines => vec![
                            o.name.clone(),
                            o.kind.clone(),
                            o.extra.clone(),
                            o.updated.clone(),
                            o.comment.clone(),
                        ],
                        Cat::Events => vec![
                            o.name.clone(),
                            o.kind.clone(),
                            o.extra.clone(),
                            o.updated.clone(),
                            o.comment.clone(),
                        ],
                        Cat::Queries => vec![o.name.clone(), o.updated.clone(), fmt_size(o.data_len)],
                    };
                    let mut clicked = false;
                    let mut dbl = false;
                    let mut menu_resp = None;
                    for (i, t) in cells.iter().enumerate() {
                        let right = (cat == Cat::Tables && (1..=5).contains(&i) && i != 2) || (cat == Cat::Queries && i == 2);
                        let (_, r) = row.col(|ui| {
                            if right {
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    ui.add(egui::Label::new(t.as_str()).selectable(false).truncate());
                                });
                            } else {
                                if i == 0 {
                                    icons::show(ui, icon, 16.0);
                                }
                                ui.add(egui::Label::new(t.as_str()).selectable(false).truncate());
                            }
                        });
                        clicked |= r.clicked();
                        dbl |= r.double_clicked();
                        if menu_resp.is_none() {
                            menu_resp = Some(r);
                        } else if let Some(m) = menu_resp.take() {
                            menu_resp = Some(m.union(r));
                        }
                    }
                    if clicked {
                        acts.push(Act::Select(Sel { conn: Some(ci), db: Some(db.clone()), cat, obj: Some(o.name.clone()) }));
                    }
                    if dbl {
                        acts.push(Act::OpenObj(ci, db.clone(), cat, o.name.clone()));
                    }
                    if let Some(r) = menu_resp {
                        r.context_menu(|ui| obj_menu(ui, &mut acts, ci, &db, cat, &o.name, &o.kind));
                    }
                });
            });
        });
        self.acts.extend(acts);
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        let mut close = None;
        egui::ScrollArea::horizontal().id_salt("tabbar").auto_shrink([false, true]).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if ui.add(egui::Button::selectable(self.active == 0, (icons::atom(Icon::Objects, 16.0), "Objekte"))).clicked() {
                    self.active = 0;
                }
                for (i, t) in self.tabs.iter().enumerate() {
                    let (icon, title) = match &t.kind {
                        TabKind::Data(d) => (Icon::Table, d.title()),
                        TabKind::Query(q) => (Icon::Query, format!("{}{} ({})", q.title, if q.dirty() { " •" } else { "" }, t.conn_name)),
                        TabKind::Users(_) => (Icon::User, format!("Benutzer ({})", t.conn_name)),
                        TabKind::Monitor(_) => (Icon::Monitor, format!("Monitor ({})", t.conn_name)),
                        TabKind::Sync(s) => (Icon::Sync, s.title()),
                        TabKind::Design(d) => (Icon::Design, d.title()),
                    };
                    let sel = self.active == i + 1;
                    // Schließen-Kreuz als Teil des Reiters (Platzhalter-Atom, danach eigene Klickfläche)
                    let close_id = egui::IdSalt::new(("tab_close", i));
                    let r = egui::Button::selectable(sel, (icons::atom(icon, 16.0), title, egui::Atom::custom(close_id, egui::vec2(14.0, 14.0))))
                        .frame_when_inactive(true)
                        .atom_ui(ui);
                    let mut close_hit = false;
                    if let Some(rect) = r.rect(close_id) {
                        let cr = ui.interact(rect, ui.scope_id().with(("tab_close", i)), Sense::click()).on_hover_text("Schließen (Strg+W)");
                        let col = if cr.hovered() { ui.visuals().error_fg_color } else { ui.visuals().weak_text_color() };
                        if cr.hovered() {
                            ui.painter().rect_filled(rect.expand(1.0), 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
                        }
                        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "✖", egui::FontId::proportional(11.0), col);
                        close_hit = cr.clicked();
                    }
                    if close_hit || r.response.middle_clicked() {
                        close = Some(i + 1);
                    } else if r.response.clicked() {
                        self.active = i + 1;
                    }
                }
            });
        });
        if let Some(i) = close {
            self.close_tab(i);
        }
    }

    // ------------------------------------------------------------------ Dialoge

    fn dialogs(&mut self, ctx: &egui::Context) {
        // Verbindungsdialog
        if let Some(dlg) = self.conn_dialog.as_mut() {
            let mut result = 0; // 1 = OK, 2 = Abbrechen
            if let Some(w) = dlg.test.as_mut() {
                if let Some(r) = w.poll() {
                    if r.tag == dlg.test_tag {
                        dlg.test_result = Some(match r.error {
                            Some(e) => Err(e),
                            None => Ok(r.set(0).and_then(|s| s.rows.first()).and_then(|x| x[0].clone()).unwrap_or_default()),
                        });
                        dlg.test = None;
                    }
                }
            }
            let title = if dlg.idx.is_some() { "Verbindung bearbeiten" } else { "Neue Verbindung" };
            egui::Window::new(title).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                let c = &mut dlg.cfg;
                egui::Grid::new("conn_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Verbindungsname:");
                    ui.add(egui::TextEdit::singleline(&mut c.name).desired_width(240.0));
                    ui.end_row();
                    ui.label("Host:");
                    ui.add(egui::TextEdit::singleline(&mut c.host).desired_width(240.0));
                    ui.end_row();
                    ui.label("Port:");
                    ui.add(egui::DragValue::new(&mut c.port).range(1..=65535));
                    ui.end_row();
                    ui.label("Benutzername:");
                    ui.add(egui::TextEdit::singleline(&mut c.user).desired_width(240.0));
                    ui.end_row();
                    ui.label("Passwort:");
                    ui.add(egui::TextEdit::singleline(&mut c.password).password(true).desired_width(240.0));
                    ui.end_row();
                    ui.label("");
                    ui.checkbox(&mut c.save_password, "Passwort speichern");
                    ui.end_row();
                    ui.label("Standard-DB:");
                    ui.add(egui::TextEdit::singleline(&mut c.default_db).hint_text("optional").desired_width(240.0));
                    ui.end_row();
                    ui.label("SSL/TLS:");
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut c.use_ssl, "verwenden");
                        ui.add_enabled(c.use_ssl, egui::Checkbox::new(&mut c.ssl_verify, "Zertifikat prüfen"));
                    });
                    ui.end_row();
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.add_enabled(dlg.test.is_none(), egui::Button::new("Verbindung testen")).clicked() {
                        let mut w = Worker::spawn(dlg.cfg.clone(), ctx.clone());
                        dlg.test_tag = w.query(None, &["SELECT VERSION()"]);
                        dlg.test = Some(w);
                        dlg.test_result = None;
                    }
                    if dlg.test.is_some() {
                        ui.spinner();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Abbrechen").clicked() {
                            result = 2;
                        }
                        if ui.add_enabled(!dlg.cfg.name.trim().is_empty(), egui::Button::new("OK")).clicked() {
                            result = 1;
                        }
                    });
                });
                match &dlg.test_result {
                    Some(Ok(v)) => {
                        ui.colored_label(Color32::from_rgb(0x2e, 0x9e, 0x44), format!("✔ Verbindung erfolgreich – {v}"));
                    }
                    Some(Err(e)) => {
                        ui.colored_label(ui.visuals().error_fg_color, format!("✖ {e}"));
                    }
                    None => {}
                }
            });
            if ctx.input(|i| i.key_pressed(Key::Escape)) {
                result = 2;
            }
            if result == 1 {
                let dlg = self.conn_dialog.take().unwrap();
                match dlg.idx {
                    Some(i) => {
                        let (old, new) = (self.conns[i].cfg.name.clone(), dlg.cfg.name.clone());
                        if old != new {
                            queries::rename_conn(&old, &new);
                            let uid = self.conns[i].uid;
                            for t in self.tabs.iter_mut().filter(|t| t.conn_uid == uid) {
                                t.conn_name = new.clone();
                                if let TabKind::Query(q) = &mut t.kind {
                                    q.conn_name = new.clone();
                                }
                            }
                        }
                        let changed = self.conns[i].cfg != dlg.cfg;
                        self.conns[i].cfg = dlg.cfg;
                        if changed {
                            self.close_conn(i);
                        }
                    }
                    None => {
                        self.next_uid += 1;
                        self.conns.push(ConnNode::new(self.next_uid, dlg.cfg));
                        let i = self.conns.len() - 1;
                        self.sel = Sel { conn: Some(i), ..Default::default() };
                        self.open_conn(i, ctx);
                    }
                }
                self.save_settings();
            } else if result == 2 {
                self.conn_dialog = None;
            }
        }

        // Verbindung entfernen
        if let Some(i) = self.delete_conn {
            let mut choice = 0;
            let name = self.conns[i].cfg.name.clone();
            egui::Modal::new(egui::Id::unique("del_conn")).show(ctx, |ui| {
                ui.heading("Verbindung entfernen");
                ui.label(format!("Verbindung „{name}“ aus der Liste entfernen? (Die Datenbank bleibt unverändert.)"));
                ui.horizontal(|ui| {
                    if ui.button("Entfernen").clicked() {
                        choice = 1;
                    }
                    if ui.button("Abbrechen").clicked() {
                        choice = 2;
                    }
                });
            });
            if choice == 1 {
                let uid = self.conns[i].uid;
                self.conns.remove(i);
                self.tabs.retain(|t| t.conn_uid != uid);
                self.active = 0;
                self.sel = Sel::default();
                self.save_settings();
            }
            if choice != 0 {
                self.delete_conn = None;
            }
        }

        // Bestätigung für gefährliche Aktionen
        if let Some(cf) = &self.confirm {
            let mut choice = 0;
            egui::Modal::new(egui::Id::unique("confirm")).show(ctx, |ui| {
                ui.set_max_width(520.0);
                ui.heading(&cf.title);
                ui.label(&cf.text);
                ui.add_space(4.0);
                ui.label(RichText::new(cf.stmts.join(";\n")).monospace().weak());
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Ausführen").color(ui.visuals().error_fg_color)).clicked() {
                        choice = 1;
                    }
                    if ui.button("Abbrechen").clicked() {
                        choice = 2;
                    }
                });
            });
            if choice == 1 {
                let cf = self.confirm.take().unwrap();
                let what = cf.title.clone();
                if cf.refresh_dbs && self.sel.conn == Some(cf.conn) {
                    self.sel.db = None;
                }
                self.conns[cf.conn].send(
                    Pending::Exec { refresh_db: cf.refresh_db, refresh_dbs: cf.refresh_dbs, what },
                    cf.db,
                    cf.stmts.into_iter().map(Stmt::plain).collect(),
                    Mode::StopOnError,
                );
            } else if choice == 2 {
                self.confirm = None;
            }
        }

        // Neue Datenbank
        if let Some(nd) = self.newdb.as_mut() {
            let mut choice = 0;
            egui::Modal::new(egui::Id::unique("newdb")).show(ctx, |ui| {
                ui.heading(if nd.edit { "Datenbank bearbeiten" } else { "Neue Datenbank" });
                if nd.loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Lese Einstellungen …");
                    });
                    if ui.button("Abbrechen").clicked() {
                        choice = 2;
                    }
                    return;
                }
                egui::Grid::new("newdb_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Name:");
                    ui.add_enabled(!nd.edit, egui::TextEdit::singleline(&mut nd.name).desired_width(240.0));
                    ui.end_row();
                    ui.label("Zeichensatz:");
                    egui::ComboBox::from_id_salt("nd_cs").selected_text(nd.charset.clone()).show_ui(ui, |ui| {
                        for cs in ["utf8mb4", "utf8mb3", "latin1", "ascii", "binary"] {
                            if ui.selectable_label(nd.charset == cs, cs).clicked() {
                                nd.charset = cs.into();
                                nd.collation = match cs {
                                    "utf8mb4" => "utf8mb4_unicode_ci",
                                    "utf8mb3" => "utf8mb3_general_ci",
                                    "latin1" => "latin1_german2_ci",
                                    "ascii" => "ascii_general_ci",
                                    _ => "binary",
                                }
                                .into();
                            }
                        }
                    });
                    ui.end_row();
                    ui.label("Kollation:");
                    ui.add(egui::TextEdit::singleline(&mut nd.collation).desired_width(240.0));
                    ui.end_row();
                });
                if nd.edit {
                    ui.add_space(4.0);
                    ui.weak("Die Änderung gilt nur als Standard für neue Tabellen.");
                    ui.add_enabled(
                        !nd.tables.is_empty(),
                        egui::Checkbox::new(&mut nd.convert, format!("Bestehende Tabellen ({}) konvertieren", nd.tables.len())),
                    )
                    .on_hover_text("ALTER TABLE … CONVERT TO CHARACTER SET … – stellt alle Textspalten der Tabellen um.");
                }
                ui.horizontal(|ui| {
                    let label = if nd.edit { "Speichern" } else { "Anlegen" };
                    if ui.add_enabled(!nd.name.trim().is_empty() && !nd.charset.is_empty(), egui::Button::new(label)).clicked() {
                        choice = 1;
                    }
                    if ui.button("Abbrechen").clicked() {
                        choice = 2;
                    }
                });
            });
            if choice == 1 {
                let nd = self.newdb.take().unwrap();
                let name = nd.name.trim();
                let mut cs = format!("CHARACTER SET {}", nd.charset);
                if !nd.collation.trim().is_empty() {
                    cs.push_str(&format!(" COLLATE {}", nd.collation.trim()));
                }
                if nd.edit {
                    let mut stmts = vec![Stmt::plain(format!("ALTER DATABASE {} {cs}", qi(name)))];
                    if nd.convert {
                        // Fremdschlüssel zwischen noch nicht konvertierten Tabellen würden sonst scheitern
                        stmts.push(Stmt::plain("SET FOREIGN_KEY_CHECKS = 0"));
                        stmts.extend(nd.tables.iter().map(|t| Stmt::plain(format!("ALTER TABLE {}.{} CONVERT TO {cs}", qi(name), qi(t)))));
                        stmts.push(Stmt::plain("SET FOREIGN_KEY_CHECKS = 1"));
                    }
                    // Continue: übrige Tabellen weiter konvertieren und die Prüfung sicher wieder einschalten
                    let mode = if nd.convert { Mode::Continue } else { Mode::StopOnError };
                    self.conns[nd.conn].send(
                        Pending::Exec { refresh_db: Some(name.to_string()), refresh_dbs: false, what: format!("Datenbank {name} geändert") },
                        None,
                        stmts,
                        mode,
                    );
                } else {
                    self.conns[nd.conn].send(
                        Pending::Exec { refresh_db: None, refresh_dbs: true, what: format!("Datenbank {name} angelegt") },
                        None,
                        vec![Stmt::plain(format!("CREATE DATABASE {} {cs}", qi(name)))],
                        Mode::StopOnError,
                    );
                }
            } else if choice == 2 {
                self.newdb = None;
            }
        }

        // Optionen
        if self.settings_open {
            let mut open = true;
            let mut changed = false;
            egui::Window::new("Optionen")
                .open(&mut open)
                .resizable(false)
                .collapsible(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                egui::Grid::new("opt_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Zeilen pro Seite (Datengrid):");
                    changed |= ui.add(egui::DragValue::new(&mut self.settings.page_size).range(50..=1_000_000).speed(50)).changed();
                    ui.end_row();
                    ui.label("Max. Zeilen pro Abfrage-Ergebnis:");
                    changed |= ui.add(egui::DragValue::new(&mut self.settings.max_query_rows).range(100..=10_000_000).speed(1000)).changed();
                    ui.end_row();
                    ui.label("Design:");
                    ui.horizontal(|ui| {
                        for m in crate::config::ThemeMode::ALL {
                            changed |= ui.radio_value(&mut self.settings.theme, m, m.label()).changed();
                        }
                    });
                    ui.end_row();
                    ui.label("Schriftart:");
                    let cur = if self.settings.font.is_empty() { self.fonts.first().copied().unwrap_or("Standard") } else { &self.settings.font };
                    let mut pick = None;
                    egui::ComboBox::from_id_salt("opt_font").selected_text(cur.to_string()).width(200.0).show_ui(ui, |ui| {
                        for f in &self.fonts {
                            if ui.selectable_label(*f == cur, *f).clicked() {
                                pick = Some(f.to_string());
                            }
                        }
                    });
                    if let Some(f) = pick {
                        crate::install_fonts(ctx, &f);
                        self.settings.font = f;
                        changed = true;
                    }
                    ui.end_row();
                    ui.label("Schriftdarstellung:");
                    if ui.checkbox(&mut self.settings.sharp_text, "Scharf (ganze Pixelgrößen, kräftigere Kanten)").changed() {
                        // Schriftgrößen beim nächsten Frame neu runden
                        self.last_ppp = 0.0;
                        changed = true;
                    }
                    ui.end_row();
                    ui.label("Schriftgröße:");
                    // Erst beim Loslassen anwenden – sonst springt das Fenster beim Ziehen mit dem Zoom
                    let r = ui.add(egui::Slider::new(&mut self.settings.font_scale, 0.8..=1.6).step_by(0.05));
                    changed |= r.drag_stopped() || (r.changed() && !r.dragged());
                    ui.end_row();
                    ui.label("Zeilenhöhe im Datengrid:");
                    let r = ui.add(egui::Slider::new(&mut self.settings.grid_row_h, 14.0..=40.0).step_by(1.0).suffix(" px"));
                    if r.changed() {
                        crate::grid::set_row_h(self.settings.grid_row_h);
                    }
                    changed |= r.drag_stopped() || (r.changed() && !r.dragged());
                    ui.end_row();
                    ui.label("TelliMATE-Modus:");
                    changed |= ui
                        .checkbox(&mut self.settings.tellimate, "Tabellen vorgaenge_*, helper_* und tarife_* im Baum als Unterordner")
                        .changed();
                    ui.end_row();
                });
                ui.weak(format!("Gespeichert in: {}", Settings::location()));
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Einstellungen exportieren …").on_hover_text("Verbindungen und gespeicherte Abfragen in eine Datei schreiben").clicked() {
                        self.backup = Some(crate::backup::BackupDlg::export(self.conns.iter().map(|c| c.cfg.clone()).collect()));
                    }
                    if ui.button("Einstellungen importieren …").on_hover_text("Verbindungen und gespeicherte Abfragen aus einer Datei übernehmen").clicked() {
                        self.backup = Some(crate::backup::BackupDlg::import());
                    }
                });
                if cfg!(windows) {
                    if ui.button("Aus Navicat importieren …").clicked() {
                        let found = navicat::find();
                        if found.is_empty() {
                            self.error = Some("Keine Navicat-Verbindungen gefunden.".into());
                        } else {
                            self.navicat = Some(NavicatDlg::new(found));
                        }
                    }
                }
            });
            if changed {
                crate::apply_style(ctx, &self.settings);
                self.save_settings();
            }
            if !open {
                self.settings_open = false;
            }
        }

        // Import-Assistent
        if let Some((ci, w)) = self.import.as_mut() {
            let ci = *ci;
            let open = w.show(ctx);
            if let Some(db) = w.take_done() {
                self.acts.push(Act::RefreshDb(ci, db));
            }
            if !open {
                self.import = None;
            }
        }

        // CSV-Export aus dem Kontextmenü
        if let Some((ci, db, name, dlg)) = self.export_dlg.as_mut() {
            if let Some(r) = dlg.show(ctx, "ctx_export") {
                let (ci, db, name) = (*ci, db.clone(), name.clone());
                self.export_dlg = None;
                if let Some((path, bom)) = r {
                    let sql = format!("SELECT * FROM {}.{}", qi(&db), qi(&name));
                    let job = export::export_query(self.conns[ci].cfg.clone(), sql, path, bom, ctx.clone());
                    self.export_jobs.push((name, job));
                }
            }
        }
        // SQL-Dump
        if let Some((ci, dlg)) = self.dump_dlg.as_mut() {
            if let Some(r) = dlg.show(ctx) {
                let ci = *ci;
                let (db, table) = (dlg.db.clone(), dlg.table.clone());
                self.dump_dlg = None;
                if let Some((path, opts)) = r {
                    self.settings.dump = opts.clone();
                    self.save_settings();
                    let name = match &table {
                        Some(t) => format!("{db}.{t}"),
                        None => db.clone(),
                    };
                    let job = crate::dump::start(self.conns[ci].cfg.clone(), db, table, opts, path, ctx.clone());
                    self.export_jobs.push((name, job));
                }
            }
        }
        let mut i = 0;
        while i < self.export_jobs.len() {
            match self.export_jobs[i].1.poll() {
                Some(res) => {
                    let (name, job) = self.export_jobs.remove(i);
                    match res {
                        Ok(n) => self.status = format!("{name}: {n} Zeilen exportiert nach {}", job.path.display()),
                        Err(e) => self.error = Some(format!("Export {name}: {e}")),
                    }
                }
                None => i += 1,
            }
        }

        // Gespeicherte Abfrage löschen
        if let Some((ci, db, name)) = self.delete_query.clone() {
            let mut choice = 0;
            egui::Modal::new(egui::Id::unique("del_query")).show(ctx, |ui| {
                ui.heading("Abfrage löschen");
                ui.label(format!("Gespeicherte Abfrage „{name}“ ({db}) löschen?"));
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Löschen").color(ui.visuals().error_fg_color)).clicked() {
                        choice = 1;
                    }
                    if ui.button("Abbrechen").clicked() {
                        choice = 2;
                    }
                });
            });
            if choice == 1 {
                if let Err(e) = queries::delete(&self.conns[ci].cfg.name, &db, &name) {
                    self.error = Some(e);
                }
                if self.sel.obj.as_deref() == Some(&name) {
                    self.sel.obj = None;
                }
                self.refresh_saved(ci, &db);
            }
            if choice != 0 {
                self.delete_query = None;
            }
        }

        // Einstellungen exportieren/importieren
        if let Some(dlg) = self.backup.as_mut() {
            let existing: Vec<String> = self.conns.iter().map(|c| c.cfg.name.clone()).collect();
            match dlg.show(ctx, &existing) {
                Some(crate::backup::Outcome::Imported(new)) => {
                    self.add_imported(new);
                    self.save_settings();
                }
                Some(crate::backup::Outcome::Close) => self.backup = None,
                None => {}
            }
        }

        // Navicat-Import
        if let Some(nd) = self.navicat.as_mut() {
            let mut choice = 0; // 1 = Importieren, 2 = Nein, 3 = Bericht schließen
            egui::Modal::new(egui::Id::unique("navicat")).show(ctx, |ui| {
                ui.set_width(620.0);
                ui.heading("Navicat-Einstellungen importieren");
                if let Some(r) = &nd.report {
                    ui.label(r);
                    if ui.button("OK").clicked() {
                        choice = 3;
                    }
                    return;
                }
                let nq: usize = nd.found.iter().map(|c| c.queries).sum();
                ui.label(format!(
                    "Es wurden {} Navicat-Verbindungen mit {nq} gespeicherten Abfragen gefunden. Sollen sie übernommen werden?",
                    nd.found.len()
                ));
                ui.horizontal(|ui| {
                    if ui.small_button("Alle").clicked() {
                        nd.sel.iter_mut().for_each(|s| *s = true);
                    }
                    if ui.small_button("Keine").clicked() {
                        nd.sel.iter_mut().for_each(|s| *s = false);
                    }
                });
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    for (c, s) in nd.found.iter().zip(nd.sel.iter_mut()) {
                        let mut t = format!("{}  –  {}@{}:{}", c.cfg.name, c.cfg.user, c.cfg.host, c.cfg.port);
                        if c.queries > 0 {
                            t.push_str(&format!("  ({} Abfragen)", c.queries));
                        }
                        if c.ssh {
                            t.push_str("  [SSH-Tunnel nicht unterstützt]");
                        }
                        ui.checkbox(s, t);
                    }
                });
                ui.weak("Vorhandene Verbindungen gleichen Namens bleiben unverändert; ihre Abfragen werden ergänzt.");
                ui.weak("Der Import ist später jederzeit über Optionen möglich.");
                ui.horizontal(|ui| {
                    if ui.add_enabled(nd.sel.iter().any(|s| *s), egui::Button::new("Importieren")).clicked() {
                        choice = 1;
                    }
                    if ui.button("Nicht importieren").clicked() {
                        choice = 2;
                    }
                });
            });
            match choice {
                1 => {
                    let nd = self.navicat.as_mut().unwrap();
                    let chosen: Vec<&NavConn> = nd.found.iter().zip(&nd.sel).filter(|(_, s)| **s).map(|(c, _)| c).collect();
                    let existing: Vec<String> = self.conns.iter().map(|c| c.cfg.name.clone()).collect();
                    let (new, rep) = navicat::import(&chosen, &existing);
                    nd.report = Some(rep.text());
                    self.add_imported(new);
                    self.settings.navicat_asked = true;
                    self.save_settings();
                }
                2 => {
                    self.navicat = None;
                    self.settings.navicat_asked = true;
                    self.save_settings();
                    if self.conns.is_empty() {
                        self.conn_dialog = Some(ConnDialog::new(None, ConnConfig { name: "Lokal".into(), ..Default::default() }));
                    }
                }
                3 => self.navicat = None,
                _ => {}
            }
        }

        // Fehlermeldung
        if let Some(e) = self.error.clone() {
            let mut close = false;
            egui::Window::new("Fehler")
                .collapsible(false)
                .resizable(true)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_max_width(560.0);
                    ui.colored_label(ui.visuals().error_fg_color, RichText::new(&e).monospace());
                    ui.horizontal(|ui| {
                        if ui.button("OK").clicked() {
                            close = true;
                        }
                        if ui.button("Kopieren").clicked() {
                            ctx.copy_text(e.clone());
                        }
                    });
                });
            if close || ctx.input(|i| i.key_pressed(Key::Escape)) {
                self.error = None;
            }
        }
    }
}

/// Listenansicht wie im Explorer: nur Icon und Name, spaltenweise von oben nach unten,
/// horizontal scrollbar; es werden nur die sichtbaren Spalten gezeichnet.
fn explorer_list(ui: &mut egui::Ui, list: &[&Obj], cat: Cat, sel: Option<&str>, ci: usize, db: &str, acts: &mut Vec<Act>) {
    const ROW_H: f32 = 20.0;
    const ICON: f32 = 16.0;
    let font = egui::TextStyle::Body.resolve(ui.style());
    let (text, sel_text) = (ui.visuals().text_color(), ui.visuals().selection.stroke.color);
    let max_w = list
        .iter()
        .map(|o| ui.painter().layout_no_wrap(o.name.clone(), font.clone(), text).size().x)
        .fold(60.0f32, f32::max)
        .min(360.0);
    // Icon + Abstand + Name + Innenabstand, dazu Spaltenabstand
    let item_w = ICON + 4.0 + max_w + 8.0;
    let col_w = item_w + 16.0;
    egui::ScrollArea::horizontal().id_salt(("objgrid", cat)).auto_shrink([false, false]).show_viewport(ui, |ui, vp| {
        let rows = ((vp.height() / ROW_H).floor() as usize).max(1);
        let cols = list.len().div_ceil(rows);
        let origin = ui.max_rect().min;
        ui.set_width(cols as f32 * col_w);
        ui.set_height(vp.height());
        let c0 = (vp.min.x / col_w).floor().max(0.0) as usize;
        let c1 = ((vp.max.x / col_w).ceil() as usize).min(cols);
        for c in c0..c1 {
            for r in 0..rows {
                let i = c * rows + r;
                let Some(o) = list.get(i) else { break };
                let rect = egui::Rect::from_min_size(origin + egui::vec2(c as f32 * col_w, r as f32 * ROW_H), egui::vec2(item_w, ROW_H));
                let resp = ui.interact(rect, ui.scope_id().with(("objitem", &o.name)), Sense::click());
                let selected = sel == Some(o.name.as_str());
                if selected {
                    ui.painter().rect_filled(rect, 0.0, ui.visuals().selection.bg_fill);
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
                }
                let icon = if cat == Cat::Routines && o.kind == "PROCEDURE" { Icon::Procedure } else { cat.icon() };
                let ir = egui::Rect::from_center_size(egui::pos2(rect.left() + 2.0 + ICON / 2.0, rect.center().y), egui::vec2(ICON, ICON));
                icons::paint(ui.painter(), ir, icon, true);
                let g = ui.painter().layout_no_wrap(o.name.clone(), font.clone(), if selected { sel_text } else { text });
                let pos = egui::pos2(ir.right() + 4.0, rect.center().y - g.size().y / 2.0);
                ui.painter().with_clip_rect(rect).galley(pos, g, text);
                let s = Sel { conn: Some(ci), db: Some(db.to_string()), cat, obj: Some(o.name.clone()) };
                if resp.double_clicked() {
                    acts.push(Act::OpenObj(ci, db.to_string(), cat, o.name.clone()));
                } else if resp.clicked() || resp.secondary_clicked() {
                    acts.push(Act::SelectList(s));
                }
                resp.context_menu(|ui| obj_menu(ui, acts, ci, db, cat, &o.name, &o.kind));
            }
        }
    });
}

fn db_menu(ui: &mut egui::Ui, acts: &mut Vec<Act>, ci: usize, name: &str) {
    if ui.add(icons::button(Icon::NewQuery, "Neue Abfrage")).clicked() {
        acts.push(Act::NewQuery(ci, Some(name.into()), String::new()));
        ui.close();
    }
    if ui.add(icons::button(Icon::Refresh, "Aktualisieren")).clicked() {
        acts.push(Act::RefreshDb(ci, name.into()));
        ui.close();
    }
    if ui.add(icons::button(Icon::DatabaseClosed, "Datenbank schließen")).clicked() {
        acts.push(Act::CloseDb(ci, name.into()));
        ui.close();
    }
    if ui.add(icons::button(Icon::NewTable, "Neue Tabelle …")).clicked() {
        acts.push(Act::Design(ci, name.to_string(), None));
        ui.close();
    }
    if ui.add(icons::button(Icon::Import, "Importieren (CSV/TXT) …")).clicked() {
        acts.push(Act::Import(ci, name.to_string(), None));
        ui.close();
    }
    if ui.add(icons::button(Icon::Export, "SQL-Dump erstellen …")).clicked() {
        acts.push(Act::Dump(ci, name.to_string(), None));
        ui.close();
    }
    if ui.add(icons::button(Icon::Sync, "Strukturabgleich …")).clicked() {
        acts.push(Act::Sync(Some((ci, name.to_string()))));
        ui.close();
    }
    ui.separator();
    if ui.add(icons::button(Icon::Edit, "Datenbank bearbeiten …")).clicked() {
        acts.push(Act::EditDb(ci, name.to_string()));
        ui.close();
    }
    if ui.add(icons::button(Icon::NewDatabase, "Neue Datenbank …")).clicked() {
        acts.push(Act::NewDb(ci));
        ui.close();
    }
    if ui.add(icons::button(Icon::Delete, "Datenbank löschen …")).clicked() {
        acts.push(Act::Confirm(Confirm {
            title: "Datenbank löschen".into(),
            text: format!("Datenbank „{name}“ mit ALLEN Tabellen unwiderruflich löschen?"),
            conn: ci,
            db: None,
            stmts: vec![format!("DROP DATABASE {}", qi(name))],
            refresh_db: None,
            refresh_dbs: true,
        }));
        ui.close();
    }
}

fn obj_menu(ui: &mut egui::Ui, acts: &mut Vec<Act>, ci: usize, db: &str, cat: Cat, name: &str, kind: &str) {
    if cat == Cat::Queries {
        if ui.add(icons::button(Icon::Open, "Öffnen")).clicked() {
            acts.push(Act::OpenObj(ci, db.into(), cat, name.into()));
            ui.close();
        }
        if ui.add(icons::button(Icon::Copy, "Name kopieren")).clicked() {
            ui.ctx().copy_text(name.to_string());
            ui.close();
        }
        ui.separator();
        if ui.add(icons::button(Icon::Delete, "Löschen …")).clicked() {
            acts.push(Act::DeleteQuery(ci, db.into(), name.into()));
            ui.close();
        }
        return;
    }
    if matches!(cat, Cat::Tables | Cat::Views) && ui.add(icons::button(Icon::Open, "Öffnen")).clicked() {
        acts.push(Act::OpenObj(ci, db.into(), cat, name.into()));
        ui.close();
    }
    if cat == Cat::Tables && ui.add(icons::button(Icon::Design, "Tabelle entwerfen")).clicked() {
        acts.push(Act::Design(ci, db.into(), Some(name.into())));
        ui.close();
    }
    if ui.add(icons::button(Icon::Ddl, "DDL anzeigen / bearbeiten")).clicked() {
        acts.push(Act::Ddl(ci, db.into(), cat, name.into(), kind.into()));
        ui.close();
    }
    if matches!(cat, Cat::Tables | Cat::Views) && ui.add(icons::button(Icon::Export, "Exportieren (CSV) …")).clicked() {
        acts.push(Act::Export(ci, db.into(), name.into()));
        ui.close();
    }
    if cat == Cat::Tables && ui.add(icons::button(Icon::Import, "Importieren (CSV/TXT) …")).clicked() {
        acts.push(Act::Import(ci, db.into(), Some(name.into())));
        ui.close();
    }
    if matches!(cat, Cat::Tables | Cat::Views) && ui.add(icons::button(Icon::Export, "SQL-Dump erstellen …")).clicked() {
        acts.push(Act::Dump(ci, db.into(), Some(name.into())));
        ui.close();
    }
    if matches!(cat, Cat::Tables | Cat::Views) && ui.add(icons::button(Icon::NewQuery, "SELECT-Abfrage")).clicked() {
        acts.push(Act::NewQuery(ci, Some(db.into()), format!("SELECT * FROM {} LIMIT 1000;", qi(name))));
        ui.close();
    }
    if ui.add(icons::button(Icon::Copy, "Name kopieren")).clicked() {
        ui.ctx().copy_text(name.to_string());
        ui.close();
    }
    ui.separator();
    if cat == Cat::Tables && ui.add(icons::button(Icon::Truncate, "Tabelle leeren …")).clicked() {
        acts.push(Act::Confirm(Confirm {
            title: "Tabelle leeren".into(),
            text: format!("Alle Daten aus „{name}“ löschen (TRUNCATE)?"),
            conn: ci,
            db: Some(db.into()),
            stmts: vec![format!("TRUNCATE TABLE {}", qi(name))],
            refresh_db: Some(db.into()),
            refresh_dbs: false,
        }));
        ui.close();
    }
    if ui.add(icons::button(Icon::Delete, "Löschen …")).clicked() {
        acts.push(Act::Confirm(drop_confirm(ci, db, cat, name, kind)));
        ui.close();
    }
}

fn drop_confirm(ci: usize, db: &str, cat: Cat, name: &str, kind: &str) -> Confirm {
    let what = match cat {
        Cat::Tables => "TABLE",
        Cat::Views => "VIEW",
        Cat::Routines => {
            if kind == "PROCEDURE" {
                "PROCEDURE"
            } else {
                "FUNCTION"
            }
        }
        Cat::Events => "EVENT",
        Cat::Queries => unreachable!("Abfragen werden nicht per DROP gelöscht"),
    };
    Confirm {
        title: format!("{} löschen", cat.label()),
        text: format!("„{name}“ unwiderruflich löschen?"),
        conn: ci,
        db: Some(db.into()),
        stmts: vec![format!("DROP {what} {}", qi(name))],
        refresh_db: Some(db.into()),
        refresh_dbs: false,
    }
}

impl NavicatDlg {
    fn new(found: Vec<NavConn>) -> Self {
        let sel = vec![true; found.len()];
        Self { found, sel, report: None }
    }
}

impl ConnDialog {
    fn new(idx: Option<usize>, cfg: ConnConfig) -> Self {
        Self { idx, cfg, test: None, test_tag: 0, test_result: None }
    }
}

pub fn fmt_size(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

impl eframe::App for App {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        (self.settings.open_tabs, self.settings.active_tab) = self.view_state();
        self.save_settings();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Schriftgrößen an die Pixeldichte des aktuellen Monitors anpassen (ändert sich beim Verschieben)
        let ppp = ctx.pixels_per_point();
        if (ppp - self.last_ppp).abs() > 0.001 {
            self.last_ppp = ppp;
            crate::snap_text_sizes(&ctx, self.settings.sharp_text);
        }
        self.poll_conns(&ctx);

        // Globale Tastenkürzel
        let (new_q, close_t) = ctx.input(|i| (i.modifiers.command && i.key_pressed(Key::T), i.modifiers.command && i.key_pressed(Key::W)));
        if new_q {
            if let Some(ci) = self.sel.conn {
                self.acts.push(Act::NewQuery(ci, self.sel.db.clone(), String::new()));
            }
        }
        if close_t && self.active > 0 {
            let a = self.active;
            self.close_tab(a);
        }

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(2.0);
            self.toolbar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("statusbar").show(ui, |ui| self.statusbar(ui));
        egui::Panel::left("tree").resizable(true).default_size(250.0).min_size(140.0).show(ui, |ui| self.tree(ui));
        // Unten kein Innenabstand: Fußzeilen der Tabs schließen direkt an die Statusleiste an
        let central = egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin { left: 8, right: 8, top: 8, bottom: 0 });
        egui::CentralPanel::default().frame(central).show(ui, |ui| {
            self.tab_bar(ui);
            ui.separator();
            if self.active == 0 {
                self.objects_view(ui);
            } else {
                let idx = self.active - 1;
                let uid = self.tabs[idx].conn_uid;
                let dbs: Vec<String> = self.conns.iter().find(|c| c.uid == uid).map(|c| c.dbs.iter().map(|d| d.name.clone()).collect()).unwrap_or_default();
                let mut refresh = None;
                let mut saved_query: Option<String> = None;
                match &mut self.tabs[idx].kind {
                    TabKind::Data(d) => d.ui(ui),
                    TabKind::Query(q) => {
                        q.ui(ui, &dbs);
                        if let Some(db) = q.take_saved() {
                            saved_query = Some(db);
                        }
                    }
                    TabKind::Users(u) => u.ui(ui),
                    TabKind::Monitor(m) => m.ui(ui),
                    TabKind::Sync(s) => {
                        s.ui(ui);
                        if let Some((uid, db, sql)) = s.take_open_query() {
                            if let Some(ci) = self.conns.iter().position(|c| c.uid == uid) {
                                self.acts.push(Act::NewQuery(ci, Some(db), sql));
                            }
                        }
                    }
                    TabKind::Design(d) => {
                        d.ui(ui);
                        if d.take_saved() {
                            refresh = Some(d.db.clone());
                        }
                    }
                }
                if let (Some(db), Some(ci)) = (refresh, self.conns.iter().position(|c| c.uid == uid)) {
                    self.acts.push(Act::RefreshDb(ci, db));
                }
                if let (Some(db), Some(ci)) = (saved_query, self.conns.iter().position(|c| c.uid == uid)) {
                    self.refresh_saved(ci, &db);
                }
            }
        });

        self.dialogs(&ctx);
        self.apply_acts(&ctx);
        if crate::export::bom() != self.settings.csv_bom {
            self.settings.csv_bom = crate::export::bom();
            self.save_settings();
        }
    }
}

