//! Datenbankschicht: jeder `Worker` besitzt genau eine Verbindung in einem
//! eigenen Thread. Die UI schickt `Job`s und pollt Ergebnisse, ohne je zu blockieren.

use mysql::consts::{ColumnFlags, ColumnType};
use mysql::prelude::*;
use mysql::{Conn, Opts, OptsBuilder, SslOpts, Value};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// Maximale Zeichen, die pro Zelle gehalten werden, bevor gekürzt wird (nur BLOB-Anzeige).
const MAX_BLOB_HEX: usize = 4096;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ConnConfig {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub save_password: bool,
    pub default_db: String,
    pub use_ssl: bool,
    pub ssl_verify: bool,
}

impl Default for ConnConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            host: "localhost".into(),
            port: 3306,
            user: "root".into(),
            password: String::new(),
            save_password: true,
            default_db: String::new(),
            use_ssl: false,
            ssl_verify: false,
        }
    }
}

impl ConnConfig {
    pub fn opts(&self) -> Opts {
        self.opts_for(&self.host)
    }

    /// Wie `opts`, aber mit abweichendem Host (z. B. einer bereits aufgelösten IP).
    pub fn opts_for(&self, host: &str) -> Opts {
        let mut b = OptsBuilder::new()
            .ip_or_hostname(Some(host.to_string()))
            .tcp_port(self.port)
            .user(Some(self.user.clone()))
            .pass(Some(self.password.clone()))
            .prefer_socket(false)
            .tcp_connect_timeout(Some(Duration::from_secs(10)))
            .tcp_keepalive_time_ms(Some(60_000))
            .stmt_cache_size(Some(32))
            .init(vec!["SET NAMES utf8mb4"]);
        if !self.default_db.is_empty() {
            b = b.db_name(Some(self.default_db.clone()));
        }
        if self.use_ssl {
            b = b.ssl_opts(Some(
                SslOpts::default()
                    .with_danger_accept_invalid_certs(!self.ssl_verify)
                    .with_danger_skip_domain_validation(!self.ssl_verify),
            ));
        }
        b.into()
    }
}

/// Baut eine Verbindung auf. Hostnamen werden selbst aufgelöst und alle Adressen
/// nacheinander probiert (IPv4 zuerst): `mysql` bricht sonst schon nach der ersten
/// nicht erreichbaren Adresse ab (z. B. `localhost` → `::1`, Server lauscht nur auf IPv4).
pub fn connect(cfg: &ConnConfig) -> Result<Conn, mysql::Error> {
    let host = cfg.host.trim();
    // Bei Zertifikatsprüfung muss der Hostname erhalten bleiben.
    if host.parse::<std::net::IpAddr>().is_ok() || (cfg.use_ssl && cfg.ssl_verify) {
        return Conn::new(cfg.opts_for(host));
    }
    let mut addrs: Vec<std::net::IpAddr> = std::net::ToSocketAddrs::to_socket_addrs(&(host, cfg.port))
        .map_err(|e| {
            mysql::Error::IoError(std::io::Error::other(format!("Host „{host}“ konnte nicht aufgelöst werden: {e}")))
        })?
        .map(|a| a.ip())
        .collect();
    addrs.sort_by_key(|ip| ip.is_ipv6());
    let mut seen = Vec::new();
    addrs.retain(|ip| !seen.contains(ip) && { seen.push(*ip); true });
    let mut last = None;
    for ip in addrs {
        match Conn::new(cfg.opts_for(&ip.to_string())) {
            Ok(c) => return Ok(c),
            // Server hat geantwortet (z. B. Zugriff verweigert) → andere Adressen helfen nicht.
            Err(e @ mysql::Error::MySqlError(_)) => return Err(e),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        mysql::Error::IoError(std::io::Error::other(format!("Host „{host}“ liefert keine Adresse")))
    }))
}

