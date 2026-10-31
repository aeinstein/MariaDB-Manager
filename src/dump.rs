//! SQL-Dump wie `mysqldump` – nativ, ohne externes Programm. Läuft auf einer eigenen
//! Verbindung (optional in einer konsistenten Transaktion) und streamt die Daten direkt
//! in die Datei. Ebenen: ganze Datenbank oder einzelne Tabelle/Ansicht.
//!
//! Aufbau der Datei wie bei mysqldump: Tabellen (Struktur, Daten, Trigger), dann
//! Ansichten (erst Platzhalter, dann die echten Definitionen – so spielt die
//! Reihenfolge keine Rolle), Routinen und Ereignisse. Die Datei lässt sich mit
//! `mariadb < datei.sql` oder im SQL-Editor (DELIMITER wird unterstützt) einspielen.

use crate::db::{self, qi, qs, ConnConfig};
use crate::export::{self, ExportJob, Progress};
use eframe::egui;
use mysql::consts::ColumnType;
use mysql::prelude::*;
use mysql::{Conn, Row, Value};
use serde::{Deserialize, Serialize};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// Größe eines Mehrzeilen-INSERTs, ab der ein neues Statement beginnt (wie mysqldump: 1 MB).
const MAX_STMT: usize = 1 << 20;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct DumpOpts {
    pub structure: bool,
    pub data: bool,
    pub drop: bool,
    pub create_db: bool,
    pub views: bool,
    pub triggers: bool,
    pub routines: bool,
    pub events: bool,
    pub extended_insert: bool,
    pub complete_insert: bool,
    pub single_transaction: bool,
    pub add_locks: bool,
    pub strip_definer: bool,
}

impl Default for DumpOpts {
    fn default() -> Self {
        Self {
            structure: true,
            data: true,
            drop: true,
            create_db: false,
            views: true,
            triggers: true,
            routines: true,
            events: true,
            extended_insert: true,
            complete_insert: false,
            single_transaction: true,
            add_locks: true,
            strip_definer: false,
        }
    }
}

/// Startet den Dump im Hintergrund. `table = None` → ganze Datenbank.
pub fn start(cfg: ConnConfig, db: String, table: Option<String>, opts: DumpOpts, path: PathBuf, ctx: egui::Context) -> ExportJob {
    export::spawn(path, ctx, move |path, p| {
        let f = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut w = BufWriter::with_capacity(1 << 16, f);
        let n = Dumper { c: connect(&cfg, &opts)?, w: &mut w, o: &opts, db: &db, p, rows: 0 }.run(&cfg, table.as_deref())?;
        w.flush().map_err(|e| e.to_string())?;
        Ok(n)
    })
}

fn connect(cfg: &ConnConfig, o: &DumpOpts) -> Result<Conn, String> {
    let mut c = db::connect(cfg).map_err(|e| format!("Verbindung fehlgeschlagen: {e}"))?;
    let mut init = vec![
        "SET NAMES utf8mb4",
        // Zeitstempel unabhängig von der Server-Zeitzone (wie mysqldump --tz-utc)
        "SET SESSION TIME_ZONE = '+00:00'",
        // kein ANSI_QUOTES o. Ä., damit SHOW CREATE mit Backticks quotet
        "SET SESSION SQL_MODE = ''",
        "SET SESSION SQL_QUOTE_SHOW_CREATE = 1",
    ];
    if o.single_transaction {
        init.push("SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ");
        init.push("START TRANSACTION WITH CONSISTENT SNAPSHOT");
    }
    for s in init {
        c.query_drop(s).map_err(|e| format!("{s}: {e}"))?;
    }
    Ok(c)
}

#[derive(Clone, Copy)]
enum Kind {
    Num,
    Bin,
    Str,
}

fn kind(c: &mysql::Column) -> Kind {
    if c.column_type() == ColumnType::MYSQL_TYPE_BIT || db::is_binary_col(c) {
        Kind::Bin
    } else if db::is_numeric(c.column_type()) {
        Kind::Num
    } else {
        Kind::Str
    }
}

