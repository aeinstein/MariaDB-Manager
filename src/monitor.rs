//! Servermonitor im Stil von mytop: Kennzahlen aus `SHOW GLOBAL STATUS`, Prozessliste
//! (filtern, sortieren, Abfrage abbrechen, Verbindung beenden, EXPLAIN) sowie eigene
//! Reiter für Statuszähler und Servervariablen.

use crate::app::fmt_size;
use crate::db::{ConnConfig, Mode, ResultSet, Stmt, Worker};
use crate::icons::{self, Icon};
use crate::sqlhl;
use eframe::egui::{self, Color32, RichText, Sense};
use egui_extras::{Column, TableBuilder};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

const INTERVALS: &[(u64, &str)] = &[(0, "Aus"), (1, "1 s"), (2, "2 s"), (5, "5 s"), (10, "10 s"), (30, "30 s"), (60, "60 s")];
const HEADERS: &[&str] = &["Id", "Benutzer", "Host", "Datenbank", "Befehl", "Zeit (s)", "Status", "Fortschritt", "Abfrage"];
const COL_TIME: usize = 5;

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Processes,
    Status,
    Variables,
}

#[derive(Clone, Default)]
struct Proc {
    id: u64,
    user: String,
    host: String,
    db: String,
    command: String,
    time: i64,
    state: String,
    info: String,
    progress: f64,
}

impl Proc {
    fn idle(&self) -> bool {
        self.command.eq_ignore_ascii_case("Sleep")
    }

    fn cell(&self, c: usize) -> String {
        match c {
            0 => self.id.to_string(),
            1 => self.user.clone(),
            2 => self.host.clone(),
            3 => self.db.clone(),
            4 => self.command.clone(),
            5 => self.time.to_string(),
            6 => self.state.clone(),
            7 if self.progress > 0.0 => format!("{:.1} %", self.progress).replace('.', ","),
            7 => String::new(),
            // Abfrage einzeilig
            _ => self.info.split_whitespace().collect::<Vec<_>>().join(" "),
        }
    }

    fn cmp(&self, o: &Proc, c: usize) -> std::cmp::Ordering {
        match c {
            0 => self.id.cmp(&o.id),
            5 => self.time.cmp(&o.time),
            7 => self.progress.total_cmp(&o.progress),
            _ => self.cell(c).to_lowercase().cmp(&o.cell(c).to_lowercase()),
        }
    }
}

/// Rückfrage vor KILL.
struct Kill {
    ids: Vec<u64>,
    query_only: bool,
    text: String,
}

/// Aktionen aus Tabelle/Kontextmenü, nach dem Zeichnen ausgeführt.
enum MAct {
    Select(u64),
    Explain(u64),
    KillQuery(u64),
    Kill(u64),
    KillUser(String),
}

pub struct MonitorTab {
    id: u64,
    worker: Worker,
    page: Page,
    interval: u64,
    next: Option<Instant>,
    tag_refresh: u64,
    tag_action: u64,
    tag_explain: u64,
    want_vars: bool,
    procs: Vec<Proc>,
    proc_err: Option<String>,
    own_id: u64,
    version: String,
    status: Vec<(String, String)>,
    stat: HashMap<String, f64>,
    prev: Option<(Instant, HashMap<String, f64>)>,
    rate: HashMap<String, f64>,
    vars: Vec<(String, String)>,
    filter: String,
    hide_idle: bool,
    hide_own: bool,
    sort: (usize, bool),
    sel: Option<Proc>,
    explain: Option<Result<ResultSet, String>>,
    explain_fallback: Option<(String, String)>,
    status_filter: String,
    only_changed: bool,
    var_filter: String,
    kill: Option<Kill>,
    confirm_flush: bool,
    error: Option<String>,
    note: String,
}