/// Ein einzelnes Statement, optional mit Parametern (dann als Prepared Statement).
#[derive(Clone, Debug)]
pub struct Stmt {
    pub sql: String,
    pub params: Option<Vec<Option<String>>>,
}

impl Stmt {
    pub fn plain(sql: impl Into<String>) -> Self {
        Self { sql: sql.into(), params: None }
    }
    pub fn with(sql: impl Into<String>, params: Vec<Option<String>>) -> Self {
        Self { sql: sql.into(), params: Some(params) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Bei erstem Fehler abbrechen.
    StopOnError,
    /// Alles in einer Transaktion, bei Fehler Rollback.
    Transaction,
    /// Fehler protokollieren und weitermachen.
    Continue,
}

pub struct Job {
    pub tag: u64,
    pub db: Option<String>,
    pub stmts: Vec<Stmt>,
    pub mode: Mode,
    pub max_rows: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Col {
    pub name: String,
    pub numeric: bool,
    pub binary: bool,
    /// Herkunft laut Metadaten (leer bei berechneten Spalten): Datenbank, Tabelle, Spaltenname.
    pub schema: String,
    pub table: String,
    pub org: String,
}

#[derive(Clone, Debug, Default)]
pub struct ResultSet {
    pub columns: Vec<Col>,
    pub rows: Vec<Vec<Option<String>>>,
    pub truncated: bool,
}

impl ResultSet {
    pub fn col(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name))
    }
    /// Wert aus Zeile `r`, Spalte `name` als &str ("" bei NULL/fehlend).
    pub fn get<'a>(&'a self, r: usize, name: &str) -> &'a str {
        self.col(name)
            .and_then(|c| self.rows.get(r).and_then(|row| row[c].as_deref()))
            .unwrap_or("")
    }
}

#[derive(Clone, Debug, Default)]
pub struct StmtOut {
    pub sql: String,
    pub sets: Vec<ResultSet>,
    pub affected: u64,
    pub last_insert_id: Option<u64>,
    pub warnings: u16,
    pub info: String,
    pub elapsed: Duration,
    pub error: Option<String>,
}

pub struct JobResult {
    pub tag: u64,
    pub outs: Vec<StmtOut>,
    pub error: Option<String>,
    pub elapsed: Duration,
}

impl JobResult {
    /// Erstes Result-Set des n-ten Statements.
    pub fn set(&self, n: usize) -> Option<&ResultSet> {
        self.outs.get(n).and_then(|o| o.sets.first())
    }
    pub fn take_set(&mut self, n: usize) -> Option<ResultSet> {
        let o = self.outs.get_mut(n)?;
        if o.sets.is_empty() {
            None
        } else {
            Some(o.sets.remove(0))
        }
    }
}

/// Handle auf einen Verbindungs-Thread.
pub struct Worker {
    tx: mpsc::Sender<Job>,
    rx: mpsc::Receiver<JobResult>,
    conn_id: Arc<AtomicU32>,
    cfg: ConnConfig,
    next_tag: u64,
    pub busy: usize,
}

impl Worker {
    pub fn spawn(cfg: ConnConfig, ctx: eframe::egui::Context) -> Self {
        let (tx, job_rx) = mpsc::channel::<Job>();
        let (res_tx, rx) = mpsc::channel::<JobResult>();
        let conn_id = Arc::new(AtomicU32::new(0));
        let cid = conn_id.clone();
        let wcfg = cfg.clone();
        std::thread::Builder::new()
            .name("db-worker".into())
            .spawn(move || worker_loop(wcfg, job_rx, res_tx, cid, ctx))
            .expect("Thread konnte nicht gestartet werden");
        Self { tx, rx, conn_id, cfg, next_tag: 1, busy: 0 }
    }

    /// Schickt einen Job; liefert das Tag zurück, unter dem das Ergebnis kommt.
    pub fn send(&mut self, db: Option<String>, stmts: Vec<Stmt>, mode: Mode, max_rows: usize) -> u64 {
        let tag = self.next_tag;
        self.next_tag += 1;
        self.busy += 1;
        let _ = self.tx.send(Job { tag, db, stmts, mode, max_rows });
        tag
    }