/// Wert als SQL-Literal anhängen (Escaping wie mysqldump).
fn push_value(out: &mut Vec<u8>, v: Value, k: Kind) {
    match v {
        Value::NULL => out.extend_from_slice(b"NULL"),
        Value::Bytes(b) => match k {
            Kind::Num => out.extend_from_slice(&b),
            Kind::Bin if b.is_empty() => out.extend_from_slice(b"''"),
            Kind::Bin => {
                const H: &[u8; 16] = b"0123456789ABCDEF";
                out.extend_from_slice(b"0x");
                for x in b {
                    out.push(H[(x >> 4) as usize]);
                    out.push(H[(x & 15) as usize]);
                }
            }
            Kind::Str => push_str(out, &b),
        },
        // Textprotokoll liefert eigentlich nur Bytes – der Vollständigkeit halber
        other => {
            let s = db::value_full(other, false).unwrap_or_default();
            if matches!(k, Kind::Num) {
                out.extend_from_slice(s.as_bytes());
            } else {
                push_str(out, s.as_bytes());
            }
        }
    }
}

fn push_str(out: &mut Vec<u8>, b: &[u8]) {
    out.push(b'\'');
    for &c in b {
        match c {
            0 => out.extend_from_slice(b"\\0"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\'' => out.extend_from_slice(b"\\'"),
            b'"' => out.extend_from_slice(b"\\\""),
            0x1a => out.extend_from_slice(b"\\Z"),
            _ => out.push(c),
        }
    }
    out.push(b'\'');
}

/// `DEFINER=`user`@`host`` aus einer CREATE-Anweisung entfernen.
pub fn strip_definer(sql: &str) -> String {
    let Some(i) = sql.find("DEFINER=") else { return sql.to_string() };
    let rest = &sql[i + 8..];
    // Benutzer und Host sind mit Backticks (oder Quotes) gequotet: `a`@`b`
    let mut chars = rest.char_indices().peekable();
    let mut end = rest.len();
    let mut parts = 0;
    while let Some((_, c)) = chars.next() {
        if c == '`' || c == '\'' {
            // bis zum schließenden Zeichen (verdoppelte Quotes überspringen)
            while let Some((_, d)) = chars.next() {
                if d == c {
                    if chars.peek().is_some_and(|&(_, n)| n == c) {
                        chars.next();
                        continue;
                    }
                    break;
                }
            }
            parts += 1;
            if parts == 2 {
                end = chars.peek().map_or(rest.len(), |&(j, _)| j);
                break;
            }
        } else if c != '@' {
            // unquotiert (z. B. CURRENT_USER) – bis zum Leerzeichen
            end = rest.find(' ').unwrap_or(rest.len());
            break;
        }
    }
    let tail = rest[end..].trim_start();
    format!("{}{}", &sql[..i], tail)
}

struct Dumper<'a> {
    c: Conn,
    w: &'a mut BufWriter<std::fs::File>,
    o: &'a DumpOpts,
    db: &'a str,
    p: &'a Progress,
    rows: u64,
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

impl Dumper<'_> {
    fn out(&mut self, s: &str) -> Result<(), String> {
        self.w.write_all(s.as_bytes()).map_err(err)
    }

    fn definer(&self, s: String) -> String {
        if self.o.strip_definer {
            strip_definer(&s)
        } else {
            s
        }
    }

    /// Erste Zeile eines SHOW CREATE …; Spalte `col` (NULL = fehlende Rechte).
    fn show_create(&mut self, sql: &str, col: usize) -> Result<Option<(String, String)>, String> {
        let row: Option<Row> = self.c.query_first(sql).map_err(|e| format!("{sql}: {e}"))?;
        Ok(row.and_then(|r| {
            let stmt: Option<String> = r.get::<Option<String>, usize>(col).flatten();
            let mode: String = r.get::<Option<String>, usize>(1).flatten().unwrap_or_default();
            stmt.map(|s| (s, mode))
        }))
    }

    fn run(mut self, cfg: &ConnConfig, table: Option<&str>) -> Result<u64, String> {
        let db = self.db.to_string();
        let version: String = self.c.query_first("SELECT VERSION()").map_err(err)?.unwrap_or_default();
        let now = crate::queries::fmt_time(std::time::SystemTime::now());
        let what = match table {
            Some(t) => format!("Datenbank: {db}  Tabelle: {t}"),
            None => format!("Datenbank: {db}"),
        };
        self.out(&format!(
            "-- MariaDB Manager {} – SQL-Dump\n--\n-- Host: {}    {what}\n-- Server-Version: {version}\n-- Erstellt: {now} (UTC)\n\n",
            env!("CARGO_PKG_VERSION"),
            cfg.host
        ))?;
        self.out(
            "/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */;\n\
             /*!40101 SET @OLD_CHARACTER_SET_RESULTS=@@CHARACTER_SET_RESULTS */;\n\
             /*!40101 SET @OLD_COLLATION_CONNECTION=@@COLLATION_CONNECTION */;\n\
             /*!40101 SET NAMES utf8mb4 */;\n\
             /*!40103 SET @OLD_TIME_ZONE=@@TIME_ZONE */;\n\
             /*!40103 SET TIME_ZONE='+00:00' */;\n\
             /*!40014 SET @OLD_UNIQUE_CHECKS=@@UNIQUE_CHECKS, UNIQUE_CHECKS=0 */;\n\
             /*!40014 SET @OLD_FOREIGN_KEY_CHECKS=@@FOREIGN_KEY_CHECKS, FOREIGN_KEY_CHECKS=0 */;\n\
             /*!40101 SET @OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='NO_AUTO_VALUE_ON_ZERO' */;\n\
             /*!40111 SET @OLD_SQL_NOTES=@@SQL_NOTES, SQL_NOTES=0 */;\n\n",
        )?;

        if self.o.create_db && table.is_none() {
            let sql = format!("SHOW CREATE DATABASE {}", qi(&db));
            if let Some((create, _)) = self.show_create(&sql, 1)? {
                let create = create.replacen("CREATE DATABASE", "CREATE DATABASE /*!32312 IF NOT EXISTS*/", 1);
                self.out(&format!("--\n-- Datenbank {}\n--\n\n{create};\n\nUSE {};\n\n", qi(&db), qi(&db)))?;
            }
        }

        // Tabellen, Ansichten, Sequenzen
        let mut q = "SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?".to_string();
        let mut params = vec![db.clone()];
        if let Some(t) = table {
            q.push_str(" AND TABLE_NAME = ?");
            params.push(t.to_string());
        }
        q.push_str(" ORDER BY TABLE_NAME");
        let objs: Vec<(String, String)> = self.c.exec(q, params).map_err(err)?;
        if objs.is_empty() {
            return Err(match table {
                Some(t) => format!("Tabelle „{t}“ nicht gefunden."),
                None => format!("Datenbank „{db}“ nicht gefunden oder leer."),
            });
        }
        let views: Vec<String> = objs.iter().filter(|o| o.1 == "VIEW").map(|o| o.0.clone()).collect();

        for (name, ty) in objs.iter().filter(|o| o.1 != "VIEW") {
            self.p.stage(name.clone());
            let seq = ty == "SEQUENCE";
            if self.o.structure {
                self.table_structure(name)?;
            }
            if self.o.data {
                if seq {
                    self.sequence_value(name)?;
                } else {
                    self.table_data(name)?;
                }
            }
            if self.o.triggers && !seq {
                self.triggers(name)?;
            }
        }

        if self.o.structure && self.o.views && !views.is_empty() {
            // Platzhalter, damit Ansichten, die auf andere Ansichten verweisen, in beliebiger Reihenfolge entstehen können
            for v in &views {
                self.p.stage(v.clone());
                self.view_stub(v)?;
            }
            for v in &views {
                self.view(v)?;
            }
        }

        if table.is_none() {
            if self.o.routines {
                self.routines()?;
            }
            if self.o.events {
                self.events()?;
            }
        }

        self.out(
            "/*!40103 SET TIME_ZONE=@OLD_TIME_ZONE */;\n\
             /*!40101 SET SQL_MODE=@OLD_SQL_MODE */;\n\
             /*!40014 SET FOREIGN_KEY_CHECKS=@OLD_FOREIGN_KEY_CHECKS */;\n\
             /*!40014 SET UNIQUE_CHECKS=@OLD_UNIQUE_CHECKS */;\n\
             /*!40101 SET CHARACTER_SET_CLIENT=@OLD_CHARACTER_SET_CLIENT */;\n\
             /*!40101 SET CHARACTER_SET_RESULTS=@OLD_CHARACTER_SET_RESULTS */;\n\
             /*!40101 SET COLLATION_CONNECTION=@OLD_COLLATION_CONNECTION */;\n\
             /*!40111 SET SQL_NOTES=@OLD_SQL_NOTES */;\n\n",
        )?;
        let now = crate::queries::fmt_time(std::time::SystemTime::now());
        self.out(&format!("-- Dump abgeschlossen: {now} (UTC)\n"))?;
        if self.o.single_transaction {
            let _ = self.c.query_drop("COMMIT");
        }
        self.p.rows(self.rows)?;
        Ok(self.rows)
    }

    fn table_structure(&mut self, t: &str) -> Result<(), String> {
        let fq = format!("{}.{}", qi(self.db), qi(t));
        let Some((create, _)) = self.show_create(&format!("SHOW CREATE TABLE {fq}"), 1)? else { return Ok(()) };
        let mut s = format!("--\n-- Struktur von {}\n--\n\n", qi(t));
        if self.o.drop {
            s += &format!("DROP TABLE IF EXISTS {};\n", qi(t));
        }
        s += "/*!40101 SET @saved_cs_client     = @@character_set_client */;\n/*!40101 SET character_set_client = utf8mb4 */;\n";
        s += &create;
        s += ";\n/*!40101 SET character_set_client = @saved_cs_client */;\n\n";
        self.out(&s)
    }

    fn sequence_value(&mut self, t: &str) -> Result<(), String> {
        let sql = format!("SELECT next_not_cached_value FROM {}.{}", qi(self.db), qi(t));
        let v: Option<String> = self.c.query_first(&sql).map_err(|e| format!("{sql}: {e}"))?;
        if let Some(v) = v {
            self.out(&format!("SELECT SETVAL({}, {v}, 0);\n\n", qi(t)))?;
        }
        Ok(())
    }

    fn table_data(&mut self, t: &str) -> Result<(), String> {
        // Generierte Spalten können nicht eingefügt werden; unsichtbare liefert SELECT * nicht
        let cols: Vec<(String, String)> = self
            .c
            .exec(
                "SELECT COLUMN_NAME, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
                (self.db, t),
            )
            .map_err(err)?;
        let generated = |e: &str| {
            let e = e.to_ascii_uppercase();
            e.contains("VIRTUAL") || e.contains("STORED") || e.contains("PERSISTENT")
        };
        let special = cols.iter().any(|(_, e)| generated(e) || e.to_ascii_uppercase().contains("INVISIBLE"));
        let names: Vec<String> = cols.iter().filter(|(_, e)| !generated(e)).map(|(n, _)| qi(n)).collect();
        if names.is_empty() {
            return Ok(());
        }
        let list = names.join(", ");
        let prefix = if self.o.complete_insert || special {
            format!("INSERT INTO {} ({list}) VALUES ", qi(t))
        } else {
            format!("INSERT INTO {} VALUES ", qi(t))
        };

        let mut head = format!("--\n-- Daten von {}\n--\n\n", qi(t));
        if self.o.add_locks {
            head += &format!("LOCK TABLES {} WRITE;\n", qi(t));
        }
        head += &format!("/*!40000 ALTER TABLE {} DISABLE KEYS */;\n", qi(t));
        self.out(&head)?;

        let sql = format!("SELECT {list} FROM {}.{}", qi(self.db), qi(t));
        let extended = self.o.extended_insert;
        let mut n = self.rows;
        {
            let mut res = self.c.query_iter(&sql).map_err(|e| format!("{sql}: {e}"))?;
            if let Some(set) = res.iter() {
                let kinds: Vec<Kind> = set.columns().as_ref().iter().map(kind).collect();
                let mut buf: Vec<u8> = Vec::with_capacity(MAX_STMT + 4096);
                for row in set {
                    let row = row.map_err(err)?;
                    if buf.is_empty() {
                        buf.extend_from_slice(prefix.as_bytes());
                    } else {
                        buf.push(b',');
                    }
                    buf.push(b'(');
                    for (i, v) in row.unwrap().into_iter().enumerate() {
                        if i > 0 {
                            buf.push(b',');
                        }
                        push_value(&mut buf, v, kinds.get(i).copied().unwrap_or(Kind::Str));
                    }
                    buf.push(b')');
                    if !extended || buf.len() >= MAX_STMT {
                        buf.extend_from_slice(b";\n");
                        self.w.write_all(&buf).map_err(err)?;
                        buf.clear();
                    }
                    n += 1;
                    if n % 2000 == 0 {
                        self.p.rows(n)?;
                    }
                }
                if !buf.is_empty() {
                    buf.extend_from_slice(b";\n");
                    self.w.write_all(&buf).map_err(err)?;
                }
            };
        }
        self.rows = n;
        self.p.rows(n)?;

        let mut tail = format!("/*!40000 ALTER TABLE {} ENABLE KEYS */;\n", qi(t));
        if self.o.add_locks {
            tail += "UNLOCK TABLES;\n";
        }
        tail += "\n";
        self.out(&tail)
    }

    /// SHOW CREATE TRIGGER/PROCEDURE/FUNCTION/EVENT mit DELIMITER ;; und eigenem sql_mode.
    fn routine_block(&mut self, kind: &str, name: &str, col: usize) -> Result<(), String> {
        let sql = format!("SHOW CREATE {kind} {}.{}", qi(self.db), qi(name));
        let Some((create, mode)) = self.show_create(&sql, col)? else {
            return self.out(&format!("-- {kind} {}: keine Berechtigung, Definition übersprungen\n\n", qi(name)));
        };
        let create = self.definer(create);
        let mut s = String::new();
        if self.o.drop {
            s += &format!("/*!50003 DROP {kind} IF EXISTS {} */;\n", qi(name));
        }
        s += &format!(
            "/*!50003 SET @saved_sql_mode = @@sql_mode */;\n/*!50003 SET sql_mode = {} */;\nDELIMITER ;;\n{create} ;;\nDELIMITER ;\n/*!50003 SET sql_mode = @saved_sql_mode */;\n\n",
            qs(&mode)
        );
        self.out(&s)
    }

    fn triggers(&mut self, t: &str) -> Result<(), String> {
        let names: Vec<String> = self
            .c
            .exec(
                "SELECT TRIGGER_NAME FROM information_schema.TRIGGERS WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ? ORDER BY EVENT_MANIPULATION, ACTION_TIMING, ACTION_ORDER",
                (self.db, t),
            )
            .map_err(err)?;
        if names.is_empty() {
            return Ok(());
        }
        self.out(&format!("--\n-- Trigger von {}\n--\n\n", qi(t)))?;
        for n in names {
            self.routine_block("TRIGGER", &n, 2)?;
        }
        Ok(())
    }

    fn view_stub(&mut self, v: &str) -> Result<(), String> {
        let cols: Vec<String> = self
            .c
            .exec(
                "SELECT COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
                (self.db, v),
            )
            .map_err(err)?;
        let sel = if cols.is_empty() { "1 AS `1`".to_string() } else { cols.iter().map(|c| format!("1 AS {}", qi(c))).collect::<Vec<_>>().join(", ") };
        let mut s = format!("--\n-- Platzhalter für Ansicht {}\n--\n\n", qi(v));
        if self.o.drop {
            s += &format!("DROP TABLE IF EXISTS {};\n/*!50001 DROP VIEW IF EXISTS {} */;\n", qi(v), qi(v));
        }
        s += &format!("/*!50001 CREATE VIEW {} AS SELECT {sel} */;\n\n", qi(v));
        self.out(&s)
    }

    fn view(&mut self, v: &str) -> Result<(), String> {
        let sql = format!("SHOW CREATE VIEW {}.{}", qi(self.db), qi(v));
        let Some((create, _)) = self.show_create(&sql, 1)? else { return Ok(()) };
        // MariaDB qualifiziert Ansicht und Tabellen mit der eigenen DB – entfernen, damit sich
        // der Dump auch in eine andere Datenbank einspielen lässt
        let create = self.definer(crate::sync::unqualify(&create, self.db));
        self.out(&format!(
            "--\n-- Ansicht {}\n--\n\n/*!50001 DROP VIEW IF EXISTS {} */;\n/*!50001 SET @saved_cs_client = @@character_set_client */;\n/*!50001 SET character_set_client = utf8mb4 */;\n{create};\n/*!50001 SET character_set_client = @saved_cs_client */;\n\n",
            qi(v),
            qi(v)
        ))
    }

    fn routines(&mut self) -> Result<(), String> {
        let list: Vec<(String, String)> = self
            .c
            .exec(
                "SELECT ROUTINE_NAME, ROUTINE_TYPE FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = ? ORDER BY ROUTINE_TYPE, ROUTINE_NAME",
                (self.db,),
            )
            .map_err(err)?;
        if list.is_empty() {
            return Ok(());
        }
        self.p.stage("Routinen");
        self.out("--\n-- Prozeduren und Funktionen\n--\n\n")?;
        for (name, ty) in list {
            // PACKAGE (Oracle-Modus) u. Ä. nicht unterstützt
            if ty == "PROCEDURE" || ty == "FUNCTION" {
                self.routine_block(&ty, &name, 2)?;
            }
        }
        Ok(())
    }

    fn events(&mut self) -> Result<(), String> {
        let list: Vec<String> = self
            .c
            .exec("SELECT EVENT_NAME FROM information_schema.EVENTS WHERE EVENT_SCHEMA = ? ORDER BY EVENT_NAME", (self.db,))
            .map_err(err)?;
        if list.is_empty() {
            return Ok(());
        }
        self.p.stage("Ereignisse");
        self.out("--\n-- Ereignisse\n--\n\n")?;
        for name in list {
            self.routine_block("EVENT", &name, 3)?;
        }
        Ok(())
    }
}