impl MonitorTab {
    pub fn new(cfg: &ConnConfig, ctx: &egui::Context) -> Self {
        let mut t = Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            worker: Worker::spawn(cfg.clone(), ctx.clone()),
            page: Page::Processes,
            interval: 5,
            next: None,
            tag_refresh: 0,
            tag_action: 0,
            tag_explain: 0,
            want_vars: true,
            procs: Vec::new(),
            proc_err: None,
            own_id: 0,
            version: String::new(),
            status: Vec::new(),
            stat: HashMap::new(),
            prev: None,
            rate: HashMap::new(),
            vars: Vec::new(),
            filter: String::new(),
            hide_idle: true,
            hide_own: true,
            // wie mytop: längste Laufzeit zuerst
            sort: (COL_TIME, true),
            sel: None,
            explain: None,
            explain_fallback: None,
            status_filter: String::new(),
            only_changed: false,
            var_filter: String::new(),
            kill: None,
            confirm_flush: false,
            error: None,
            note: String::new(),
        };
        t.refresh();
        t
    }

    fn refresh(&mut self) {
        if self.tag_refresh != 0 {
            return;
        }
        let mut s = vec![
            Stmt::plain("SHOW FULL PROCESSLIST"),
            Stmt::plain("SHOW GLOBAL STATUS"),
            Stmt::plain("SELECT CONNECTION_ID(), VERSION()"),
        ];
        if self.want_vars {
            s.push(Stmt::plain("SHOW GLOBAL VARIABLES"));
            self.want_vars = false;
        }
        self.tag_refresh = self.worker.send(None, s, Mode::Continue, usize::MAX);
        if self.interval > 0 {
            self.next = Some(Instant::now() + Duration::from_secs(self.interval));
        }
    }

    fn poll(&mut self) {
        while let Some(mut res) = self.worker.poll() {
            let err = res.error.clone().or_else(|| res.outs.iter().find_map(|o| o.error.clone()));
            if res.tag == self.tag_refresh {
                self.tag_refresh = 0;
                self.error = res.error.clone();
                self.proc_err = res.outs.first().and_then(|o| o.error.clone());
                if let Some(rs) = res.take_set(0) {
                    self.procs = (0..rs.rows.len())
                        .map(|r| Proc {
                            id: rs.get(r, "Id").parse().unwrap_or(0),
                            user: rs.get(r, "User").into(),
                            host: rs.get(r, "Host").into(),
                            db: rs.get(r, "db").into(),
                            command: rs.get(r, "Command").into(),
                            time: rs.get(r, "Time").parse().unwrap_or(0),
                            state: rs.get(r, "State").into(),
                            info: rs.get(r, "Info").into(),
                            progress: rs.get(r, "Progress").parse().unwrap_or(0.0),
                        })
                        .collect();
                    // Auswahl mit aktuellen Werten auffrischen (bleibt stehen, wenn der Prozess weg ist)
                    if let Some(s) = &mut self.sel {
                        if let Some(p) = self.procs.iter().find(|p| p.id == s.id) {
                            *s = p.clone();
                        }
                    }
                }
                if let Some(rs) = res.take_set(1) {
                    let now = Instant::now();
                    let cur: HashMap<String, f64> =
                        rs.rows.iter().filter_map(|r| Some((r[0].clone()?, r[1].as_deref()?.parse().ok()?))).collect();
                    self.rate.clear();
                    if let Some((t, old)) = &self.prev {
                        let dt = now.duration_since(*t).as_secs_f64();
                        if dt > 0.1 {
                            for (k, v) in &cur {
                                if let Some(o) = old.get(k) {
                                    self.rate.insert(k.clone(), ((v - o) / dt).max(0.0));
                                }
                            }
                        }
                    }
                    self.prev = Some((now, cur.clone()));
                    self.stat = cur;
                    self.status = rs.rows.into_iter().map(|r| (r[0].clone().unwrap_or_default(), r[1].clone().unwrap_or_default())).collect();
                }
                if let Some(rs) = res.set(2) {
                    if let Some(r) = rs.rows.first() {
                        self.own_id = r[0].as_deref().and_then(|s| s.parse().ok()).unwrap_or(0);
                        self.version = r[1].clone().unwrap_or_default();
                    }
                }
                if let Some(rs) = res.take_set(3) {
                    self.vars = rs.rows.into_iter().map(|r| (r[0].clone().unwrap_or_default(), r[1].clone().unwrap_or_default())).collect();
                }
            } else if res.tag == self.tag_action {
                self.tag_action = 0;
                let errs: Vec<String> = res.outs.iter().filter_map(|o| o.error.clone()).chain(res.error.clone()).collect();
                if errs.is_empty() {
                    self.error = None;
                } else {
                    self.error = Some(errs.join("\n"));
                    self.note.clear();
                }
                self.refresh();
            } else if res.tag == self.tag_explain {
                self.tag_explain = 0;
                match (res.take_set(0), err) {
                    (Some(rs), None) => self.explain = Some(Ok(rs)),
                    (_, e) => {
                        // SHOW EXPLAIN geht nur, solange die Abfrage läuft – sonst EXPLAIN auf den Text
                        if let Some((db, sql)) = self.explain_fallback.take() {
                            let db = Some(db).filter(|d| !d.is_empty());
                            self.tag_explain = self.worker.send(db, vec![Stmt::plain(format!("EXPLAIN {sql}"))], Mode::StopOnError, 1000);
                        } else {
                            self.explain = Some(Err(e.unwrap_or_else(|| "Kein Ergebnis".into())));
                        }
                    }
                }
            }
        }
    }

    fn explain(&mut self, id: u64) {
        let Some(p) = self.procs.iter().find(|p| p.id == id).or(self.sel.as_ref().filter(|p| p.id == id)) else { return };
        let first = p.info.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
        let ok = matches!(first.trim_start_matches('('), "select" | "insert" | "update" | "delete" | "replace" | "with");
        self.explain_fallback = ok.then(|| (p.db.clone(), p.info.clone()));
        self.explain = None;
        self.tag_explain = self.worker.send(None, vec![Stmt::plain(format!("SHOW EXPLAIN FOR {id}"))], Mode::StopOnError, 1000);
    }

    fn proc_label(&self, id: u64) -> String {
        match self.procs.iter().find(|p| p.id == id) {
            Some(p) => format!("{id} ({}@{})", p.user, p.host),
            None => id.to_string(),
        }
    }

    fn apply(&mut self, a: MAct) {
        match a {
            MAct::Select(id) => {
                if self.sel.as_ref().map(|p| p.id) != Some(id) {
                    self.explain = None;
                }
                self.sel = self.procs.iter().find(|p| p.id == id).cloned();
            }
            MAct::Explain(id) => self.explain(id),
            MAct::KillQuery(id) => {
                self.kill = Some(Kill {
                    ids: vec![id],
                    query_only: true,
                    text: format!("Soll die laufende Abfrage von Prozess {} abgebrochen werden?\nDie Verbindung bleibt bestehen.", self.proc_label(id)),
                })
            }
            MAct::Kill(id) => {
                self.kill = Some(Kill {
                    ids: vec![id],
                    query_only: false,
                    text: format!("Soll die Verbindung {} wirklich beendet werden?", self.proc_label(id)),
                })
            }
            MAct::KillUser(u) => {
                let ids: Vec<u64> = self.procs.iter().filter(|p| p.user == u && p.id != self.own_id).map(|p| p.id).collect();
                if !ids.is_empty() {
                    self.kill = Some(Kill {
                        text: format!("Sollen alle {} Verbindungen von „{u}“ beendet werden?", ids.len()),
                        ids,
                        query_only: false,
                    })
                }
            }
        }
    }

    fn val(&self, k: &str) -> f64 {
        self.stat.get(k).copied().unwrap_or(0.0)
    }

    fn rate(&self, k: &str) -> Option<f64> {
        self.rate.get(k).copied()
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.poll();
        // Automatisch aktualisieren – nur solange der Tab sichtbar ist
        if self.interval > 0 {
            if self.tag_refresh == 0 && self.next.is_none_or(|t| Instant::now() >= t) {
                self.refresh();
            }
            if let Some(t) = self.next {
                ui.ctx().request_repaint_after(t.saturating_duration_since(Instant::now()));
            }
        }

        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.page, Page::Processes, "Prozesse");
            ui.selectable_value(&mut self.page, Page::Status, "Status");
            ui.selectable_value(&mut self.page, Page::Variables, "Servervariablen");
            ui.separator();
            if ui.add(icons::button(Icon::Refresh, "Aktualisieren")).clicked() {
                self.want_vars = self.page == Page::Variables;
                self.refresh();
            }
            ui.label("Automatisch:");
            let cur = INTERVALS.iter().find(|i| i.0 == self.interval).map_or("?", |i| i.1);
            egui::ComboBox::from_id_salt(("mon_iv", self.id)).selected_text(cur).width(60.0).show_ui(ui, |ui| {
                for (v, l) in INTERVALS {
                    if ui.selectable_value(&mut self.interval, *v, *l).changed() {
                        self.next = (*v > 0).then(|| Instant::now() + Duration::from_secs(*v));
                    }
                }
            });
            if self.tag_refresh != 0 || self.tag_action != 0 {
                ui.spinner();
            }
            if !self.note.is_empty() {
                ui.weak(&self.note);
            }
        });
        if let Some(e) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        ui.separator();

        if self.page != Page::Variables {
            self.summary(ui);
            ui.separator();
        }
        match self.page {
            Page::Processes => self.ui_procs(ui),
            Page::Status => self.ui_status(ui),
            Page::Variables => self.ui_vars(ui),
        }
        self.dialogs(ui);
    }

    /// Kopfbereich wie bei mytop.
    fn summary(&self, ui: &mut egui::Ui) {
        let up = self.val("Uptime").max(1.0);
        let q = self.val("Questions");
        let now = |k: &str| self.rate(k).map_or("–".to_string(), |r| fmt_dec(r));
        let pct = |a: f64, b: f64| if b > 0.0 { format!("{} %", fmt_dec(100.0 - a / b * 100.0)) } else { "–".into() };
        let com = |k: &[&str]| k.iter().map(|k| self.val(k)).sum::<f64>();
        let (se, ins, upd, del) = (
            com(&["Com_select"]),
            com(&["Com_insert", "Com_insert_select", "Com_replace", "Com_replace_select"]),
            com(&["Com_update", "Com_update_multi"]),
            com(&["Com_delete", "Com_delete_multi"]),
        );
        let sum = (se + ins + upd + del).max(1.0);
        let mix = format!("{:.0} / {:.0} / {:.0} / {:.0} %", se / sum * 100.0, ins / sum * 100.0, upd / sum * 100.0, del / sum * 100.0);
        let bytes = |k: &str| {
            let r = self.rate(k).map_or("–".to_string(), |r| format!("{}/s", fmt_size(r as u64)));
            format!("{r}  (gesamt {})", fmt_size(self.val(k) as u64))
        };
        let rows: [[(&str, String); 4]; 3] = [
            [
                ("Server:", self.version.clone()),
                ("Laufzeit:", fmt_dur(self.val("Uptime") as i64)),
                ("Abfragen:", fmt_num(q)),
                ("Abfragen/s:", format!("Ø {}  ·  aktuell {}", fmt_dec(q / up), now("Questions"))),
            ],
            [
                (
                    "Threads:",
                    format!(
                        "{} verbunden, {} aktiv, {} im Cache",
                        fmt_num(self.val("Threads_connected")),
                        fmt_num(self.val("Threads_running")),
                        fmt_num(self.val("Threads_cached"))
                    ),
                ),
                ("Verbindungen:", format!("{}  (abgebrochen {})", fmt_num(self.val("Connections")), fmt_num(self.val("Aborted_connects")))),
                ("Langsame Abfragen:", format!("{}  ({}/s)", fmt_num(self.val("Slow_queries")), now("Slow_queries"))),
                ("Sel/Ins/Upd/Del:", mix),
            ],
            [
                ("Key-Effizienz:", pct(self.val("Key_reads"), self.val("Key_read_requests"))),
                ("Buffer-Pool-Treffer:", pct(self.val("Innodb_buffer_pool_reads"), self.val("Innodb_buffer_pool_read_requests"))),
                ("Empfangen:", bytes("Bytes_received")),
                ("Gesendet:", bytes("Bytes_sent")),
            ],
        ];
        egui::Grid::new(("mon_sum", self.id)).num_columns(8).spacing([8.0, 4.0]).show(ui, |ui| {
            for row in rows {
                for (l, v) in row {
                    ui.weak(l);
                    ui.label(RichText::new(v).strong());
                }
                ui.end_row();
            }
        });
    }

    fn ui_procs(&mut self, ui: &mut egui::Ui) {
        let mut acts: Vec<MAct> = Vec::new();
        let sel_id = self.sel.as_ref().map(|p| p.id);
        let can_kill = sel_id.is_some_and(|id| id != self.own_id && self.procs.iter().any(|p| p.id == id));

        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter (Benutzer, Host, DB, Befehl, Abfrage)").desired_width(260.0));
            ui.checkbox(&mut self.hide_idle, "Leerlauf ausblenden (Sleep)");
            ui.checkbox(&mut self.hide_own, "Eigene Verbindung ausblenden");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(can_kill, icons::button(Icon::Delete, "Verbindung beenden")).clicked() {
                    acts.push(MAct::Kill(sel_id.unwrap()));
                }
                if ui.add_enabled(can_kill, egui::Button::new("Abfrage abbrechen")).clicked() {
                    acts.push(MAct::KillQuery(sel_id.unwrap()));
                }
                if ui.add_enabled(sel_id.is_some(), egui::Button::new("EXPLAIN")).clicked() {
                    acts.push(MAct::Explain(sel_id.unwrap()));
                }
            });
        });
        if let Some(e) = &self.proc_err {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }

        let f = self.filter.to_lowercase();
        let mut list: Vec<&Proc> = self
            .procs
            .iter()
            .filter(|p| !(self.hide_idle && p.idle()))
            .filter(|p| !(self.hide_own && p.id == self.own_id))
            .filter(|p| {
                f.is_empty() || [&p.user, &p.host, &p.db, &p.command, &p.state, &p.info].iter().any(|s| s.to_lowercase().contains(&f))
            })
            .collect();
        let (sc, desc) = self.sort;
        list.sort_by(|a, b| {
            let o = a.cmp(b, sc);
            if desc {
                o.reverse()
            } else {
                o
            }
        });

        // Unten: Details zum gewählten Prozess
        if let Some(p) = self.sel.clone() {
            let gone = !self.procs.iter().any(|q| q.id == p.id);
            egui::Panel::bottom(egui::Id::unique(("mon_detail", self.id)))
                .resizable(true)
                .default_size(200.0)
                .min_size(80.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.strong(format!("Prozess {}", p.id));
                        ui.label(format!("{}@{}", p.user, p.host));
                        if !p.db.is_empty() {
                            ui.label(format!("· {}", p.db));
                        }
                        ui.label(format!("· {} · {} s", p.command, p.time));
                        if !p.state.is_empty() {
                            ui.label(format!("· {}", p.state));
                        }
                        if gone {
                            ui.colored_label(Color32::from_rgb(0xd0, 0x30, 0x30), "(beendet)");
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add_enabled(!p.info.is_empty(), icons::button(Icon::Copy, "Abfrage kopieren")).clicked() {
                                ui.ctx().copy_text(p.info.clone());
                            }
                            if self.tag_explain != 0 {
                                ui.spinner();
                            }
                        });
                    });
                    egui::ScrollArea::vertical().id_salt(("mon_info", self.id)).auto_shrink([false, false]).show(ui, |ui| {
                        if p.info.is_empty() {
                            ui.weak("Keine laufende Abfrage.");
                        } else {
                            let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, w: f32| sqlhl::layouter(ui, buf, w);
                            let mut t = p.info.as_str();
                            ui.add(egui::TextEdit::multiline(&mut t).code_editor().desired_width(f32::INFINITY).layouter(&mut layouter));
                        }
                        match &self.explain {
                            Some(Ok(rs)) => {
                                ui.add_space(6.0);
                                ui.strong("EXPLAIN");
                                result_table(ui, ("mon_explain", self.id), rs);
                            }
                            Some(Err(e)) => {
                                ui.colored_label(ui.visuals().error_fg_color, format!("EXPLAIN nicht möglich: {e}"));
                            }
                            None => {}
                        }
                    });
                });
        }

        ui.weak(format!("{} von {} Prozessen", list.len(), self.procs.len()));
        let weak = ui.visuals().weak_text_color();
        let own = self.own_id;
        egui::ScrollArea::horizontal().id_salt(("mon_procs_h", self.id)).auto_shrink([false, false]).show(ui, |ui| {
            let widths = [60.0, 100.0, 150.0, 110.0, 80.0, 60.0, 150.0, 75.0];
            let mut tb = TableBuilder::new(ui)
                .id_salt(("mon_procs", self.id))
                .striped(true)
                .resizable(true)
                .sense(Sense::click())
                .auto_shrink([false, false])
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
            for w in widths {
                tb = tb.column(Column::initial(w).at_least(30.0).clip(true));
            }
            tb = tb.column(Column::remainder().at_least(200.0).clip(true));
            tb.header(22.0, |mut h| {
                for (i, t) in HEADERS.iter().enumerate() {
                    h.col(|ui| {
                        let t = if sc == i { format!("{t} {}", if desc { "▼" } else { "▲" }) } else { t.to_string() };
                        if ui.add(egui::Label::new(RichText::new(t).strong()).sense(Sense::click()).selectable(false)).clicked() {
                            self.sort = if sc == i { (i, !desc) } else { (i, i == COL_TIME || i == 7) };
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, list.len(), |mut row| {
                    let p = list[row.index()];
                    row.set_selected(sel_id == Some(p.id));
                    // Lange laufende Abfragen hervorheben
                    let color = if p.idle() {
                        Some(weak)
                    } else if p.time >= 60 {
                        Some(Color32::from_rgb(0xd0, 0x30, 0x30))
                    } else if p.time >= 10 {
                        Some(Color32::from_rgb(0xd8, 0x8a, 0x10))
                    } else {
                        None
                    };
                    let mut resp: Option<egui::Response> = None;
                    for c in 0..HEADERS.len() {
                        let t = p.cell(c);
                        let (_, r) = row.col(|ui| {
                            let mut rt = RichText::new(t);
                            if let Some(col) = color {
                                rt = rt.color(col);
                            }
                            let l = egui::Label::new(rt).selectable(false).truncate();
                            if matches!(c, 0 | 5 | 7) {
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.add(l));
                            } else {
                                ui.add(l);
                            }
                        });
                        resp = Some(match resp {
                            Some(x) => x.union(r),
                            None => r,
                        });
                    }
                    let Some(r) = resp else { return };
                    if r.clicked() || r.secondary_clicked() {
                        acts.push(MAct::Select(p.id));
                    }
                    if r.double_clicked() {
                        acts.push(MAct::Explain(p.id));
                    }
                    r.context_menu(|ui| {
                        let mine = p.id == own;
                        if ui.add_enabled(!p.info.is_empty(), egui::Button::new("EXPLAIN")).clicked() {
                            acts.push(MAct::Select(p.id));
                            acts.push(MAct::Explain(p.id));
                            ui.close();
                        }
                        if ui.add_enabled(!p.info.is_empty(), icons::button(Icon::Copy, "Abfrage kopieren")).clicked() {
                            ui.ctx().copy_text(p.info.clone());
                            ui.close();
                        }
                        ui.separator();
                        if ui.add_enabled(!mine && !p.info.is_empty(), egui::Button::new("Abfrage abbrechen (KILL QUERY)")).clicked() {
                            acts.push(MAct::KillQuery(p.id));
                            ui.close();
                        }
                        if ui.add_enabled(!mine, icons::button(Icon::Delete, "Verbindung beenden (KILL)")).clicked() {
                            acts.push(MAct::Kill(p.id));
                            ui.close();
                        }
                        if ui.add_enabled(!p.user.is_empty(), egui::Button::new(format!("Alle Verbindungen von „{}“ beenden …", p.user))).clicked() {
                            acts.push(MAct::KillUser(p.user.clone()));
                            ui.close();
                        }
                    });
                });
            });
        });
        for a in acts {
            self.apply(a);
        }
    }

    fn ui_status(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.status_filter).hint_text("Filter").desired_width(220.0));
            ui.checkbox(&mut self.only_changed, "Nur veränderte");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Zähler zurücksetzen (FLUSH STATUS) …").clicked() {
                    self.confirm_flush = true;
                }
            });
        });
        let f = self.status_filter.to_lowercase();
        let rows: Vec<Vec<String>> = self
            .status
            .iter()
            .filter(|(k, _)| f.is_empty() || k.to_lowercase().contains(&f))
            .filter(|(k, _)| !self.only_changed || self.rate(k).is_some_and(|r| r > 0.0))
            .map(|(k, v)| {
                let r = self.rate(k).filter(|r| *r > 0.0).map(fmt_dec).unwrap_or_default();
                vec![k.clone(), v.clone(), r]
            })
            .collect();
        kv_table(ui, ("mon_status", self.id), &["Variable", "Wert", "pro Sekunde"], &rows);
    }

    fn ui_vars(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.var_filter).hint_text("Filter (Name oder Wert)").desired_width(220.0));
            ui.weak(format!("{} Variablen", self.vars.len()));
        });
        let f = self.var_filter.to_lowercase();
        let rows: Vec<Vec<String>> = self
            .vars
            .iter()
            .filter(|(k, v)| f.is_empty() || k.to_lowercase().contains(&f) || v.to_lowercase().contains(&f))
            .map(|(k, v)| vec![k.clone(), v.clone()])
            .collect();
        kv_table(ui, ("mon_vars", self.id), &["Variable", "Wert"], &rows);
    }

    fn dialogs(&mut self, ui: &mut egui::Ui) {
        if let Some(k) = &self.kill {
            let mut choice = 0;
            let title = if k.query_only { "Abfrage abbrechen" } else { "Verbindung beenden" };
            egui::Modal::new(egui::Id::unique(("mon_kill", self.id))).show(ui.ctx(), |ui| {
                ui.heading(title);
                ui.label(&k.text);
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button(title).clicked() {
                        choice = 1;
                    }
                    if ui.button("Abbrechen").clicked() {
                        choice = 2;
                    }
                });
            });
            if choice == 1 {
                let kw = if k.query_only { "KILL QUERY" } else { "KILL" };
                let stmts = k.ids.iter().map(|id| Stmt::plain(format!("{kw} {id}"))).collect();
                self.note = format!("{} Prozess(e) beendet", k.ids.len());
                if k.query_only {
                    self.note = format!("Abfrage von Prozess {} abgebrochen", k.ids[0]);
                }
                self.tag_action = self.worker.send(None, stmts, Mode::Continue, 0);
            }
            if choice != 0 {
                self.kill = None;
            }
        }
        if self.confirm_flush {
            let mut choice = 0;
            egui::Modal::new(egui::Id::unique(("mon_flush", self.id))).show(ui.ctx(), |ui| {
                ui.heading("Zähler zurücksetzen");
                ui.label("Sollen die Statuszähler des Servers mit FLUSH STATUS zurückgesetzt werden?");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Zurücksetzen").clicked() {
                        choice = 1;
                    }
                    if ui.button("Abbrechen").clicked() {
                        choice = 2;
                    }
                });
            });
            if choice == 1 {
                self.note = "Statuszähler zurückgesetzt".into();
                self.prev = None;
                self.tag_action = self.worker.send(None, vec![Stmt::plain("FLUSH STATUS")], Mode::StopOnError, 0);
            }
            if choice != 0 {
                self.confirm_flush = false;
            }
        }
    }
}

