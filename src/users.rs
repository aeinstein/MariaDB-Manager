//! Benutzer-Manager: Benutzer anlegen/ändern/löschen und Rechte auf Server-,
//! Datenbank- und Tabellenebene setzen (GRANT/REVOKE werden als Diff erzeugt).

use crate::icons::{self, Icon};
use crate::db::{qi, qs, ConnConfig, Mode, Stmt, Worker};
use eframe::egui::{self, RichText};
use std::collections::{BTreeMap, BTreeSet, HashMap};

const DB_PRIVS: &[&str] = &[
    "SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER", "INDEX", "REFERENCES",
    "CREATE TEMPORARY TABLES", "LOCK TABLES", "CREATE VIEW", "SHOW VIEW", "CREATE ROUTINE", "ALTER ROUTINE",
    "EXECUTE", "EVENT", "TRIGGER", "DELETE HISTORY",
];
const TABLE_PRIVS: &[&str] = &[
    "SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER", "INDEX", "REFERENCES", "CREATE VIEW",
    "SHOW VIEW", "TRIGGER", "DELETE HISTORY",
];
const GLOBAL_FALLBACK: &[&str] = &[
    "SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "RELOAD", "SHUTDOWN", "PROCESS", "FILE",
    "REFERENCES", "INDEX", "ALTER", "SHOW DATABASES", "SUPER", "CREATE TEMPORARY TABLES", "LOCK TABLES",
    "EXECUTE", "REPLICATION SLAVE", "BINLOG MONITOR", "CREATE VIEW", "SHOW VIEW", "CREATE ROUTINE",
    "ALTER ROUTINE", "CREATE USER", "EVENT", "TRIGGER", "CREATE TABLESPACE", "DELETE HISTORY",
];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Scope {
    Global,
    Db(String),
    Table(String, String),
}