/// Dialog: Zieldatei und Optionen. `Some(Some(..))` = Start, `Some(None)` = Abbrechen.
pub struct DumpDialog {
    pub db: String,
    pub table: Option<String>,
    pub path: String,
    pub opts: DumpOpts,
}

impl DumpDialog {
    pub fn new(db: &str, table: Option<&str>, opts: DumpOpts) -> Self {
        let file = match table {
            Some(t) => format!("{db}_{t}"),
            None => db.to_string(),
        };
        Self { db: db.into(), table: table.map(Into::into), path: export::default_path_ext(&file, "sql"), opts }
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Option<Option<(PathBuf, DumpOpts)>> {
        let mut out = None;
        let db_level = self.table.is_none();
        egui::Modal::new(egui::Id::unique("dump_dlg")).show(ctx, |ui| {
            ui.set_width(540.0);
            ui.heading("SQL-Dump erstellen");
            match &self.table {
                Some(t) => ui.label(format!("Tabelle {}.{}", self.db, t)),
                None => ui.label(format!("Datenbank {}", self.db)),
            };
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Datei:");
                ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(420.0));
                #[cfg(any(windows, target_os = "macos"))]
                if ui.button("…").on_hover_text("Speicherort wählen").clicked() {
                    let p = PathBuf::from(&self.path);
                    let mut d = rfd::FileDialog::new().add_filter("SQL-Datei", &["sql"]);
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
            ui.add_space(6.0);
            let o = &mut self.opts;
            egui::Grid::new("dump_opts").num_columns(2).spacing([24.0, 4.0]).show(ui, |ui| {
                ui.checkbox(&mut o.structure, "Struktur (CREATE)");
                ui.checkbox(&mut o.data, "Daten (INSERT)");
                ui.end_row();
                ui.checkbox(&mut o.drop, "DROP … IF EXISTS voranstellen");
                ui.add_enabled(db_level, egui::Checkbox::new(&mut o.create_db, "CREATE DATABASE / USE"));
                ui.end_row();
                ui.add_enabled(o.structure, egui::Checkbox::new(&mut o.views, "Ansichten"));
                ui.checkbox(&mut o.triggers, "Trigger");
                ui.end_row();
                ui.add_enabled(db_level, egui::Checkbox::new(&mut o.routines, "Prozeduren & Funktionen"));
                ui.add_enabled(db_level, egui::Checkbox::new(&mut o.events, "Ereignisse"));
                ui.end_row();
                ui.add_enabled(o.data, egui::Checkbox::new(&mut o.extended_insert, "Mehrzeilige INSERTs"));
                ui.add_enabled(o.data, egui::Checkbox::new(&mut o.complete_insert, "Spaltennamen in INSERT"));
                ui.end_row();
                ui.add_enabled(o.data, egui::Checkbox::new(&mut o.add_locks, "LOCK TABLES beim Einspielen"));
                ui.checkbox(&mut o.strip_definer, "DEFINER entfernen");
                ui.end_row();
            });
            ui.checkbox(&mut o.single_transaction, "Konsistenter Stand (eine Transaktion, ohne Sperren – InnoDB)")
                .on_hover_text("START TRANSACTION WITH CONSISTENT SNAPSHOT, wie mysqldump --single-transaction");
            if Path::new(&self.path).exists() {
                ui.colored_label(ui.visuals().warn_fg_color, "Die Datei existiert bereits und wird überschrieben.");
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let ok = !self.path.trim().is_empty() && (self.opts.structure || self.opts.data);
                if ui.add_enabled(ok, egui::Button::new("Dump erstellen")).clicked() {
                    out = Some(Some((PathBuf::from(self.path.trim()), self.opts.clone())));
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
    fn escaping() {
        let mut b = Vec::new();
        push_str(&mut b, b"a'b\\c\nd\0\"\x1a");
        assert_eq!(b, b"'a\\'b\\\\c\\nd\\0\\\"\\Z'");
        let mut b = Vec::new();
        push_value(&mut b, Value::Bytes(vec![0xde, 0xad]), Kind::Bin);
        push_value(&mut b, Value::Bytes(vec![]), Kind::Bin);
        push_value(&mut b, Value::Bytes(b"12.5".to_vec()), Kind::Num);
        push_value(&mut b, Value::NULL, Kind::Str);
        assert_eq!(b, b"0xDEAD''12.5NULL");
    }

    #[test]
    fn definer() {
        assert_eq!(
            strip_definer("CREATE DEFINER=`root`@`localhost` PROCEDURE `p`() BEGIN END"),
            "CREATE PROCEDURE `p`() BEGIN END"
        );
        assert_eq!(
            strip_definer("CREATE ALGORITHM=UNDEFINED DEFINER=`a``b`@`%` SQL SECURITY DEFINER VIEW `v` AS select 1"),
            "CREATE ALGORITHM=UNDEFINED SQL SECURITY DEFINER VIEW `v` AS select 1"
        );
        assert_eq!(strip_definer("CREATE TABLE t (a int)"), "CREATE TABLE t (a int)");
    }

    /// Die Ausgabe muss sich mit `split_sql` (SQL-Editor) wieder zerlegen lassen.
    #[test]
    fn split_dump_output() {
        let mut ins = b"INSERT INTO `t` VALUES ".to_vec();
        push_value(&mut ins, Value::Bytes(b"a';\\b\nc\"".to_vec()), Kind::Str);
        let ins = String::from_utf8(ins).unwrap();
        let src = format!(
            "/*!40101 SET NAMES utf8mb4 */;\n{ins};\n/*!50003 SET sql_mode = '' */;\nDELIMITER ;;\n\
             CREATE PROCEDURE `p`() BEGIN SELECT 1; END ;;\nDELIMITER ;\nSELECT 2;\n"
        );
        let v = db::split_sql(&src);
        assert_eq!(v.len(), 5, "{v:#?}");
        assert_eq!(v[1], ins);
        assert_eq!(v[3].trim(), "CREATE PROCEDURE `p`() BEGIN SELECT 1; END");
    }

    /// Dump gegen echten Server (DB „shop“ aus tests/testdata.sql) und Wiedereinspielen in eine Kopie.
    #[test]
    fn itest_dump_roundtrip() {
        let Some(cfg) = db::itest::cfg() else { return };
        let path = std::env::temp_dir().join(format!("mdbm_dump_{}.sql", std::process::id()));
        let ctx = egui::Context::default();
        let job = start(cfg.clone(), "shop".into(), None, DumpOpts::default(), path.clone(), ctx);
        let n = loop {
            if let Some(r) = job.poll() {
                break r.expect("Dump fehlgeschlagen");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert!(n > 0);
        let sql = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(sql.contains("CREATE TABLE"));
        assert!(sql.contains("INSERT INTO"));

        // In eine frische DB einspielen und Zeilenzahlen vergleichen
        let mut c = db::connect(&cfg).unwrap();
        c.query_drop("DROP DATABASE IF EXISTS mdbm_dump_copy").unwrap();
        c.query_drop("CREATE DATABASE mdbm_dump_copy").unwrap();
        c.query_drop("USE mdbm_dump_copy").unwrap();
        for s in db::split_sql(&sql) {
            c.query_drop(&s).unwrap_or_else(|e| panic!("{e}\n{s}"));
        }
        let tables: Vec<String> = c
            .query("SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA='shop' AND TABLE_TYPE='BASE TABLE'")
            .unwrap();
        for t in tables {
            let a: u64 = c.query_first(format!("SELECT COUNT(*) FROM shop.{}", qi(&t))).unwrap().unwrap();
            let b: u64 = c.query_first(format!("SELECT COUNT(*) FROM mdbm_dump_copy.{}", qi(&t))).unwrap().unwrap();
            assert_eq!(a, b, "Zeilen in {t}");
        }
        c.query_drop("DROP DATABASE mdbm_dump_copy").unwrap();
    }
}