    pub fn query(&mut self, db: Option<String>, sqls: &[&str]) -> u64 {
        self.send(db, sqls.iter().map(|s| Stmt::plain(*s)).collect(), Mode::StopOnError, usize::MAX)
    }

    pub fn poll(&mut self) -> Option<JobResult> {
        match self.rx.try_recv() {
            Ok(r) => {
                self.busy = self.busy.saturating_sub(1);
                Some(r)
            }
            Err(_) => None,
        }
    }

    /// Bricht die laufende Abfrage serverseitig ab (KILL QUERY über eine Zweitverbindung).
    pub fn cancel(&self) {
        let id = self.conn_id.load(Ordering::Relaxed);
        if id == 0 {
            return;
        }
        let cfg = self.cfg.clone();
        std::thread::spawn(move || {
            if let Ok(mut c) = connect(&cfg) {
                let _ = c.query_drop(format!("KILL QUERY {id}"));
            }
        });
    }
}

fn worker_loop(
    cfg: ConnConfig,
    jobs: mpsc::Receiver<Job>,
    out: mpsc::Sender<JobResult>,
    conn_id: Arc<AtomicU32>,
    ctx: eframe::egui::Context,
) {
    let mut conn: Option<Conn> = None;
    let mut cur_db: Option<String> = None;

    while let Ok(job) = jobs.recv() {
        let t0 = Instant::now();
        let mut result = JobResult { tag: job.tag, outs: Vec::new(), error: None, elapsed: Duration::ZERO };

        // Verbinden (bzw. nach Verbindungsabbruch neu verbinden).
        let alive = conn.as_mut().map(|c| c.ping().is_ok()).unwrap_or(false);
        if !alive {
            conn = None;
            cur_db = None;
            match connect(&cfg) {
                Ok(c) => {
                    conn_id.store(c.connection_id(), Ordering::Relaxed);
                    cur_db = Some(cfg.default_db.clone()).filter(|d| !d.is_empty());
                    conn = Some(c);
                }
                Err(e) => {
                    result.error = Some(format!("Verbindung fehlgeschlagen: {e}"));
                }
            }
        }

        if let Some(c) = conn.as_mut() {
            run_job(c, &job, &mut cur_db, &mut result);
        }
        result.elapsed = t0.elapsed();
        if out.send(result).is_err() {
            break;
        }
        ctx.request_repaint();
    }
}

fn run_job(c: &mut Conn, job: &Job, cur_db: &mut Option<String>, result: &mut JobResult) {
    if let Some(db) = &job.db {
        if cur_db.as_deref() != Some(db.as_str()) {
            if let Err(e) = c.query_drop(format!("USE {}", qi(db))) {
                result.error = Some(e.to_string());
                return;
            }
            *cur_db = Some(db.clone());
        }
    }
    if job.mode == Mode::Transaction {
        if let Err(e) = c.query_drop("START TRANSACTION") {
            result.error = Some(e.to_string());
            return;
        }
    }
    for stmt in &job.stmts {
        let out = run_stmt(c, stmt, job.max_rows);
        let failed = out.error.clone();
        // USE im SQL-Editor nachverfolgen
        if failed.is_none() {
            let t = stmt.sql.trim_start();
            if t.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("use ")) {
                let name = t[4..].trim().trim_end_matches(';').trim().trim_matches('`');
                *cur_db = Some(name.to_string());
            }
        }
        result.outs.push(out);
        if let Some(e) = failed {
            match job.mode {
                Mode::Continue => {}
                Mode::StopOnError => {
                    result.error = Some(e);
                    return;
                }
                Mode::Transaction => {
                    let _ = c.query_drop("ROLLBACK");
                    result.error = Some(format!("{e}\n(Transaktion zurückgerollt)"));
                    return;
                }
            }
        }
    }
    if job.mode == Mode::Transaction {
        if let Err(e) = c.query_drop("COMMIT") {
            result.error = Some(e.to_string());
        }
    }
}

