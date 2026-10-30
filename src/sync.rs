//! Strukturabgleich zwischen zwei Datenbanken (auch über verschiedene Verbindungen):
//! Tabellen vergleichen, auswählen und das Skript (`ALTER`/`CREATE`/`DROP TABLE`) erzeugen,
//! das das Ziel an die Quelle angleicht. Ausgeführt wird nichts – das Skript lässt sich
//! kopieren oder im SQL-Editor des Ziels öffnen.

use crate::db::{ConnConfig, Mode, Worker};
use crate::designtab::{self, SchemaInfo};
use crate::icons::{self, Icon};
use crate::sqlhl;
use eframe::egui::{self, Color32, RichText, Sense};
use egui_extras::{Column, TableBuilder};
use std::sync::atomic::{AtomicU64, Ordering};

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

    fn load(&mut self) {
        self.schema = None;
        self.error = None;
        if let Some(w) = &mut self.worker {
            self.tag_schema = w.send(None, designtab::schema_stmts(&self.db), Mode::StopOnError, usize::MAX);
        }
    }

    fn busy(&self) -> bool {
        self.tag_dbs != 0 || self.tag_schema != 0
    }

    fn poll(&mut self) -> bool {
        let mut done = false;
        let Some(w) = &mut self.worker else { return false };
        while let Some(res) = w.poll() {
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

struct Entry {
    name: String,
    status: Status,
    /// Anweisungen bzw. Fehlermeldung.
    sql: Result<Vec<String>, String>,
    checked: bool,
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
        let mut names: Vec<&String> = s.tables.keys().chain(t.tables.keys()).collect();
        names.sort_by_key(|n| n.to_lowercase());
        names.dedup();
        let tdb = self.tgt.db.clone();
        self.entries = names
            .into_iter()
            .map(|n| {
                let (a, b) = (s.tables.get(n), t.tables.get(n));
                let sql = designtab::sync_sql(a, b, n, &tdb, &|c: &str| t.charset_of(c).or_else(|| s.charset_of(c)));
                let status = match (&sql, a, b) {
                    (Err(_), _, _) => Status::Error,
                    (_, Some(_), None) => Status::OnlySrc,
                    (_, None, Some(_)) => Status::OnlyTgt,
                    (Ok(v), _, _) if v.is_empty() => Status::Equal,
                    _ => Status::Diff,
                };
                Entry { name: n.clone(), status, sql, checked: matches!(status, Status::Diff | Status::OnlySrc) }
            })
            .collect();
        self.compared = Some((self.side_label(&self.src), self.side_label(&self.tgt)));
    }

    /// Skript für die angehakten Tabellen (bzw. nur die fokussierte).
    fn script(&self) -> String {
        let mut out = String::new();
        let list: Vec<&Entry> = match self.focus {
            Some(i) => self.entries.get(i).into_iter().collect(),
            None => self.entries.iter().filter(|e| e.checked).collect(),
        };
        if let Some((s, t)) = &self.compared {
            out += &format!(
                "-- Strukturabgleich: Quelle {s} → Ziel {t}\n-- Erstellt: {} (UTC)\n\nSET FOREIGN_KEY_CHECKS = 0;\n\n",
                crate::queries::fmt_time(std::time::SystemTime::now())
            );
        }
        for e in list {
            out += &format!("-- {} ({})\n", e.name, e.status.label());
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

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let a = self.src.poll();
        let b = self.tgt.poll();
        if (a || b) && self.src.schema.is_some() && self.tgt.schema.is_some() {
            self.build();
        }

        self.ui_sides(ui);
        for e in [&self.src.error, &self.tgt.error].into_iter().flatten() {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        ui.separator();
        if self.entries.is_empty() {
            if self.src.tag_schema != 0 || self.tgt.tag_schema != 0 {
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
                            }
                        }
                    });
                });
                if side.busy() {
                    ui.spinner();
                } else {
                    ui.label("");
                }
                ui.end_row();
            }
        });
        ui.horizontal(|ui| {
            let ready = !self.src.db.is_empty() && !self.tgt.db.is_empty();
            let same = self.src.conn == self.tgt.conn && self.src.db == self.tgt.db;
            let busy = self.src.tag_schema != 0 || self.tgt.tag_schema != 0;
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
            if self.src.schema.is_some() && self.tgt.schema.is_some() {
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
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Tabellen filtern").desired_width(180.0));
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
                            ui.strong("Tabelle");
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
                                ui.add(egui::Label::new(&e.name).selectable(false).truncate());
                            });
                            let st = e.status;
                            let (_, r2) = row.col(|ui| {
                                ui.add(egui::Label::new(RichText::new(st.label()).color(st.color(ui))).selectable(false).truncate());
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
        ui.horizontal(|ui| {
            match self.focus.and_then(|i| self.entries.get(i)) {
                Some(e) => {
                    ui.strong(format!("Tabelle {}", e.name));
                    if ui.button("Gesamtes Skript anzeigen").clicked() {
                        self.focus = None;
                    }
                }
                None => {
                    ui.strong(format!("Skript für {n} Tabelle(n)"));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
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
        egui::ScrollArea::both().id_salt(("sync_sql", self.id)).auto_shrink([false, false]).show(ui, |ui| {
            let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, w: f32| sqlhl::layouter(ui, buf, w);
            let mut t = script.as_str();
            ui.add(egui::TextEdit::multiline(&mut t).code_editor().desired_width(f32::INFINITY).layouter(&mut layouter));
        });
    }

    fn set_all(&mut self, v: bool) {
        for e in &mut self.entries {
            if e.status != Status::Equal && e.status != Status::Error {
                e.checked = v;
            }
        }
    }
}