impl Scope {
    fn sql(&self) -> String {
        match self {
            Scope::Global => "*.*".into(),
            Scope::Db(d) => format!("{}.*", qi(d)),
            Scope::Table(d, t) => format!("{}.{}", qi(d), qi(t)),
        }
    }
    fn label(&self) -> String {
        match self {
            Scope::Global => "Server (global)".into(),
            Scope::Db(d) => format!("{d}.*"),
            Scope::Table(d, t) => format!("{d}.{t}"),
        }
    }
    fn avail<'a>(&self, global: &'a [String]) -> Vec<&'a str>
    where
        'static: 'a,
    {
        match self {
            Scope::Global => global.iter().map(|s| s.as_str()).collect(),
            Scope::Db(_) => DB_PRIVS.to_vec(),
            Scope::Table(..) => TABLE_PRIVS.to_vec(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct ScopePrivs {
    privs: BTreeSet<String>,
    grant: bool,
}

type Privs = BTreeMap<Scope, ScopePrivs>;

#[derive(Clone, Copy, PartialEq)]
enum Page {
    General,
    Server,
    Objects,
    Sql,
}

struct UserEdit {
    is_new: bool,
    orig_user: String,
    orig_host: String,
    user: String,
    host: String,
    password: String,
    password2: String,
    change_pw: bool,
    orig: Privs,
    cur: Privs,
    loaded: bool,
    sel_scope: Option<Scope>,
}

impl UserEdit {
    fn new_user() -> Self {
        let mut cur = Privs::new();
        cur.insert(Scope::Global, ScopePrivs::default());
        Self {
            is_new: true,
            orig_user: String::new(),
            orig_host: String::new(),
            user: String::new(),
            host: "%".into(),
            password: String::new(),
            password2: String::new(),
            change_pw: true,
            orig: Privs::new(),
            cur,
            loaded: true,
            sel_scope: None,
        }
    }
}

pub struct UsersTab {
    worker: Worker,
    users: Vec<(String, String)>,
    sel: Option<usize>,
    filter: String,
    tag_list: u64,
    tag_privs: u64,
    tag_apply: u64,
    tag_tables: HashMap<u64, String>,
    global_privs: Vec<String>,
    dbs: Vec<String>,
    tables: HashMap<String, Vec<String>>,
    edit: Option<UserEdit>,
    page: Page,
    new_db: String,
    new_table: String,
    status: String,
    error: Option<String>,
    confirm_drop: bool,
    busy: bool,
}

fn account(u: &str, h: &str) -> String {
    format!("{}@{}", qs(u), qs(h))
}

impl UsersTab {
    pub fn new(cfg: &ConnConfig, ctx: &egui::Context) -> Self {
        let mut t = Self {
            worker: Worker::spawn(cfg.clone(), ctx.clone()),
            users: Vec::new(),
            sel: None,
            filter: String::new(),
            tag_list: 0,
            tag_privs: 0,
            tag_apply: 0,
            tag_tables: HashMap::new(),
            global_privs: GLOBAL_FALLBACK.iter().map(|s| s.to_string()).collect(),
            dbs: Vec::new(),
            tables: HashMap::new(),
            edit: None,
            page: Page::General,
            new_db: String::new(),
            new_table: String::new(),
            status: String::new(),
            error: None,
            confirm_drop: false,
            busy: false,
        };
        t.reload();
        t
    }

    fn reload(&mut self) {
        self.tag_list = self.worker.send(
            None,
            vec![
                Stmt::plain("SELECT User, Host FROM mysql.user WHERE is_role <> 'Y' ORDER BY User, Host"),
                Stmt::plain("SHOW PRIVILEGES"),
                Stmt::plain("SHOW DATABASES"),
            ],
            Mode::Continue,
            usize::MAX,
        );
        self.busy = true;
    }

    fn load_privs(&mut self, u: &str, h: &str) {
        let grantee = format!("'{u}'@'{h}'");
        let g = || vec![Some(grantee.clone())];
        self.tag_privs = self.worker.send(
            None,
            vec![
                Stmt::with("SELECT PRIVILEGE_TYPE, IS_GRANTABLE FROM information_schema.USER_PRIVILEGES WHERE GRANTEE = ?", g()),
                Stmt::with("SELECT TABLE_SCHEMA, PRIVILEGE_TYPE, IS_GRANTABLE FROM information_schema.SCHEMA_PRIVILEGES WHERE GRANTEE = ?", g()),
                Stmt::with("SELECT TABLE_SCHEMA, TABLE_NAME, PRIVILEGE_TYPE, IS_GRANTABLE FROM information_schema.TABLE_PRIVILEGES WHERE GRANTEE = ?", g()),
            ],
            Mode::Continue,
            usize::MAX,
        );
        self.busy = true;
    }

    fn load_tables(&mut self, db: &str) {
        if self.tables.contains_key(db) || self.tag_tables.values().any(|d| d == db) {
            return;
        }
        let tag = self.worker.send(
            None,
            vec![Stmt::with(
                "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME",
                vec![Some(db.to_string())],
            )],
            Mode::StopOnError,
            usize::MAX,
        );
        self.tag_tables.insert(tag, db.to_string());
    }

    fn select_user(&mut self, i: usize) {
        let (u, h) = self.users[i].clone();
        self.sel = Some(i);
        self.edit = Some(UserEdit {
            is_new: false,
            orig_user: u.clone(),
            orig_host: h.clone(),
            user: u.clone(),
            host: h.clone(),
            password: String::new(),
            password2: String::new(),
            change_pw: false,
            orig: Privs::new(),
            cur: Privs::new(),
            loaded: false,
            sel_scope: None,
        });
        self.load_privs(&u, &h);
    }

    fn poll(&mut self) {
        while let Some(mut res) = self.worker.poll() {
            if res.tag == self.tag_list {
                self.busy = false;
                if let Some(e) = res.outs.first().and_then(|o| o.error.clone()) {
                    self.error = Some(e);
                }
                if let Some(rs) = res.take_set(0) {
                    self.users = rs
                        .rows
                        .into_iter()
                        .map(|r| (r[0].clone().unwrap_or_default(), r[1].clone().unwrap_or_default()))
                        .collect();
                }
                if let Some(rs) = res.take_set(1) {
                    let skip = ["USAGE", "GRANT OPTION", "PROXY"];
                    let v: Vec<String> = rs
                        .rows
                        .iter()
                        .filter_map(|r| r[0].as_ref().map(|s| s.to_ascii_uppercase()))
                        .filter(|s| !skip.contains(&s.as_str()))
                        .collect();
                    if !v.is_empty() {
                        self.global_privs = v;
                    }
                }
                if let Some(rs) = res.take_set(2) {
                    self.dbs = rs.rows.into_iter().filter_map(|r| r[0].clone()).collect();
                }
                // Auswahl wiederherstellen
                if let Some(e) = &self.edit {
                    if !e.is_new {
                        let key = (e.user.clone(), e.host.clone());
                        match self.users.iter().position(|x| *x == key) {
                            Some(i) => self.select_user(i),
                            None => {
                                self.edit = None;
                                self.sel = None;
                            }
                        }
                    }
                }
            } else if res.tag == self.tag_privs {
                self.busy = false;
                let mut p = Privs::new();
                p.insert(Scope::Global, ScopePrivs::default());
                let add = |p: &mut Privs, s: Scope, priv_: &str, g: &str| {
                    let e = p.entry(s).or_default();
                    if priv_ != "USAGE" {
                        e.privs.insert(priv_.to_string());
                    }
                    if g == "YES" {
                        e.grant = true;
                    }
                };
                if let Some(rs) = res.set(0) {
                    for r in 0..rs.rows.len() {
                        add(&mut p, Scope::Global, rs.get(r, "PRIVILEGE_TYPE"), rs.get(r, "IS_GRANTABLE"));
                    }
                }
                if let Some(rs) = res.set(1) {
                    for r in 0..rs.rows.len() {
                        add(&mut p, Scope::Db(rs.get(r, "TABLE_SCHEMA").into()), rs.get(r, "PRIVILEGE_TYPE"), rs.get(r, "IS_GRANTABLE"));
                    }
                }
                if let Some(rs) = res.set(2) {
                    for r in 0..rs.rows.len() {
                        add(
                            &mut p,
                            Scope::Table(rs.get(r, "TABLE_SCHEMA").into(), rs.get(r, "TABLE_NAME").into()),
                            rs.get(r, "PRIVILEGE_TYPE"),
                            rs.get(r, "IS_GRANTABLE"),
                        );
                    }
                }
                if let Some(e) = self.edit.as_mut() {
                    e.orig = p.clone();
                    e.cur = p;
                    e.loaded = true;
                    e.sel_scope = e.cur.keys().find(|s| **s != Scope::Global).cloned();
                }
                if let Some(err) = res.outs.iter().find_map(|o| o.error.clone()) {
                    self.error = Some(err);
                }
            } else if res.tag == self.tag_apply {
                self.busy = false;
                if let Some(e) = res.error {
                    self.error = Some(e);
                } else {
                    self.status = format!("{} Anweisung(en) ausgeführt.", res.outs.len());
                    if let Some(e) = self.edit.as_mut() {
                        e.is_new = false;
                        e.change_pw = false;
                        e.password.clear();
                        e.password2.clear();
                    }
                }
                self.reload();
            } else if let Some(db) = self.tag_tables.remove(&res.tag) {
                let v = res.set(0).map(|rs| rs.rows.iter().filter_map(|r| r[0].clone()).collect()).unwrap_or_default();
                self.tables.insert(db, v);
            }
        }
    }

    /// Erzeugt alle SQL-Anweisungen für die aktuellen Änderungen.
    fn build_sql(e: &UserEdit) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        if e.user.trim().is_empty() && e.is_new {
            return Err("Benutzername fehlt.".into());
        }
        if e.host.trim().is_empty() {
            return Err("Host fehlt (z. B. % oder localhost).".into());
        }
        if e.change_pw && e.password != e.password2 {
            return Err("Die Passwörter stimmen nicht überein.".into());
        }
        let acc = account(&e.user, &e.host);
        if e.is_new {
            let pw = if e.change_pw { format!(" IDENTIFIED BY {}", qs(&e.password)) } else { String::new() };
            out.push(format!("CREATE USER {acc}{pw}"));
        } else {
            if e.user != e.orig_user || e.host != e.orig_host {
                out.push(format!("RENAME USER {} TO {acc}", account(&e.orig_user, &e.orig_host)));
            }
            if e.change_pw {
                out.push(format!("ALTER USER {acc} IDENTIFIED BY {}", qs(&e.password)));
            }
        }
        let empty = ScopePrivs::default();
        let scopes: BTreeSet<&Scope> = e.orig.keys().chain(e.cur.keys()).collect();
        for s in scopes {
            let o = e.orig.get(s).unwrap_or(&empty);
            let n = e.cur.get(s).unwrap_or(&empty);
            let removed: Vec<&str> = o.privs.difference(&n.privs).map(|x| x.as_str()).collect();
            let added: Vec<&str> = n.privs.difference(&o.privs).map(|x| x.as_str()).collect();
            if !removed.is_empty() {
                out.push(format!("REVOKE {} ON {} FROM {acc}", removed.join(", "), s.sql()));
            }
            if o.grant && !n.grant {
                out.push(format!("REVOKE GRANT OPTION ON {} FROM {acc}", s.sql()));
            }
            if !added.is_empty() {
                let wg = if n.grant && !o.grant { " WITH GRANT OPTION" } else { "" };
                out.push(format!("GRANT {} ON {} TO {acc}{wg}", added.join(", "), s.sql()));
            } else if n.grant && !o.grant {
                out.push(format!("GRANT GRANT OPTION ON {} TO {acc}", s.sql()));
            }
        }
        Ok(out)
    }

    fn apply(&mut self) {
        let Some(e) = &self.edit else { return };
        match Self::build_sql(e) {
            Err(m) => self.error = Some(m),
            Ok(v) if v.is_empty() => self.status = "Keine Änderungen.".into(),
            Ok(v) => {
                self.tag_apply = self.worker.send(None, v.into_iter().map(Stmt::plain).collect(), Mode::StopOnError, 0);
                self.busy = true;
                self.error = None;
            }
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.poll();

        ui.horizontal(|ui| {
            if ui.add(icons::button(Icon::NewUser, "Neuer Benutzer")).clicked() {
                self.edit = Some(UserEdit::new_user());
                self.sel = None;
                self.page = Page::General;
            }
            let has_sel = self.edit.as_ref().is_some_and(|e| !e.is_new);
            if ui.add_enabled(has_sel, icons::button(Icon::Delete, "Benutzer löschen")).clicked() {
                self.confirm_drop = true;
            }
            let dirty = self.edit.as_ref().is_some_and(|e| e.is_new || e.change_pw || e.orig != e.cur || e.user != e.orig_user || e.host != e.orig_host);
            if ui.add_enabled(dirty && !self.busy, egui::Button::new("💾 Speichern")).clicked() {
                self.apply();
            }
            if ui.add_enabled(!self.busy, icons::button(Icon::Refresh, "Aktualisieren")).clicked() {
                self.reload();
            }
            if self.busy {
                ui.spinner();
            }
            ui.separator();
            ui.weak(&self.status);
        });
        if let Some(err) = self.error.clone() {
            ui.horizontal(|ui| {
                ui.colored_label(ui.visuals().error_fg_color, RichText::new(err).monospace());
                if ui.small_button("✖").clicked() {
                    self.error = None;
                }
            });
        }
        ui.separator();

        egui::Panel::left(egui::Id::unique("users_list"))
            .resizable(true)
            .default_size(240.0)
            .show(ui, |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("🔍 Filter").desired_width(f32::INFINITY));
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    let f = self.filter.to_lowercase();
                    let mut clicked = None;
                    for (i, (u, h)) in self.users.iter().enumerate() {
                        let label = format!("{}@{}", if u.is_empty() { "(anonym)" } else { u }, h);
                        if !f.is_empty() && !label.to_lowercase().contains(&f) {
                            continue;
                        }
                        if ui.add(egui::Button::selectable(self.sel == Some(i), (icons::atom(Icon::User, 16.0), label.as_str())).frame_when_inactive(false)).clicked() {
                            clicked = Some(i);
                        }
                    }
                    if let Some(i) = clicked {
                        self.select_user(i);
                    }
                });
            });

        egui::CentralPanel::default().show(ui, |ui| {
            if self.edit.is_none() {
                ui.centered_and_justified(|ui| ui.weak("Benutzer links auswählen oder „Neuer Benutzer“ anlegen."));
                return;
            }
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.page, Page::General, "Allgemein");
                ui.selectable_value(&mut self.page, Page::Server, "Serverrechte");
                ui.selectable_value(&mut self.page, Page::Objects, "Datenbank- & Tabellenrechte");
                ui.selectable_value(&mut self.page, Page::Sql, "SQL-Vorschau");
            });
            ui.separator();
            let page = self.page;
            match page {
                Page::General => self.ui_general(ui),
                Page::Server => self.ui_server(ui),
                Page::Objects => self.ui_objects(ui),
                Page::Sql => {
                    let e = self.edit.as_ref().unwrap();
                    let text = match Self::build_sql(e) {
                        Ok(v) if v.is_empty() => "-- Keine Änderungen".to_string(),
                        Ok(v) => v.join(";\n") + ";",
                        Err(m) => format!("-- {m}"),
                    };
                    egui::ScrollArea::both().show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(text).monospace()).selectable(true));
                    });
                }
            }
        });

        if self.confirm_drop {
            let mut choice = 0;
            let acc = self.edit.as_ref().map(|e| format!("{}@{}", e.orig_user, e.orig_host)).unwrap_or_default();
            egui::Modal::new(egui::Id::unique("drop_user")).show(ui.ctx(), |ui| {
                ui.heading("Benutzer löschen");
                ui.label(format!("Soll der Benutzer „{acc}“ wirklich gelöscht werden?"));
                ui.horizontal(|ui| {
                    if ui.button("Löschen").clicked() {
                        choice = 1;
                    }
                    if ui.button("Abbrechen").clicked() {
                        choice = 2;
                    }
                });
            });
            if choice == 1 {
                if let Some(e) = self.edit.take() {
                    let sql = format!("DROP USER {}", account(&e.orig_user, &e.orig_host));
                    self.tag_apply = self.worker.send(None, vec![Stmt::plain(sql)], Mode::StopOnError, 0);
                    self.busy = true;
                    self.sel = None;
                }
            }
            if choice != 0 {
                self.confirm_drop = false;
            }
        }
    }

    fn ui_general(&mut self, ui: &mut egui::Ui) {
        let e = self.edit.as_mut().unwrap();
        egui::Grid::new("user_general").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Benutzername:");
            ui.add(egui::TextEdit::singleline(&mut e.user).desired_width(260.0));
            ui.end_row();
            ui.label("Host:");
            ui.add(egui::TextEdit::singleline(&mut e.host).desired_width(260.0).hint_text("% = beliebig"));
            ui.end_row();
            ui.label("");
            ui.checkbox(&mut e.change_pw, if e.is_new { "Passwort setzen" } else { "Passwort ändern" });
            ui.end_row();
            ui.add_enabled_ui(e.change_pw, |ui| ui.label("Passwort:"));
            ui.add_enabled(e.change_pw, egui::TextEdit::singleline(&mut e.password).password(true).desired_width(260.0));
            ui.end_row();
            ui.add_enabled_ui(e.change_pw, |ui| ui.label("Wiederholen:"));
            ui.add_enabled(e.change_pw, egui::TextEdit::singleline(&mut e.password2).password(true).desired_width(260.0));
            ui.end_row();
        });
        if e.change_pw && !e.password2.is_empty() && e.password != e.password2 {
            ui.colored_label(ui.visuals().warn_fg_color, "Passwörter stimmen nicht überein.");
        }
        if !e.loaded {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Rechte werden geladen …");
            });
        }
    }

    fn priv_checkboxes(ui: &mut egui::Ui, sp: &mut ScopePrivs, avail: &[&str], id: &str) {
        ui.horizontal(|ui| {
            if ui.small_button("Alle").clicked() {
                sp.privs = avail.iter().map(|s| s.to_string()).collect();
            }
            if ui.small_button("Keine").clicked() {
                sp.privs.clear();
                sp.grant = false;
            }
            ui.separator();
            ui.checkbox(&mut sp.grant, "GRANT OPTION (darf Rechte weitergeben)");
        });
        ui.add_space(4.0);
        let cols = ((ui.available_width() / 190.0).floor() as usize).max(1);
        egui::Grid::new(id).num_columns(cols).spacing([16.0, 4.0]).show(ui, |ui| {
            for (i, p) in avail.iter().enumerate() {
                let mut on = sp.privs.contains(*p);
                if ui.checkbox(&mut on, *p).changed() {
                    if on {
                        sp.privs.insert(p.to_string());
                    } else {
                        sp.privs.remove(*p);
                    }
                }
                if (i + 1) % cols == 0 {
                    ui.end_row();
                }
            }
        });
    }

    fn ui_server(&mut self, ui: &mut egui::Ui) {
        let global = self.global_privs.clone();
        let e = self.edit.as_mut().unwrap();
        if !e.loaded {
            ui.spinner();
            return;
        }
        ui.label("Globale Rechte gelten für alle Datenbanken und Tabellen (ON *.*).");
        ui.add_space(4.0);
        let avail = Scope::Global.avail(&global);
        let sp = e.cur.entry(Scope::Global).or_default();
        egui::ScrollArea::vertical().show(ui, |ui| Self::priv_checkboxes(ui, sp, &avail, "global_privs"));
    }

    fn ui_objects(&mut self, ui: &mut egui::Ui) {
        if !self.edit.as_ref().unwrap().loaded {
            ui.spinner();
            return;
        }
        // Hinzufügen-Zeile
        let mut add: Option<Scope> = None;
        let mut want_tables: Option<String> = None;
        ui.horizontal(|ui| {
            ui.label("Datenbank:");
            egui::ComboBox::from_id_salt("priv_db")
                .selected_text(if self.new_db.is_empty() { "(wählen)" } else { self.new_db.as_str() })
                .width(180.0)
                .show_ui(ui, |ui| {
                    for d in &self.dbs {
                        if ui.selectable_label(self.new_db == *d, d.as_str()).clicked() {
                            self.new_db = d.clone();
                            self.new_table.clear();
                        }
                    }
                });
            if !self.new_db.is_empty() {
                want_tables = Some(self.new_db.clone());
            }
            ui.label("Tabelle:");
            let tables = self.tables.get(&self.new_db).cloned().unwrap_or_default();
            egui::ComboBox::from_id_salt("priv_tbl")
                .selected_text(if self.new_table.is_empty() { "(alle Tabellen)" } else { self.new_table.as_str() })
                .width(200.0)
                .show_ui(ui, |ui| {
                    if ui.selectable_label(self.new_table.is_empty(), "(alle Tabellen)").clicked() {
                        self.new_table.clear();
                    }
                    for t in &tables {
                        if ui.selectable_label(self.new_table == *t, t.as_str()).clicked() {
                            self.new_table = t.clone();
                        }
                    }
                });
            if ui.add_enabled(!self.new_db.is_empty(), icons::button(Icon::Add, "Hinzufügen")).clicked() {
                add = Some(if self.new_table.is_empty() {
                    Scope::Db(self.new_db.clone())
                } else {
                    Scope::Table(self.new_db.clone(), self.new_table.clone())
                });
            }
        });
        if let Some(db) = want_tables {
            self.load_tables(&db);
        }
        let e = self.edit.as_mut().unwrap();
        if let Some(s) = add {
            e.cur.entry(s.clone()).or_default();
            e.sel_scope = Some(s);
        }
        ui.separator();

        let mut remove: Option<Scope> = None;
        egui::Panel::left(egui::Id::unique("scope_list")).resizable(true).default_size(220.0).show(ui, |ui| {
            ui.strong("Rechte-Bereiche");
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let scopes: Vec<Scope> = e.cur.keys().filter(|s| **s != Scope::Global).cloned().collect();
                if scopes.is_empty() {
                    ui.weak("Keine Datenbank- oder Tabellenrechte.");
                }
                for s in scopes {
                    let icon = if matches!(s, Scope::Db(_)) { Icon::Database } else { Icon::Table };
                    let n = e.cur.get(&s).map(|p| p.privs.len()).unwrap_or(0);
                    let r = ui.add(egui::Button::selectable(e.sel_scope.as_ref() == Some(&s), (icons::atom(icon, 16.0), format!("{}  ({n})", s.label()))).frame_when_inactive(false));
                    if r.clicked() {
                        e.sel_scope = Some(s.clone());
                    }
                    r.context_menu(|ui| {
                        if ui.add(icons::button(Icon::Delete, "Bereich entfernen (alle Rechte entziehen)")).clicked() {
                            remove = Some(s.clone());
                            ui.close();
                        }
                    });
                }
            });
        });
        if let Some(s) = remove.take() {
            e.cur.remove(&s);
            if e.sel_scope.as_ref() == Some(&s) {
                e.sel_scope = None;
            }
        }
        egui::CentralPanel::default().show(ui, |ui| {
            let Some(s) = e.sel_scope.clone() else {
                ui.weak("Bereich auswählen oder oben hinzufügen.");
                return;
            };
            ui.horizontal(|ui| {
                ui.heading(s.label());
                if ui.add(icons::button(Icon::Delete, "Entfernen")).clicked() {
                    remove = Some(s.clone());
                }
            });
            let avail = s.avail(&[]);
            if let Some(sp) = e.cur.get_mut(&s) {
                // Rechte, die auf dem Server existieren, aber nicht in unserer Liste stehen, trotzdem anzeigen
                let mut av: Vec<&str> = avail.clone();
                let extra: Vec<String> = sp.privs.iter().filter(|p| !avail.contains(&p.as_str())).cloned().collect();
                let extra_ref: Vec<&str> = extra.iter().map(|s| s.as_str()).collect();
                av.extend(extra_ref);
                egui::ScrollArea::vertical().show(ui, |ui| Self::priv_checkboxes(ui, sp, &av, "scope_privs"));
            }
        });
        if let Some(s) = remove.take() {
            e.cur.remove(&s);
            e.sel_scope = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::itest::{cfg, wait};

    #[test]
    fn grant_revoke_diff() {
        let mut e = UserEdit::new_user();
        e.user = "mdbm_u".into();
        e.host = "localhost".into();
        e.password = "geheim'1".into();
        e.password2 = e.password.clone();
        e.cur.get_mut(&Scope::Global).unwrap().privs.insert("PROCESS".into());
        let db = e.cur.entry(Scope::Db("shop".into())).or_default();
        db.privs.extend(["SELECT".to_string(), "INSERT".to_string()]);
        let t = e.cur.entry(Scope::Table("shop".into(), "kunden".into())).or_default();
        t.privs.insert("UPDATE".into());
        t.grant = true;
        let sql = UsersTab::build_sql(&e).unwrap();
        assert_eq!(sql[0], "CREATE USER 'mdbm_u'@'localhost' IDENTIFIED BY 'geheim''1'");
        assert!(sql.contains(&"GRANT PROCESS ON *.* TO 'mdbm_u'@'localhost'".to_string()));
        assert!(sql.contains(&"GRANT INSERT, SELECT ON `shop`.* TO 'mdbm_u'@'localhost'".to_string()));
        assert!(sql.contains(&"GRANT UPDATE ON `shop`.`kunden` TO 'mdbm_u'@'localhost' WITH GRANT OPTION".to_string()));

        // Gegen echten Server ausführen und zurücklesen
        let Some(cfg) = cfg() else { return };
        let mut w = Worker::spawn(cfg, eframe::egui::Context::default());
        w.query(None, &["DROP USER IF EXISTS 'mdbm_u'@'localhost'"]);
        wait(&mut w);
        w.send(None, sql.into_iter().map(Stmt::plain).collect(), Mode::StopOnError, 0);
        let r = wait(&mut w);
        assert!(r.error.is_none(), "{:?}", r.error);

        // Zweiter Schritt: Rechte entziehen, Grant-Option weg, Tabellenbereich entfernen
        e.is_new = false;
        e.orig_user = e.user.clone();
        e.orig_host = e.host.clone();
        e.change_pw = false;
        e.orig = e.cur.clone();
        e.cur.get_mut(&Scope::Db("shop".into())).unwrap().privs.remove("INSERT");
        e.cur.remove(&Scope::Table("shop".into(), "kunden".into()));
        let sql = UsersTab::build_sql(&e).unwrap();
        w.send(None, sql.into_iter().map(Stmt::plain).collect(), Mode::StopOnError, 0);
        let r = wait(&mut w);
        assert!(r.error.is_none(), "{:?}", r.error);
        w.query(None, &["SHOW GRANTS FOR 'mdbm_u'@'localhost'"]);
        let r = wait(&mut w);
        let grants: Vec<String> = r.set(0).unwrap().rows.iter().filter_map(|x| x[0].clone()).collect();
        let all = grants.join("\n");
        assert!(all.contains("GRANT PROCESS ON *.*"), "{all}");
        assert!(all.contains("GRANT SELECT ON `shop`.*"), "{all}");
        assert!(!all.contains("kunden"), "{all}");
        w.query(None, &["DROP USER 'mdbm_u'@'localhost'"]);
        wait(&mut w);
    }
}