fn run_stmt(c: &mut Conn, stmt: &Stmt, max_rows: usize) -> StmtOut {
    let t0 = Instant::now();
    let mut out = StmtOut { sql: stmt.sql.clone(), ..Default::default() };
    let r = match &stmt.params {
        None => c.query_iter(&stmt.sql).map(|mut q| collect(&mut q, max_rows, &mut out)),
        Some(p) => {
            let params: Vec<Value> = p
                .iter()
                .map(|v| match v {
                    None => Value::NULL,
                    Some(s) => Value::Bytes(s.as_bytes().to_vec()),
                })
                .collect();
            let params = if params.is_empty() { mysql::Params::Empty } else { mysql::Params::Positional(params) };
            c.exec_iter(&stmt.sql, params).map(|mut q| collect(&mut q, max_rows, &mut out))
        }
    };
    match r {
        Ok(Ok(())) => {}
        Ok(Err(e)) | Err(e) => out.error = Some(e.to_string()),
    }
    out.elapsed = t0.elapsed();
    out
}

fn collect<P: mysql::prelude::Protocol>(
    q: &mut mysql::QueryResult<'_, '_, '_, P>,
    max_rows: usize,
    out: &mut StmtOut,
) -> Result<(), mysql::Error> {
    while let Some(set) = q.iter() {
        let cols: Vec<Col> = set
            .columns()
            .as_ref()
            .iter()
            .map(|c| Col {
                name: c.name_str().into_owned(),
                numeric: is_numeric(c.column_type()),
                binary: is_binary_col(c),
                schema: c.schema_str().into_owned(),
                table: c.org_table_str().into_owned(),
                org: c.org_name_str().into_owned(),
            })
            .collect();
        let has_cols = !cols.is_empty();
        let mut rs = ResultSet { columns: cols, rows: Vec::new(), truncated: false };
        out.affected += set.affected_rows();
        if let Some(id) = set.last_insert_id() {
            if id > 0 {
                out.last_insert_id = Some(id);
            }
        }
        out.warnings = out.warnings.saturating_add(set.warnings());
        let info = set.info_str();
        if !info.is_empty() {
            out.info = info.into_owned();
        }
        for row in set {
            let row = row?;
            if rs.rows.len() >= max_rows {
                rs.truncated = true;
                continue; // Rest wird vom Protokoll ohnehin gelesen
            }
            let vals: Vec<Value> = row.unwrap();
            let r: Vec<Option<String>> = vals
                .into_iter()
                .enumerate()
                .map(|(i, v)| value_to_string(v, rs.columns[i].binary))
                .collect();
            rs.rows.push(r);
        }
        if has_cols {
            out.sets.push(rs);
        }
    }
    Ok(())
}

/// Binärspalte (BLOB/BINARY …) – Werte werden als Hex dargestellt.
pub fn is_binary_col(c: &mysql::Column) -> bool {
    c.character_set() == 63
        && !is_numeric(c.column_type())
        && !is_temporal(c.column_type())
        && !c.flags().contains(ColumnFlags::NUM_FLAG)
}

pub fn is_numeric(t: ColumnType) -> bool {
    use ColumnType::*;
    matches!(
        t,
        MYSQL_TYPE_TINY
            | MYSQL_TYPE_SHORT
            | MYSQL_TYPE_LONG
            | MYSQL_TYPE_INT24
            | MYSQL_TYPE_LONGLONG
            | MYSQL_TYPE_DECIMAL
            | MYSQL_TYPE_NEWDECIMAL
            | MYSQL_TYPE_FLOAT
            | MYSQL_TYPE_DOUBLE
            | MYSQL_TYPE_YEAR
    )
}