/// Einfache Tabelle (Name/Wert/…) mit Kontextmenü zum Kopieren.
fn kv_table(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, headers: &[&str], rows: &[Vec<String>]) {
    egui::ScrollArea::horizontal().id_salt(("h", &id)).auto_shrink([false, false]).show(ui, |ui| {
        let mut tb = TableBuilder::new(ui)
            .id_salt(&id)
            .striped(true)
            .resizable(true)
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::initial(300.0).at_least(80.0).clip(true));
        for i in 1..headers.len() {
            tb = tb.column(if i + 1 == headers.len() { Column::remainder().at_least(80.0) } else { Column::initial(260.0).at_least(60.0) }.clip(true));
        }
        tb.header(22.0, |mut h| {
            for t in headers {
                h.col(|ui| {
                    ui.strong(*t);
                });
            }
        })
        .body(|body| {
            body.rows(20.0, rows.len(), |mut row| {
                let r = &rows[row.index()];
                for (c, t) in r.iter().enumerate() {
                    row.col(|ui| {
                        let resp = ui.add(egui::Label::new(t.as_str()).truncate().sense(Sense::click()));
                        resp.context_menu(|ui| {
                            if ui.add(icons::button(Icon::Copy, "Wert kopieren")).clicked() {
                                ui.ctx().copy_text(t.clone());
                                ui.close();
                            }
                            if c > 0 && ui.add(icons::button(Icon::Copy, "Name kopieren")).clicked() {
                                ui.ctx().copy_text(r[0].clone());
                                ui.close();
                            }
                        });
                    });
                }
            });
        });
    });
}