fn is_temporal(t: ColumnType) -> bool {
    use ColumnType::*;
    matches!(
        t,
        MYSQL_TYPE_DATE
            | MYSQL_TYPE_TIME
            | MYSQL_TYPE_DATETIME
            | MYSQL_TYPE_TIMESTAMP
            | MYSQL_TYPE_NEWDATE
            | MYSQL_TYPE_DATETIME2
            | MYSQL_TYPE_TIMESTAMP2
            | MYSQL_TYPE_TIME2
    )
}

fn value_to_string(v: Value, binary: bool) -> Option<String> {
    value_conv(v, binary, MAX_BLOB_HEX / 2)
}

/// Wie `value_to_string`, aber Binärwerte ungekürzt (für Export).
pub fn value_full(v: Value, binary: bool) -> Option<String> {
    value_conv(v, binary, usize::MAX)
}

fn value_conv(v: Value, binary: bool, max_bytes: usize) -> Option<String> {
    match v {
        Value::NULL => None,
        Value::Bytes(b) => {
            if binary {
                Some(hex(&b, max_bytes))
            } else {
                Some(match String::from_utf8(b) {
                    Ok(s) => s,
                    Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
                })
            }
        }
        Value::Int(i) => Some(i.to_string()),
        Value::UInt(u) => Some(u.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::Double(d) => Some(d.to_string()),
        Value::Date(y, m, d, h, mi, s, us) => Some(if us > 0 {
            format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}.{us:06}")
        } else {
            format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
        }),
        Value::Time(neg, d, h, m, s, us) => {
            let h = d * 24 + h as u32;
            let sign = if neg { "-" } else { "" };
            Some(if us > 0 {
                format!("{sign}{h:02}:{m:02}:{s:02}.{us:06}")
            } else {
                format!("{sign}{h:02}:{m:02}:{s:02}")
            })
        }
    }
}

fn hex(b: &[u8], max_bytes: usize) -> String {
    const H: &[u8; 16] = b"0123456789ABCDEF";
    let n = b.len().min(max_bytes);
    let mut s = String::with_capacity(2 + n * 2 + 24);
    s.push_str("0x");
    for &x in &b[..n] {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 15) as usize] as char);
    }
    if n < b.len() {
        s.push_str(&format!("… ({} Bytes)", b.len()));
    }
    s
}

/// Bezeichner mit Backticks quoten.
pub fn qi(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// String-Literal quoten (nur für DDL/DCL, Daten laufen über Parameter).
pub fn qs(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
}

/// Teilt ein SQL-Skript in einzelne Statements. Beachtet Strings, Bezeichner,
/// Kommentare und `DELIMITER`-Anweisungen.
pub fn split_sql(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut delim = ";".to_string();
    let b = src.as_bytes();
    let mut i = 0;
    let mut start = 0;
    let mut at_line_start = true;
    while i < b.len() {
        let c = b[i];
        // DELIMITER-Befehl am Zeilenanfang
        if at_line_start {
            let rest = &src[i..];
            let trimmed = rest.trim_start_matches([' ', '\t']);
            if trimmed.len() >= 10 && trimmed[..10].eq_ignore_ascii_case("delimiter ") {
                let pending = src[start..i].trim();
                if !pending.is_empty() {
                    out.push(pending.to_string());
                }
                let line_end = rest.find('\n').map(|p| i + p).unwrap_or(b.len());
                let skip = rest.len() - trimmed.len();
                let d = src[i + skip + 10..line_end].trim();
                if !d.is_empty() {
                    delim = d.to_string();
                }
                i = line_end;
                start = i;
                continue;
            }
        }
        at_line_start = false;
        match c {
            b'\'' | b'"' | b'`' => {
                i += 1;
                while i < b.len() {
                    if b[i] == b'\\' && c != b'`' {
                        i += 2;
                        continue;
                    }
                    if b[i] == c {
                        if i + 1 < b.len() && b[i + 1] == c {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
                continue;
            }
            b'-' if i + 2 < b.len() && b[i + 1] == b'-' && (b[i + 2] == b' ' || b[i + 2] == b'\t') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            b'\n' => {
                at_line_start = true;
                i += 1;
                continue;
            }
            _ => {}
        }
        if src[i..].starts_with(delim.as_str()) {
            let s = src[start..i].trim();
            if !s.is_empty() {
                out.push(s.to_string());
            }
            i += delim.len();
            start = i;
            continue;
        }
        i += 1;
    }
    let s = src[start.min(src.len())..].trim();
    if !s.is_empty() {
        out.push(s.to_string());
    }
    out.retain(|s| !is_only_comments(s));
    out
}

fn is_only_comments(s: &str) -> bool {
    s.lines().all(|l| {
        let l = l.trim();
        l.is_empty() || l.starts_with("-- ") || l == "--" || l.starts_with('#')
    }) && !s.contains("/*")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_basic() {
        let v = split_sql("SELECT 1; SELECT ';'; -- x;\nSELECT `a;b` FROM t");
        assert_eq!(v, vec!["SELECT 1", "SELECT ';'", "-- x;\nSELECT `a;b` FROM t"]);
    }

    #[test]
    fn split_delimiter() {
        let v = split_sql("DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; END$$\nDELIMITER ;\nSELECT 2;");
        assert_eq!(v, vec!["CREATE PROCEDURE p() BEGIN SELECT 1; END", "SELECT 2"]);
    }

    #[test]
    fn split_comment_only() {
        assert!(split_sql("-- nur Kommentar\n").is_empty());
        assert_eq!(split_sql("/* a; */ SELECT 1"), vec!["/* a; */ SELECT 1"]);
    }

    #[test]
    fn quoting() {
        assert_eq!(qi("a`b"), "`a``b`");
        assert_eq!(qs("it's"), "'it''s'");
    }

    #[test]
    fn opts_for_host() {
        let cfg = ConnConfig { host: "db.example".into(), ..Default::default() };
        assert_eq!(cfg.opts().get_ip_or_hostname(), "db.example");
        assert_eq!(cfg.opts_for("127.0.0.1").get_ip_or_hostname(), "127.0.0.1");
    }

    #[test]
    fn connect_unresolvable_host() {
        let cfg = ConnConfig { host: "gibt-es-nicht.invalid".into(), ..Default::default() };
        let e = connect(&cfg).err().expect("Fehler erwartet").to_string();
        assert!(e.contains("konnte nicht aufgelöst werden"), "{e}");
    }
}

/// Integrationstests gegen einen echten Server. Aktiv nur, wenn `MDBM_TEST_HOST` gesetzt ist
/// (Benutzer/Passwort aus `MDBM_TEST_USER` / `MDBM_TEST_PASS`, DB `shop` mit Testdaten).
#[cfg(test)]
pub(crate) mod itest {
    use super::*;

    pub fn cfg() -> Option<ConnConfig> {
        let host = std::env::var("MDBM_TEST_HOST").ok()?;
        Some(ConnConfig {
            name: "test".into(),
            host,
            user: std::env::var("MDBM_TEST_USER").unwrap_or("test".into()),
            password: std::env::var("MDBM_TEST_PASS").unwrap_or("test".into()),
            ..Default::default()
        })
    }

    pub fn wait(w: &mut Worker) -> JobResult {
        let t0 = Instant::now();
        loop {
            if let Some(r) = w.poll() {
                return r;
            }
            assert!(t0.elapsed() < Duration::from_secs(60), "Timeout");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn roundtrip() {
        let Some(cfg) = cfg() else { return };
        let mut w = Worker::spawn(cfg, eframe::egui::Context::default());
        w.query(Some("shop".into()), &["SELECT id, name, umsatz, notiz FROM kunden ORDER BY id LIMIT 3", "CALL top_kunden(2)"]);
        let r = wait(&mut w);
        assert!(r.error.is_none(), "{:?}", r.error);
        let s = r.set(0).unwrap();
        assert_eq!(s.rows.len(), 3);
        assert!(s.columns[0].numeric && s.columns[2].numeric && !s.columns[1].numeric);
        assert_eq!(s.rows[0][1].as_deref(), Some("Kunde 1"));
        assert_eq!(r.outs[1].sets.len(), 1, "CALL liefert ein Result-Set");

        // Binärspalten werden hex angezeigt
        w.query(Some("shop".into()), &["SELECT inhalt FROM dateien ORDER BY id"]);
        let r = wait(&mut w);
        let s = r.set(0).unwrap();
        assert!(s.columns[0].binary);
        assert_eq!(s.rows[0][0].as_deref(), Some("0xDEADBEEF00FF"));
        assert_eq!(s.rows[1][0], None);
    }

    #[test]
    fn params_and_transaction() {
        let Some(cfg) = cfg() else { return };
        let mut w = Worker::spawn(cfg, eframe::egui::Context::default());
        w.query(None, &["CREATE DATABASE IF NOT EXISTS mdbm_test", "CREATE OR REPLACE TABLE mdbm_test.t (id INT PRIMARY KEY, v VARCHAR(50))"]);
        assert!(wait(&mut w).error.is_none());
        // Parameter mit Sonderzeichen + NULL
        w.send(
            Some("mdbm_test".into()),
            vec![
                Stmt::with("INSERT INTO t (id, v) VALUES (?, ?)", vec![Some("1".into()), Some("O'Reilly \\ \"x\"".into())]),
                Stmt::with("INSERT INTO t (id, v) VALUES (?, ?)", vec![Some("2".into()), None]),
            ],
            Mode::Transaction,
            0,
        );
        assert!(wait(&mut w).error.is_none());
        // Transaktion mit Fehler -> Rollback
        w.send(
            Some("mdbm_test".into()),
            vec![
                Stmt::with("UPDATE t SET v = ? WHERE id <=> ?", vec![Some("geändert".into()), Some("1".into())]),
                Stmt::plain("INSERT INTO t (id) VALUES (1)"), // Duplikat
            ],
            Mode::Transaction,
            0,
        );
        let r = wait(&mut w);
        assert!(r.error.as_deref().unwrap_or("").contains("zurückgerollt"));
        w.query(Some("mdbm_test".into()), &["SELECT v FROM t ORDER BY id"]);
        let r = wait(&mut w);
        let s = r.set(0).unwrap();
        assert_eq!(s.rows[0][0].as_deref(), Some("O'Reilly \\ \"x\""));
        assert_eq!(s.rows[1][0], None);
        w.query(None, &["DROP DATABASE mdbm_test"]);
        wait(&mut w);
    }

    #[test]
    fn speed_and_cancel() {
        let Some(cfg) = cfg() else { return };
        let mut w = Worker::spawn(cfg, eframe::egui::Context::default());
        w.query(Some("shop".into()), &["SELECT * FROM kunden"]);
        let r = wait(&mut w);
        let s = r.set(0).unwrap();
        assert!(s.rows.len() >= 200_000);
        eprintln!("200.000 Zeilen × {} Spalten geladen in {:?}", s.columns.len(), r.elapsed);

        // Zeilenlimit
        w.send(Some("shop".into()), vec![Stmt::plain("SELECT * FROM bestellungen")], Mode::StopOnError, 1000);
        let r = wait(&mut w);
        assert!(r.set(0).unwrap().truncated);
        assert_eq!(r.set(0).unwrap().rows.len(), 1000);

        // Abbrechen
        w.query(None, &["SELECT SLEEP(20)"]);
        std::thread::sleep(Duration::from_millis(300));
        let t0 = Instant::now();
        w.cancel();
        let _ = wait(&mut w);
        assert!(t0.elapsed() < Duration::from_secs(5), "KILL QUERY wirkt nicht");
    }
}