/// Ergebnis (z. B. EXPLAIN) als kleine Tabelle.
fn result_table(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, rs: &ResultSet) {
    egui::Grid::new(id).striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
        for c in &rs.columns {
            ui.strong(&c.name);
        }
        ui.end_row();
        for r in &rs.rows {
            for v in r {
                match v {
                    Some(s) => ui.label(s),
                    None => ui.weak("NULL"),
                };
            }
            ui.end_row();
        }
    });
}

/// Ganzzahl mit Tausenderpunkten.
fn fmt_num(v: f64) -> String {
    let s = format!("{}", v.round() as i64);
    let (sign, digits) = s.strip_prefix('-').map_or(("", s.as_str()), |d| ("-", d));
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push('.');
        }
        out.push(ch);
    }
    format!("{sign}{out}")
}

/// Kommazahl mit einer Nachkommastelle (große Werte ganzzahlig).
fn fmt_dec(v: f64) -> String {
    if v >= 1000.0 {
        fmt_num(v)
    } else {
        format!("{v:.1}").replace('.', ",")
    }
}

fn fmt_dur(s: i64) -> String {
    let (d, h, m, s) = (s / 86_400, s % 86_400 / 3600, s % 3600 / 60, s % 60);
    if d > 0 {
        format!("{d} T {h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(fmt_num(0.0), "0");
        assert_eq!(fmt_num(1234567.0), "1.234.567");
        assert_eq!(fmt_num(-1000.0), "-1.000");
        assert_eq!(fmt_dec(12.34), "12,3");
        assert_eq!(fmt_dur(90_061), "1 T 01:01:01");
        assert_eq!(fmt_dur(59), "00:00:59");
    }
}
