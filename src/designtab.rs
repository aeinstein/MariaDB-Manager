//! Tabellenentwurf im Stil von Navicat 8: Felder, Indizes, Fremdschlüssel, Trigger,
//! Optionen und Kommentar bearbeiten. Speichern erzeugt nur die Differenz als
//! `ALTER TABLE` (bzw. `CREATE TABLE` für neue Tabellen).

use crate::db::{qi, qs, ConnConfig, JobResult, Mode, ResultSet, Stmt, Worker};
use crate::icons::{self, Icon};
use crate::sqlhl;
use eframe::egui::{self, Key, Modifiers, RichText};
use egui_extras::{Column, TableBuilder};
use std::collections::{HashMap, HashSet};

const TYPES: &[&str] = &[
    "tinyint", "smallint", "mediumint", "int", "bigint", "decimal", "float", "double", "bit",
    "char", "varchar", "tinytext", "text", "mediumtext", "longtext",
    "binary", "varbinary", "tinyblob", "blob", "mediumblob", "longblob",
    "enum", "set", "date", "time", "datetime", "timestamp", "year",
    "json", "uuid", "inet4", "inet6",
    "geometry", "point", "linestring", "polygon", "multipoint", "multilinestring", "multipolygon", "geometrycollection",
];
const INDEX_KINDS: &[&str] = &["INDEX", "UNIQUE", "FULLTEXT", "SPATIAL"];
const INDEX_METHODS: &[&str] = &["", "BTREE", "HASH"];
const FK_RULES: &[&str] = &["RESTRICT", "CASCADE", "SET NULL", "NO ACTION", "SET DEFAULT"];
const ROW_FORMATS: &[&str] = &["", "DEFAULT", "DYNAMIC", "COMPACT", "REDUNDANT", "COMPRESSED", "FIXED", "PAGE"];
const GEN_KINDS: &[&str] = &["", "VIRTUAL", "PERSISTENT"];
const PART_METHODS: &[&str] = &["", "RANGE", "LIST", "HASH", "KEY", "LINEAR HASH", "LINEAR KEY", "RANGE COLUMNS", "LIST COLUMNS"];
const SUBPART_METHODS: &[&str] = &["", "HASH", "KEY", "LINEAR HASH", "LINEAR KEY"];

/// RANGE/LIST (inkl. COLUMNS) brauchen explizite Partitionen mit Werten.
fn part_explicit(m: &str) -> bool {
    m.starts_with("RANGE") || m.starts_with("LIST")
}

fn is_int(t: &str) -> bool {
    matches!(t, "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint")
}
fn is_numeric(t: &str) -> bool {
    is_int(t) || matches!(t, "decimal" | "numeric" | "float" | "double" | "real")
}
fn is_text(t: &str) -> bool {
    matches!(t, "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "enum" | "set")
}
fn takes_len(t: &str) -> bool {
    is_numeric(t)
        || matches!(t, "bit" | "char" | "varchar" | "binary" | "varbinary" | "text" | "blob" | "enum" | "set" | "time" | "datetime" | "timestamp")
}
fn takes_dec(t: &str) -> bool {
    matches!(t, "decimal" | "numeric" | "float" | "double" | "real")
}

// ------------------------------------------------------------------ Modell

#[derive(Clone, Debug, Default, PartialEq)]
struct Field {
    name: String,
    typ: String,
    /// Länge bzw. bei ENUM/SET die Werteliste (`'a','b'`).
    len: String,
    dec: String,
    nullable: bool,
    /// Standardwert als SQL-Ausdruck; leer = kein Standardwert.
    default: String,
    comment: String,
    auto_inc: bool,
    unsigned: bool,
    zerofill: bool,
    on_update: bool,
    charset: String,
    collation: String,
    gen_expr: String,
    gen_kind: String,
}

#[derive(Clone, Debug, Default)]
struct FieldRow {
    f: Field,
    /// Ursprünglicher Spaltenname; `None` = neues Feld.
    orig: Option<String>,
    /// Position im Primärschlüssel (Reihenfolge des Anklickens).
    pk: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Index {
    name: String,
    cols: String,
    kind: String,
    method: String,
    comment: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Fk {
    name: String,
    cols: String,
    ref_db: String,
    ref_table: String,
    ref_cols: String,
    on_delete: String,
    on_update: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Trigger {
    name: String,
    timing: String,
    event: String,
    body: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct TableOpts {
    engine: String,
    collation: String,
    auto_inc: String,
    row_format: String,
    comment: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Partition {
    name: String,
    /// RANGE: Obergrenze (z. B. `100`, `MAXVALUE`), LIST: Werteliste.
    values: String,
    comment: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Partitioning {
    /// Leer = nicht partitioniert.
    method: String,
    expr: String,
    /// Anzahl bei HASH/KEY.
    count: String,
    sub_method: String,
    sub_expr: String,
    sub_count: String,
    parts: Vec<Partition>,
    /// Vom Entwurf nicht unterstützt (z. B. SYSTEM_TIME) – wird nie geändert.
    readonly: bool,
}

/// `PARTITION BY …`-Klausel.
fn partition_sql(p: &Partitioning) -> String {
    let m = p.method.as_str();
    let mut s = format!("PARTITION BY {m}({})", p.expr.trim());
    let explicit = part_explicit(m);
    if !explicit && p.count.trim().parse::<u32>().is_ok() {
        s.push_str(&format!(" PARTITIONS {}", p.count.trim()));
    }
    if explicit && !p.sub_method.is_empty() {
        s.push_str(&format!(" SUBPARTITION BY {}({})", p.sub_method, p.sub_expr.trim()));
        if p.sub_count.trim().parse::<u32>().is_ok() {
            s.push_str(&format!(" SUBPARTITIONS {}", p.sub_count.trim()));
        }
    }
    if explicit {
        let defs: Vec<String> = p
            .parts
            .iter()
            .map(|pt| {
                let v = pt.values.trim();
                let mut d = if m.starts_with("RANGE") {
                    if v.eq_ignore_ascii_case("MAXVALUE") {
                        format!("PARTITION {} VALUES LESS THAN MAXVALUE", qi(pt.name.trim()))
                    } else {
                        format!("PARTITION {} VALUES LESS THAN ({v})", qi(pt.name.trim()))
                    }
                } else {
                    format!("PARTITION {} VALUES IN ({v})", qi(pt.name.trim()))
                };
                if !pt.comment.is_empty() {
                    d.push_str(&format!(" COMMENT = {}", qs(&pt.comment)));
                }
                d
            })
            .collect();
        s.push_str(&format!(" (\n  {}\n)", defs.join(",\n  ")));
    }
    s
}

#[derive(Clone, Debug)]
struct Row<T> {
    cur: T,
    orig: Option<T>,
}

impl<T: Clone> Row<T> {
    fn loaded(v: T) -> Self {
        Self { cur: v.clone(), orig: Some(v) }
    }
    fn new(v: T) -> Self {
        Self { cur: v, orig: None }
    }
}

/// Aktueller Stand plus Ursprungszustand einer Tabelle.
#[derive(Clone, Debug, Default)]
struct Design {
    /// `None` = neue Tabelle.
    table: Option<String>,
    /// Name für neue Tabellen.
    name: String,
    fields: Vec<FieldRow>,
    orig_fields: Vec<Field>,
    orig_pk: Vec<String>,
    indexes: Vec<Row<Index>>,
    orig_indexes: Vec<Index>,
    fks: Vec<Row<Fk>>,
    orig_fks: Vec<Fk>,
    triggers: Vec<Row<Trigger>>,
    orig_triggers: Vec<Trigger>,
    opts: TableOpts,
    orig_opts: TableOpts,
    part: Partitioning,
    orig_part: Partitioning,
}

/// Zerlegt z. B. `int(10) unsigned zerofill` → (typ, länge, dezimal, unsigned, zerofill).
fn parse_type(ct: &str) -> (String, String, String, bool, bool) {
    let (head, inner, suffix) = match (ct.find('('), ct.rfind(')')) {
        (Some(a), Some(b)) if b > a => (&ct[..a], &ct[a + 1..b], &ct[b + 1..]),
        _ => {
            let mut it = ct.splitn(2, ' ');
            (it.next().unwrap_or(""), "", it.next().unwrap_or(""))
        }
    };
    let typ = head.trim().to_lowercase();
    let suffix = suffix.to_lowercase();
    let (len, dec) = if matches!(typ.as_str(), "enum" | "set") {
        (inner.to_string(), String::new())
    } else {
        let mut it = inner.splitn(2, ',');
        (it.next().unwrap_or("").trim().to_string(), it.next().unwrap_or("").trim().to_string())
    };
    (typ, len, dec, suffix.contains("unsigned"), suffix.contains("zerofill"))
}

/// Standardwert als SQL: Ausdrücke/Literale unverändert, bloßer Text wird gequotet.
fn default_sql(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    let up = t.to_uppercase();
    let is_expr = up == "NULL"
        || t.parse::<f64>().is_ok()
        || t.starts_with('\'')
        || t.starts_with('"')
        || t.starts_with('(')
        || up.starts_with("B'")
        || up.starts_with("X'")
        || up.starts_with("0X")
        || matches!(up.as_str(), "CURRENT_TIMESTAMP" | "CURRENT_DATE" | "CURRENT_TIME" | "LOCALTIME" | "LOCALTIMESTAMP" | "NOW()" | "TRUE" | "FALSE")
        || (t.ends_with(')') && t.find('(').is_some_and(|p| t[..p].chars().all(|c| c.is_ascii_alphanumeric() || c == '_')));
    Some(if is_expr { t.to_string() } else { qs(t) })
}

fn type_sql(f: &Field) -> String {
    let t = f.typ.trim();
    let len = f.len.trim();
    let dec = f.dec.trim();
    if len.is_empty() || !takes_len(t) {
        t.to_string()
    } else if !dec.is_empty() && takes_dec(t) {
        format!("{t}({len},{dec})")
    } else {
        format!("{t}({len})")
    }
}

fn col_sql(f: &Field) -> String {
    let mut s = format!("{} {}", qi(f.name.trim()), type_sql(f));
    if f.unsigned {
        s.push_str(" UNSIGNED");
    }
    if f.zerofill {
        s.push_str(" ZEROFILL");
    }
    if !f.charset.is_empty() {
        s.push_str(&format!(" CHARACTER SET {}", f.charset));
    }
    if !f.collation.is_empty() {
        s.push_str(&format!(" COLLATE {}", f.collation));
    }
    if !f.gen_expr.trim().is_empty() {
        let kind = if f.gen_kind.is_empty() { "VIRTUAL" } else { &f.gen_kind };
        s.push_str(&format!(" AS ({}) {kind}", f.gen_expr.trim()));
    } else {
        s.push_str(if f.nullable { " NULL" } else { " NOT NULL" });
        if let Some(d) = default_sql(&f.default) {
            s.push_str(&format!(" DEFAULT {d}"));
        }
        if f.on_update {
            let prec = f.len.trim();
            if prec.is_empty() {
                s.push_str(" ON UPDATE CURRENT_TIMESTAMP");
            } else {
                s.push_str(&format!(" ON UPDATE CURRENT_TIMESTAMP({prec})"));
            }
        }
        if f.auto_inc {
            s.push_str(" AUTO_INCREMENT");
        }
    }
    if !f.comment.is_empty() {
        s.push_str(&format!(" COMMENT {}", qs(&f.comment)));
    }
    s
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

fn col_base(x: &str) -> &str {
    x.split('(').next().unwrap_or(x).trim()
}

/// Feldliste wie `a, b(10)` → `` `a`, `b`(10) ``.
fn cols_sql(s: &str) -> String {
    split_list(s)
        .iter()
        .map(|c| match (c.find('('), c.ends_with(')')) {
            (Some(p), true) => format!("{}{}", qi(c[..p].trim()), &c[p..]),
            _ => qi(c),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn toggle_in_list(list: &mut String, name: &str) {
    let mut v = split_list(list);
    if let Some(p) = v.iter().position(|x| col_base(x) == name) {
        v.remove(p);
    } else {
        v.push(name.to_string());
    }
    *list = v.join(", ");
}

fn index_sql(ix: &Index) -> String {
    let kind = match ix.kind.as_str() {
        "UNIQUE" => "UNIQUE INDEX",
        "FULLTEXT" => "FULLTEXT INDEX",
        "SPATIAL" => "SPATIAL INDEX",
        _ => "INDEX",
    };
    let mut s = format!("{kind} {} ({})", qi(ix.name.trim()), cols_sql(&ix.cols));
    if !ix.method.is_empty() {
        s.push_str(&format!(" USING {}", ix.method));
    }
    if !ix.comment.is_empty() {
        s.push_str(&format!(" COMMENT {}", qs(&ix.comment)));
    }
    s
}

fn fk_sql(fk: &Fk, db: &str) -> String {
    let mut s = String::new();
    if !fk.name.trim().is_empty() {
        s.push_str(&format!("CONSTRAINT {} ", qi(fk.name.trim())));
    }
    let target = if fk.ref_db.trim().is_empty() || fk.ref_db.trim() == db {
        qi(fk.ref_table.trim())
    } else {
        format!("{}.{}", qi(fk.ref_db.trim()), qi(fk.ref_table.trim()))
    };
    s.push_str(&format!("FOREIGN KEY ({}) REFERENCES {target} ({})", cols_sql(&fk.cols), cols_sql(&fk.ref_cols)));
    if !fk.on_delete.is_empty() {
        s.push_str(&format!(" ON DELETE {}", fk.on_delete));
    }
    if !fk.on_update.is_empty() {
        s.push_str(&format!(" ON UPDATE {}", fk.on_update));
    }
    s
}

fn trigger_sql(t: &Trigger, table: &str) -> String {
    format!("CREATE TRIGGER {} {} {} ON {} FOR EACH ROW {}", qi(t.name.trim()), t.timing, t.event, qi(table), t.body.trim())
}

/// Tabellenoptionen; mit `orig` nur die geänderten.
fn opts_sql(cur: &TableOpts, orig: Option<&TableOpts>, charset_of: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let changed = |a: &str, b: fn(&TableOpts) -> &str| orig.map_or(!a.is_empty(), |o| a != b(o));
    let mut v = Vec::new();
    if changed(&cur.engine, |o| &o.engine) && !cur.engine.is_empty() {
        v.push(format!("ENGINE={}", cur.engine));
    }
    if changed(&cur.collation, |o| &o.collation) && !cur.collation.is_empty() {
        match charset_of(&cur.collation) {
            Some(cs) => v.push(format!("DEFAULT CHARACTER SET {cs} COLLATE {}", cur.collation)),
            None => v.push(format!("COLLATE {}", cur.collation)),
        }
    }
    if changed(&cur.auto_inc, |o| &o.auto_inc) && cur.auto_inc.trim().parse::<u64>().is_ok() {
        v.push(format!("AUTO_INCREMENT={}", cur.auto_inc.trim()));
    }
    if changed(&cur.row_format, |o| &o.row_format) {
        v.push(format!("ROW_FORMAT={}", if cur.row_format.is_empty() { "DEFAULT" } else { &cur.row_format }));
    }
    if changed(&cur.comment, |o| &o.comment) {
        v.push(format!("COMMENT={}", qs(&cur.comment)));
    }
    v
}

/// Geänderte oder entfernte Ursprungs-Einträge (müssen gelöscht werden).
fn removed<'a, T: PartialEq>(orig: &'a [T], rows: &[Row<T>]) -> Vec<&'a T> {
    orig.iter()
        .filter(|o| !rows.iter().any(|r| r.orig.as_ref() == Some(*o) && r.cur == **o))
        .collect()
}

/// Neue oder geänderte Einträge (müssen angelegt werden).
fn added<T: PartialEq>(rows: &[Row<T>]) -> Vec<&T> {
    rows.iter().filter(|r| r.orig.as_ref() != Some(&r.cur)).map(|r| &r.cur).collect()
}

impl Design {
    fn new_table() -> Self {
        Self {
            fields: vec![FieldRow { f: Field { typ: "int".into(), len: "11".into(), ..Default::default() }, orig: None, pk: None }],
            opts: TableOpts { engine: "InnoDB".into(), ..Default::default() },
            ..Default::default()
        }
    }

    fn pk_rows(&self) -> Vec<&FieldRow> {
        let mut v: Vec<&FieldRow> = self.fields.iter().filter(|r| r.pk.is_some()).collect();
        v.sort_by_key(|r| r.pk);
        v
    }

    fn validate(&self) -> Result<(), String> {
        if self.fields.is_empty() {
            return Err("Die Tabelle braucht mindestens ein Feld.".into());
        }
        let mut seen = HashSet::new();
        for r in &self.fields {
            let n = r.f.name.trim();
            if n.is_empty() {
                return Err("Ein Feld hat keinen Namen.".into());
            }
            if r.f.typ.trim().is_empty() {
                return Err(format!("Feld „{n}“ hat keinen Typ."));
            }
            if !seen.insert(n.to_lowercase()) {
                return Err(format!("Feldname „{n}“ ist doppelt."));
            }
        }
        for ix in &self.indexes {
            if ix.cur.name.trim().is_empty() || split_list(&ix.cur.cols).is_empty() {
                return Err("Jeder Index braucht einen Namen und mindestens ein Feld.".into());
            }
        }
        for fk in &self.fks {
            let f = &fk.cur;
            if split_list(&f.cols).is_empty() || f.ref_table.trim().is_empty() || split_list(&f.ref_cols).is_empty() {
                return Err("Jeder Fremdschlüssel braucht Felder, Referenztabelle und Referenzfelder.".into());
            }
        }
        let p = &self.part;
        if !p.method.is_empty() && !p.readonly {
            let keyish = p.method.ends_with("KEY");
            if p.expr.trim().is_empty() && !keyish {
                return Err("Partitionierung: Bitte einen Ausdruck bzw. Spalten angeben.".into());
            }
            if part_explicit(&p.method) {
                if p.parts.is_empty() {
                    return Err("Partitionierung: RANGE/LIST braucht mindestens eine Partition.".into());
                }
                if p.parts.iter().any(|x| x.name.trim().is_empty() || x.values.trim().is_empty()) {
                    return Err("Partitionierung: Jede Partition braucht Name und Werte.".into());
                }
            } else if !p.count.trim().is_empty() && p.count.trim().parse::<u32>().map_or(true, |n| n == 0) {
                return Err("Partitionierung: Die Anzahl muss eine positive Zahl sein.".into());
            }
        }
        for t in &self.triggers {
            if t.cur.name.trim().is_empty() || t.cur.body.trim().is_empty() {
                return Err("Jeder Trigger braucht einen Namen und eine Definition.".into());
            }
        }
        Ok(())
    }

    /// Erzeugt die SQL-Anweisungen für alle Änderungen (leer = nichts geändert).
    fn sql(&self, db: &str, charset_of: &dyn Fn(&str) -> Option<String>) -> Result<Vec<String>, String> {
        self.validate()?;
        let pk: Vec<String> = self.pk_rows().iter().map(|r| qi(r.f.name.trim())).collect();
        let mut out = Vec::new();
        let Some(table) = &self.table else {
            let name = self.name.trim();
            if name.is_empty() {
                return Err("Bitte einen Tabellennamen angeben.".into());
            }
            let mut parts: Vec<String> = self.fields.iter().map(|r| col_sql(&r.f)).collect();
            if !pk.is_empty() {
                parts.push(format!("PRIMARY KEY ({})", pk.join(", ")));
            }
            parts.extend(self.indexes.iter().map(|i| index_sql(&i.cur)));
            parts.extend(self.fks.iter().map(|f| fk_sql(&f.cur, db)));
            let mut s = format!("CREATE TABLE {} (\n  {}\n)", qi(name), parts.join(",\n  "));
            for o in opts_sql(&self.opts, None, charset_of) {
                s.push(' ');
                s.push_str(&o);
            }
            if !self.part.method.is_empty() {
                s.push('\n');
                s.push_str(&partition_sql(&self.part));
            }
            out.push(s);
            out.extend(self.triggers.iter().map(|t| trigger_sql(&t.cur, name)));
            return Ok(out);
        };

        let t = qi(table);
        // 1. Geänderte/entfernte Fremdschlüssel zuerst löschen (eigene Anweisung,
        //    damit sie unter gleichem Namen neu angelegt werden können).
        let drop_fk: Vec<String> = removed(&self.orig_fks, &self.fks)
            .iter()
            .map(|f| format!("DROP FOREIGN KEY {}", qi(&f.name)))
            .collect();
        if !drop_fk.is_empty() {
            out.push(format!("ALTER TABLE {t} {}", drop_fk.join(", ")));
        }

        // 2. Hauptänderung
        let mut cl: Vec<String> = Vec::new();
        for ix in removed(&self.orig_indexes, &self.indexes) {
            cl.push(format!("DROP INDEX {}", qi(&ix.name)));
        }
        let cur_pk: Vec<Option<&str>> = self.pk_rows().iter().map(|r| r.orig.as_deref()).collect();
        let orig_pk: Vec<Option<&str>> = self.orig_pk.iter().map(|s| Some(s.as_str())).collect();
        let pk_changed = cur_pk != orig_pk;
        if pk_changed && !self.orig_pk.is_empty() {
            cl.push("DROP PRIMARY KEY".into());
        }
        let present: HashSet<&str> = self.fields.iter().filter_map(|r| r.orig.as_deref()).collect();
        for f in &self.orig_fields {
            if !present.contains(f.name.as_str()) {
                cl.push(format!("DROP COLUMN {}", qi(&f.name)));
            }
        }
        // Reihenfolge der erhaltenen Spalten: vorher / jetzt
        let kept: Vec<&str> = self.orig_fields.iter().map(|f| f.name.as_str()).filter(|n| present.contains(n)).collect();
        let cur_existing: Vec<&str> = self.fields.iter().filter_map(|r| r.orig.as_deref()).collect();
        for (i, r) in self.fields.iter().enumerate() {
            let pos = if i == 0 { " FIRST".to_string() } else { format!(" AFTER {}", qi(self.fields[i - 1].f.name.trim())) };
            match &r.orig {
                None => cl.push(format!("ADD COLUMN {}{pos}", col_sql(&r.f))),
                Some(o) => {
                    let changed = self.orig_fields.iter().find(|f| &f.name == o) != Some(&r.f);
                    let k = cur_existing.iter().position(|n| n == o).unwrap_or(0);
                    let j = kept.iter().position(|n| n == o).unwrap_or(0);
                    let pred_cur = k.checked_sub(1).map(|k| cur_existing[k]);
                    let pred_orig = j.checked_sub(1).map(|j| kept[j]);
                    let moved = pred_cur != pred_orig;
                    if changed || moved {
                        cl.push(format!("CHANGE COLUMN {} {}{}", qi(o), col_sql(&r.f), if moved { pos.as_str() } else { "" }));
                    }
                }
            }
        }
        if pk_changed && !pk.is_empty() {
            cl.push(format!("ADD PRIMARY KEY ({})", pk.join(", ")));
        }
        for ix in added(&self.indexes) {
            cl.push(format!("ADD {}", index_sql(ix)));
        }
        for fk in added(&self.fks) {
            cl.push(format!("ADD {}", fk_sql(fk, db)));
        }
        cl.extend(opts_sql(&self.opts, Some(&self.orig_opts), charset_of));
        if !cl.is_empty() {
            out.push(format!("ALTER TABLE {t}\n  {}", cl.join(",\n  ")));
        }

        // 3. Partitionierung (eigene Anweisung, baut die Tabelle neu auf)
        if self.part != self.orig_part && !self.part.readonly && !self.orig_part.readonly {
            if self.part.method.is_empty() {
                out.push(format!("ALTER TABLE {t} REMOVE PARTITIONING"));
            } else {
                out.push(format!("ALTER TABLE {t} {}", partition_sql(&self.part)));
            }
        }

        // 4. Trigger
        for tr in removed(&self.orig_triggers, &self.triggers) {
            out.push(format!("DROP TRIGGER {}.{}", qi(db), qi(&tr.name)));
        }
        for tr in added(&self.triggers) {
            out.push(trigger_sql(tr, table));
        }
        Ok(out)
    }

    /// Baut den Entwurf aus den Ergebnissen von `load_stmts` (Sets 0–4 und 8).
    fn from_result(res: &JobResult, db: &str, table: &str) -> Self {
        let empty = ResultSet::default();
        let set = |n| res.set(n).unwrap_or(&empty);
        let mut d = Design { table: Some(table.to_string()), ..Default::default() };

        let cols = set(0);
        for r in 0..cols.rows.len() {
            let (typ, len, dec, unsigned, zerofill) = parse_type(cols.get(r, "COLUMN_TYPE"));
            let extra = cols.get(r, "EXTRA").to_lowercase();
            let gen_expr = cols.get(r, "GENERATION_EXPRESSION").to_string();
            let gen_kind = if gen_expr.is_empty() {
                String::new()
            } else if extra.contains("virtual") {
                "VIRTUAL".into()
            } else {
                "PERSISTENT".into()
            };
            let default = if gen_expr.is_empty() { opt(cols, r, "COLUMN_DEFAULT").unwrap_or_default() } else { String::new() };
            d.orig_fields.push(Field {
                name: cols.get(r, "COLUMN_NAME").into(),
                typ,
                len,
                dec,
                nullable: cols.get(r, "IS_NULLABLE") == "YES",
                default,
                comment: cols.get(r, "COLUMN_COMMENT").into(),
                auto_inc: extra.contains("auto_increment"),
                unsigned,
                zerofill,
                on_update: extra.contains("on update"),
                charset: cols.get(r, "CHARACTER_SET_NAME").into(),
                collation: cols.get(r, "COLLATION_NAME").into(),
                gen_expr,
                gen_kind,
            });
        }

        let st = set(1);
        let mut idx: Vec<Index> = Vec::new();
        for r in 0..st.rows.len() {
            let name = st.get(r, "INDEX_NAME");
            let col = st.get(r, "COLUMN_NAME");
            let spec = match opt(st, r, "SUB_PART") {
                Some(n) if !n.is_empty() => format!("{col}({n})"),
                _ => col.to_string(),
            };
            if name == "PRIMARY" {
                d.orig_pk.push(col.to_string());
                continue;
            }
            let it = st.get(r, "INDEX_TYPE").to_uppercase();
            let pos = match idx.iter().position(|i| i.name == name) {
                Some(p) => p,
                None => {
                    idx.push(Index {
                        name: name.to_string(),
                        cols: String::new(),
                        kind: if it == "FULLTEXT" || it == "SPATIAL" {
                            it.clone()
                        } else if st.get(r, "NON_UNIQUE") == "0" {
                            "UNIQUE".into()
                        } else {
                            "INDEX".into()
                        },
                        method: if it == "BTREE" || it == "HASH" { it.clone() } else { String::new() },
                        comment: st.get(r, "INDEX_COMMENT").into(),
                    });
                    idx.len() - 1
                }
            };
            let c = &mut idx[pos].cols;
            if !c.is_empty() {
                c.push_str(", ");
            }
            c.push_str(&spec);
        }
        d.orig_indexes = idx;

        let fk = set(2);
        let mut fks: Vec<Fk> = Vec::new();
        for r in 0..fk.rows.len() {
            let name = fk.get(r, "CONSTRAINT_NAME");
            let pos = match fks.iter().position(|f| f.name == name) {
                Some(p) => p,
                None => {
                    let rdb = fk.get(r, "REFERENCED_TABLE_SCHEMA");
                    fks.push(Fk {
                        name: name.to_string(),
                        ref_db: if rdb == db { String::new() } else { rdb.to_string() },
                        ref_table: fk.get(r, "REFERENCED_TABLE_NAME").into(),
                        on_delete: fk.get(r, "DELETE_RULE").into(),
                        on_update: fk.get(r, "UPDATE_RULE").into(),
                        ..Default::default()
                    });
                    fks.len() - 1
                }
            };
            let f = &mut fks[pos];
            for (list, col) in [(&mut f.cols, "COLUMN_NAME"), (&mut f.ref_cols, "REFERENCED_COLUMN_NAME")] {
                if !list.is_empty() {
                    list.push_str(", ");
                }
                list.push_str(fk.get(r, col));
            }
        }
        d.orig_fks = fks;

        let tr = set(3);
        d.orig_triggers = (0..tr.rows.len())
            .map(|r| Trigger {
                name: tr.get(r, "TRIGGER_NAME").into(),
                timing: tr.get(r, "ACTION_TIMING").into(),
                event: tr.get(r, "EVENT_MANIPULATION").into(),
                body: tr.get(r, "ACTION_STATEMENT").into(),
            })
            .collect();

        let tb = set(4);
        if !tb.rows.is_empty() {
            let co = tb.get(0, "CREATE_OPTIONS").to_lowercase();
            let row_format = co
                .split_whitespace()
                .find_map(|p| p.strip_prefix("row_format="))
                .map(|s| s.to_uppercase())
                .unwrap_or_default();
            d.orig_opts = TableOpts {
                engine: tb.get(0, "ENGINE").into(),
                collation: tb.get(0, "TABLE_COLLATION").into(),
                auto_inc: tb.get(0, "AUTO_INCREMENT").into(),
                row_format,
                comment: tb.get(0, "TABLE_COMMENT").into(),
            };
        }

        d.fields = d
            .orig_fields
            .iter()
            .map(|f| FieldRow {
                f: f.clone(),
                orig: Some(f.name.clone()),
                pk: d.orig_pk.iter().position(|p| *p == f.name).map(|p| p as u32 + 1),
            })
            .collect();
        d.indexes = d.orig_indexes.iter().cloned().map(Row::loaded).collect();
        d.fks = d.orig_fks.iter().cloned().map(Row::loaded).collect();
        d.triggers = d.orig_triggers.iter().cloned().map(Row::loaded).collect();
        d.orig_part = parse_partitions(set(8));
        d.part = d.orig_part.clone();
        d.opts = d.orig_opts.clone();
        d
    }
}

/// Partitionierung aus `information_schema.PARTITIONS` (eine Zeile je (Sub-)Partition).
fn parse_partitions(rs: &ResultSet) -> Partitioning {
    if rs.rows.is_empty() {
        return Partitioning::default();
    }
    let method = rs.get(0, "PARTITION_METHOD").to_uppercase();
    let mut p = Partitioning {
        readonly: !PART_METHODS.contains(&method.as_str()),
        expr: rs.get(0, "PARTITION_EXPRESSION").to_string(),
        sub_method: rs.get(0, "SUBPARTITION_METHOD").to_uppercase(),
        sub_expr: rs.get(0, "SUBPARTITION_EXPRESSION").to_string(),
        method,
        ..Default::default()
    };
    for r in 0..rs.rows.len() {
        let name = rs.get(r, "PARTITION_NAME");
        if p.parts.last().is_some_and(|x| x.name == name) {
            continue;
        }
        p.parts.push(Partition {
            name: name.to_string(),
            values: rs.get(r, "PARTITION_DESCRIPTION").to_string(),
            comment: rs.get(r, "PARTITION_COMMENT").to_string(),
        });
    }
    if !p.sub_method.is_empty() && !p.parts.is_empty() {
        p.sub_count = (rs.rows.len() / p.parts.len()).to_string();
    }
    if !part_explicit(&p.method) {
        p.count = p.parts.len().to_string();
        p.parts.clear();
    }
    p
}

fn opt(rs: &ResultSet, r: usize, name: &str) -> Option<String> {
    rs.col(name).and_then(|c| rs.rows.get(r).and_then(|row| row[c].clone()))
}

fn load_stmts(db: &str, table: &str) -> Vec<Stmt> {
    let p = || vec![Some(db.to_string()), Some(table.to_string())];
    vec![
        Stmt::with(
            "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, EXTRA, COLUMN_COMMENT, \
             CHARACTER_SET_NAME, COLLATION_NAME, GENERATION_EXPRESSION \
             FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
            p(),
        ),
        Stmt::with(
            "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME, SUB_PART, INDEX_TYPE, INDEX_COMMENT \
             FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY INDEX_NAME, SEQ_IN_INDEX",
            p(),
        ),
        Stmt::with(
            "SELECT k.CONSTRAINT_NAME, k.COLUMN_NAME, k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME, \
             k.REFERENCED_COLUMN_NAME, r.UPDATE_RULE, r.DELETE_RULE \
             FROM information_schema.KEY_COLUMN_USAGE k JOIN information_schema.REFERENTIAL_CONSTRAINTS r \
             ON r.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND r.CONSTRAINT_NAME = k.CONSTRAINT_NAME AND r.TABLE_NAME = k.TABLE_NAME \
             WHERE k.TABLE_SCHEMA = ? AND k.TABLE_NAME = ? AND k.REFERENCED_TABLE_NAME IS NOT NULL \
             ORDER BY k.CONSTRAINT_NAME, k.ORDINAL_POSITION",
            p(),
        ),
        Stmt::with(
            "SELECT TRIGGER_NAME, ACTION_TIMING, EVENT_MANIPULATION, ACTION_STATEMENT \
             FROM information_schema.TRIGGERS WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ? \
             ORDER BY ACTION_TIMING, EVENT_MANIPULATION, ACTION_ORDER",
            p(),
        ),
        Stmt::with(
            "SELECT ENGINE, TABLE_COLLATION, AUTO_INCREMENT, TABLE_COMMENT, CREATE_OPTIONS \
             FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
            p(),
        ),
        Stmt::plain("SHOW ENGINES"),
        Stmt::plain("SHOW COLLATION"),
        Stmt::with(
            "SELECT c.TABLE_NAME, c.COLUMN_NAME FROM information_schema.COLUMNS c \
             JOIN information_schema.TABLES t ON t.TABLE_SCHEMA = c.TABLE_SCHEMA AND t.TABLE_NAME = c.TABLE_NAME \
             WHERE c.TABLE_SCHEMA = ? AND t.TABLE_TYPE = 'BASE TABLE' ORDER BY c.TABLE_NAME, c.ORDINAL_POSITION",
            vec![Some(db.to_string())],
        ),
        Stmt::with(
            "SELECT PARTITION_NAME, SUBPARTITION_NAME, PARTITION_METHOD, SUBPARTITION_METHOD, PARTITION_EXPRESSION, \
             SUBPARTITION_EXPRESSION, PARTITION_DESCRIPTION, PARTITION_COMMENT \
             FROM information_schema.PARTITIONS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND PARTITION_NAME IS NOT NULL \
             ORDER BY PARTITION_ORDINAL_POSITION, SUBPARTITION_ORDINAL_POSITION",
            p(),
        ),
    ]
}

// ------------------------------------------------------------------ Tab

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sub {
    Fields,
    Indexes,
    Fks,
    Triggers,
    Options,
    Partitions,
    Comment,
    Sql,
}

impl Sub {
    const ALL: [Sub; 8] = [Sub::Fields, Sub::Indexes, Sub::Fks, Sub::Triggers, Sub::Options, Sub::Partitions, Sub::Comment, Sub::Sql];
    fn label(self) -> &'static str {
        match self {
            Sub::Fields => "Felder",
            Sub::Indexes => "Indizes",
            Sub::Fks => "Fremdschlüssel",
            Sub::Triggers => "Trigger",
            Sub::Options => "Optionen",
            Sub::Partitions => "Partitionierung",
            Sub::Comment => "Kommentar",
            Sub::Sql => "SQL-Vorschau",
        }
    }
}

pub struct DesignTab {
    pub db: String,
    worker: Worker,
    d: Design,
    tag_load: u64,
    tag_save: u64,
    loading: bool,
    saving: bool,
    sub: Sub,
    sel_field: Option<usize>,
    sel_index: Option<usize>,
    sel_fk: Option<usize>,
    sel_trg: Option<usize>,
    sel_part: Option<usize>,
    engines: Vec<String>,
    /// (Kollation, Zeichensatz, ist Standard des Zeichensatzes)
    collations: Vec<(String, String, bool)>,
    charsets: Vec<String>,
    schema_cols: HashMap<String, Vec<String>>,
    tables: Vec<String>,
    error: Option<String>,
    status: String,
    saved: bool,
}

impl DesignTab {
    pub fn new(cfg: &ConnConfig, ctx: &egui::Context, db: &str, table: Option<&str>) -> Self {
        let mut t = Self {
            db: db.to_string(),
            worker: Worker::spawn(cfg.clone(), ctx.clone()),
            d: match table {
                Some(n) => Design { table: Some(n.to_string()), ..Default::default() },
                None => Design::new_table(),
            },
            tag_load: 0,
            tag_save: 0,
            loading: false,
            saving: false,
            sub: Sub::Fields,
            sel_field: Some(0),
            sel_index: None,
            sel_fk: None,
            sel_trg: None,
            sel_part: None,
            engines: Vec::new(),
            collations: Vec::new(),
            charsets: Vec::new(),
            schema_cols: HashMap::new(),
            tables: Vec::new(),
            error: None,
            status: String::new(),
            saved: false,
        };
        t.reload();
        t
    }

    /// Name der Tabelle (`None` solange eine neue Tabelle noch nicht gespeichert ist).
    pub fn table(&self) -> Option<&str> {
        self.d.table.as_deref()
    }

    pub fn title(&self) -> String {
        let name = match &self.d.table {
            Some(t) => t.clone(),
            None if self.d.name.trim().is_empty() => "Neue Tabelle".into(),
            None => self.d.name.trim().to_string(),
        };
        let dirty = if self.dirty() { " •" } else { "" };
        format!("[Entwurf] {name}{dirty}")
    }

    /// Liefert einmalig `true` nach erfolgreichem Speichern (Objektliste neu laden).
    pub fn take_saved(&mut self) -> bool {
        std::mem::take(&mut self.saved)
    }

    fn dirty(&self) -> bool {
        if self.loading {
            return false;
        }
        !matches!(self.d.sql(&self.db, &|c| self.charset_of(c)), Ok(v) if v.is_empty())
    }

    fn charset_of(&self, coll: &str) -> Option<String> {
        self.collations.iter().find(|c| c.0 == coll).map(|c| c.1.clone())
    }

    fn reload(&mut self) {
        let table = self.d.table.clone().unwrap_or_default();
        self.tag_load = self.worker.send(None, load_stmts(&self.db, &table), Mode::StopOnError, usize::MAX);
        self.loading = true;
    }

    fn save(&mut self) {
        match self.d.sql(&self.db, &|c| self.charset_of(c)) {
            Err(e) => self.error = Some(e),
            Ok(v) if v.is_empty() => self.status = "Keine Änderungen.".into(),
            Ok(v) => {
                let stmts = v.into_iter().map(Stmt::plain).collect();
                self.tag_save = self.worker.send(Some(self.db.clone()), stmts, Mode::StopOnError, usize::MAX);
                self.saving = true;
                self.error = None;
                self.status = "Speichere …".into();
            }
        }
    }

    fn poll(&mut self) {
        while let Some(res) = self.worker.poll() {
            if res.tag == self.tag_load {
                self.loading = false;
                if let Some(e) = &res.error {
                    self.error = Some(format!("Laden fehlgeschlagen: {e}"));
                    continue;
                }
                self.apply_lists(&res);
                if let Some(t) = self.d.table.clone() {
                    self.d = Design::from_result(&res, &self.db, &t);
                    self.sel_field = if self.d.fields.is_empty() { None } else { Some(0) };
                    self.sel_index = None;
                    self.sel_fk = None;
                    self.sel_trg = None;
                    self.sel_part = None;
                }
            } else if res.tag == self.tag_save {
                self.saving = false;
                if let Some(e) = res.error {
                    self.status.clear();
                    self.error = Some(format!("Speichern fehlgeschlagen: {e}"));
                } else {
                    if self.d.table.is_none() {
                        self.d.table = Some(self.d.name.trim().to_string());
                    }
                    self.status = "Gespeichert.".into();
                    self.saved = true;
                    self.reload();
                }
            }
        }
    }

    fn apply_lists(&mut self, res: &JobResult) {
        if let Some(e) = res.set(5) {
            self.engines = (0..e.rows.len())
                .filter(|&r| matches!(e.get(r, "Support").to_uppercase().as_str(), "YES" | "DEFAULT"))
                .map(|r| e.get(r, "Engine").to_string())
                .collect();
            self.engines.sort();
        }
        if let Some(c) = res.set(6) {
            self.collations = (0..c.rows.len())
                .map(|r| (c.get(r, "Collation").to_string(), c.get(r, "Charset").to_string(), c.get(r, "Default") == "Yes"))
                .filter(|(n, cs, _)| !n.is_empty() && !cs.is_empty())
                .collect();
            self.collations.sort();
            let mut cs: Vec<String> = self.collations.iter().map(|c| c.1.clone()).collect();
            cs.sort();
            cs.dedup();
            self.charsets = cs;
        }
        if let Some(s) = res.set(7) {
            self.schema_cols.clear();
            for r in 0..s.rows.len() {
                self.schema_cols.entry(s.get(r, "TABLE_NAME").to_string()).or_default().push(s.get(r, "COLUMN_NAME").to_string());
            }
            let mut t: Vec<String> = self.schema_cols.keys().cloned().collect();
            t.sort();
            self.tables = t;
        }
    }

    fn toggle_pk(&mut self, i: usize) {
        let next = self.d.fields.iter().filter_map(|r| r.pk).max().unwrap_or(0) + 1;
        let r = &mut self.d.fields[i];
        if r.pk.is_some() {
            r.pk = None;
        } else {
            r.pk = Some(next);
            r.f.nullable = false;
        }
    }

    fn new_field() -> FieldRow {
        FieldRow { f: Field { typ: "varchar".into(), len: "255".into(), nullable: true, ..Default::default() }, orig: None, pk: None }
    }

    // ---------------------------------------------------------- UI

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.poll();
        let ctrl_s = ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::S));
        if ctrl_s && !self.saving && !self.loading {
            self.save();
        }

        ui.horizontal(|ui| {
            if ui.add_enabled(!self.saving && !self.loading, icons::button(Icon::Save, "Speichern")).on_hover_text("Strg+S").clicked() {
                self.save();
            }
            if self.d.table.is_none() {
                ui.label("Name:");
                ui.add(egui::TextEdit::singleline(&mut self.d.name).desired_width(160.0).hint_text("Tabellenname"));
            }
            ui.separator();
            self.toolbar(ui);
            ui.separator();
            if ui
                .add_enabled(self.d.table.is_some() && !self.loading, icons::button(Icon::Refresh, "Neu laden"))
                .on_hover_text("Änderungen verwerfen und neu laden")
                .clicked()
            {
                self.reload();
            }
            if self.loading || self.saving {
                ui.spinner();
            }
            ui.weak(&self.status);
        });

        if let Some(e) = &self.error {
            let mut close = false;
            egui::Frame::new().fill(ui.visuals().error_fg_color.gamma_multiply(0.15)).inner_margin(6.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(ui.visuals().error_fg_color, RichText::new(e).monospace());
                    if ui.small_button("✖").clicked() {
                        close = true;
                    }
                });
            });
            if close {
                self.error = None;
            }
        }

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for s in Sub::ALL {
                ui.selectable_value(&mut self.sub, s, s.label());
            }
        });
        ui.separator();

        match self.sub {
            Sub::Fields => self.ui_fields(ui),
            Sub::Indexes => self.ui_indexes(ui),
            Sub::Fks => self.ui_fks(ui),
            Sub::Triggers => self.ui_triggers(ui),
            Sub::Options => self.ui_options(ui),
            Sub::Partitions => self.ui_partitions(ui),
            Sub::Comment => {
                ui.add(
                    egui::TextEdit::multiline(&mut self.d.opts.comment)
                        .desired_width(f32::INFINITY)
                        .min_size(egui::vec2(0.0, ui.available_height()))
                        .hint_text("Tabellenkommentar"),
                );
            }
            Sub::Sql => self.ui_sql(ui),
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        match self.sub {
            Sub::Fields => {
                let sel = self.sel_field.filter(|&i| i < self.d.fields.len());
                if ui.add(icons::button(Icon::AddField, "Feld hinzufügen")).clicked() {
                    self.d.fields.push(Self::new_field());
                    self.sel_field = Some(self.d.fields.len() - 1);
                }
                if ui.add(icons::button(Icon::InsertField, "Feld einfügen")).clicked() {
                    let at = sel.unwrap_or(0);
                    self.d.fields.insert(at, Self::new_field());
                    self.sel_field = Some(at);
                }
                if ui.add_enabled(sel.is_some(), icons::button(Icon::DeleteField, "Feld löschen")).clicked() {
                    if let Some(i) = sel {
                        self.d.fields.remove(i);
                        self.sel_field = if self.d.fields.is_empty() { None } else { Some(i.min(self.d.fields.len() - 1)) };
                    }
                }
                ui.separator();
                if ui.add_enabled(sel.is_some(), icons::button(Icon::Key, "Primärschlüssel")).clicked() {
                    if let Some(i) = sel {
                        self.toggle_pk(i);
                    }
                }
                ui.separator();
                if ui.add_enabled(sel.is_some_and(|i| i > 0), icons::button(Icon::Up, "Nach oben")).clicked() {
                    if let Some(i) = sel {
                        self.d.fields.swap(i, i - 1);
                        self.sel_field = Some(i - 1);
                    }
                }
                if ui.add_enabled(sel.is_some_and(|i| i + 1 < self.d.fields.len()), icons::button(Icon::Down, "Nach unten")).clicked() {
                    if let Some(i) = sel {
                        self.d.fields.swap(i, i + 1);
                        self.sel_field = Some(i + 1);
                    }
                }
            }
            Sub::Indexes => {
                if ui.add(icons::button(Icon::Add, "Index hinzufügen")).clicked() {
                    let n = self.d.indexes.len() + 1;
                    self.d.indexes.push(Row::new(Index { name: format!("idx_{n}"), kind: "INDEX".into(), ..Default::default() }));
                    self.sel_index = Some(self.d.indexes.len() - 1);
                }
                let sel = self.sel_index.filter(|&i| i < self.d.indexes.len());
                if ui.add_enabled(sel.is_some(), icons::button(Icon::Delete, "Index löschen")).clicked() {
                    if let Some(i) = sel {
                        self.d.indexes.remove(i);
                        self.sel_index = None;
                    }
                }
            }
            Sub::Fks => {
                if ui.add(icons::button(Icon::Add, "Fremdschlüssel hinzufügen")).clicked() {
                    let base = self.d.table.clone().unwrap_or_else(|| self.d.name.trim().to_string());
                    let n = self.d.fks.len() + 1;
                    self.d.fks.push(Row::new(Fk {
                        name: format!("fk_{base}_{n}"),
                        on_delete: "RESTRICT".into(),
                        on_update: "RESTRICT".into(),
                        ..Default::default()
                    }));
                    self.sel_fk = Some(self.d.fks.len() - 1);
                }
                let sel = self.sel_fk.filter(|&i| i < self.d.fks.len());
                if ui.add_enabled(sel.is_some(), icons::button(Icon::Delete, "Fremdschlüssel löschen")).clicked() {
                    if let Some(i) = sel {
                        self.d.fks.remove(i);
                        self.sel_fk = None;
                    }
                }
            }
            Sub::Triggers => {
                if ui.add(icons::button(Icon::Add, "Trigger hinzufügen")).clicked() {
                    let base = self.d.table.clone().unwrap_or_else(|| self.d.name.trim().to_string());
                    let n = self.d.triggers.len() + 1;
                    self.d.triggers.push(Row::new(Trigger {
                        name: format!("trg_{base}_{n}"),
                        timing: "BEFORE".into(),
                        event: "INSERT".into(),
                        body: "BEGIN\n  \nEND".into(),
                    }));
                    self.sel_trg = Some(self.d.triggers.len() - 1);
                }
                let sel = self.sel_trg.filter(|&i| i < self.d.triggers.len());
                if ui.add_enabled(sel.is_some(), icons::button(Icon::Delete, "Trigger löschen")).clicked() {
                    if let Some(i) = sel {
                        self.d.triggers.remove(i);
                        self.sel_trg = None;
                    }
                }
            }
            Sub::Partitions => {
                let p = &mut self.d.part;
                let can = part_explicit(&p.method) && !p.readonly;
                if ui.add_enabled(can, icons::button(Icon::Add, "Partition hinzufügen")).clicked() {
                    let n = p.parts.len();
                    let values = if p.method.starts_with("RANGE") { "MAXVALUE" } else { "" };
                    p.parts.push(Partition { name: format!("p{n}"), values: values.into(), ..Default::default() });
                    self.sel_part = Some(p.parts.len() - 1);
                }
                let sel = self.sel_part.filter(|&i| i < p.parts.len());
                if ui.add_enabled(can && sel.is_some(), icons::button(Icon::Delete, "Partition löschen")).clicked() {
                    if let Some(i) = sel {
                        p.parts.remove(i);
                        self.sel_part = None;
                    }
                }
            }
            _ => {}
        }
    }

    fn ui_fields(&mut self, ui: &mut egui::Ui) {
        let detail_h = 200.0;
        let table_h = (ui.available_height() - detail_h).max(120.0);
        let mut toggle = None;
        {
            let fields = &mut self.d.fields;
            let sel = &mut self.sel_field;
            ui.allocate_ui(egui::vec2(ui.available_width(), table_h), |ui| {
                TableBuilder::new(ui)
                    .id_salt("design_fields")
                    .striped(true)
                    .resizable(true)
                    .auto_shrink([false, false])
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .column(Column::exact(14.0))
                    .column(Column::initial(180.0).at_least(60.0).clip(true))
                    .column(Column::initial(130.0).at_least(60.0).clip(true))
                    .column(Column::initial(80.0).at_least(40.0).clip(true))
                    .column(Column::initial(90.0).at_least(40.0).clip(true))
                    .column(Column::initial(80.0).at_least(40.0))
                    .column(Column::initial(60.0).at_least(30.0))
                    .header(22.0, |mut h| {
                        for t in ["", "Name", "Typ", "Länge", "Dezimalstellen", "NULL erlaubt", "Schlüssel"] {
                            h.col(|ui| {
                                ui.strong(t);
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(24.0, fields.len(), |mut row| {
                            let i = row.index();
                            row.set_selected(*sel == Some(i));
                            let r = &mut fields[i];
                            let mut pick = false;
                            let (_, m) = row.col(|ui| {
                                if *sel == Some(i) {
                                    ui.label("▶");
                                }
                            });
                            pick |= m.clicked();
                            row.col(|ui| {
                                let e = ui.add(egui::TextEdit::singleline(&mut r.f.name).desired_width(f32::INFINITY));
                                pick |= e.has_focus();
                            });
                            row.col(|ui| {
                                pick |= type_combo(ui, ("dtyp", i), &mut r.f).clicked();
                            });
                            let tl = takes_len(&r.f.typ);
                            row.col(|ui| {
                                pick |= ui.add_enabled(tl, egui::TextEdit::singleline(&mut r.f.len).desired_width(f32::INFINITY)).has_focus();
                            });
                            let td = takes_dec(&r.f.typ);
                            row.col(|ui| {
                                pick |= ui.add_enabled(td, egui::TextEdit::singleline(&mut r.f.dec).desired_width(f32::INFINITY)).has_focus();
                            });
                            row.col(|ui| {
                                pick |= ui.add_enabled(r.pk.is_none(), egui::Checkbox::without_text(&mut r.f.nullable)).clicked();
                            });
                            let (_, k) = row.col(|ui| {
                                let resp = ui.allocate_response(egui::vec2(ui.available_width(), 20.0), egui::Sense::click());
                                if r.pk.is_some() {
                                    let rect = egui::Rect::from_min_size(resp.rect.left_center() - egui::vec2(0.0, 8.0), egui::vec2(16.0, 16.0));
                                    icons::paint(ui.painter(), rect, Icon::Key, true);
                                }
                                if resp.on_hover_text("Primärschlüssel umschalten").clicked() {
                                    toggle = Some(i);
                                }
                            });
                            pick |= k.clicked();
                            if pick {
                                *sel = Some(i);
                            }
                        });
                    });
            });
        }
        if let Some(i) = toggle {
            self.toggle_pk(i);
            self.sel_field = Some(i);
        }
        ui.separator();

        let Some(i) = self.sel_field.filter(|&i| i < self.d.fields.len()) else {
            ui.weak("Feld auswählen, um Details zu bearbeiten.");
            return;
        };
        let charsets = &self.charsets;
        let collations = &self.collations;
        let f = &mut self.d.fields[i].f;
        let t = f.typ.clone();
        egui::ScrollArea::vertical().id_salt("design_detail").auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("design_detail_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Standardwert:");
                ui.horizontal(|ui| {
                    ui.add_enabled(
                        f.gen_expr.is_empty(),
                        egui::TextEdit::singleline(&mut f.default).desired_width(300.0).hint_text("z. B. 'Text', 0, NULL, CURRENT_TIMESTAMP"),
                    );
                    ui.menu_button("▾", |ui| {
                        for (label, v) in [
                            ("(kein Standardwert)", ""),
                            ("NULL", "NULL"),
                            ("Leere Zeichenkette", "''"),
                            ("0", "0"),
                            ("CURRENT_TIMESTAMP", "CURRENT_TIMESTAMP"),
                        ] {
                            if ui.button(label).clicked() {
                                f.default = v.into();
                                ui.close();
                            }
                        }
                    });
                });
                ui.end_row();
                ui.label("Kommentar:");
                ui.add(egui::TextEdit::singleline(&mut f.comment).desired_width(330.0));
                ui.end_row();
                if is_text(&t) {
                    ui.label("Zeichensatz:");
                    egui::ComboBox::from_id_salt("design_cs")
                        .selected_text(if f.charset.is_empty() { "(Tabellenstandard)" } else { f.charset.as_str() })
                        .width(220.0)
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(f.charset.is_empty(), "(Tabellenstandard)").clicked() {
                                f.charset.clear();
                                f.collation.clear();
                            }
                            for cs in charsets {
                                if ui.selectable_label(&f.charset == cs, cs.as_str()).clicked() {
                                    f.charset = cs.clone();
                                    f.collation = collations.iter().find(|c| &c.1 == cs && c.2).map(|c| c.0.clone()).unwrap_or_default();
                                }
                            }
                        });
                    ui.end_row();
                    ui.label("Sortierung:");
                    egui::ComboBox::from_id_salt("design_coll")
                        .selected_text(if f.collation.is_empty() { "(Standard)" } else { f.collation.as_str() })
                        .width(220.0)
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(f.collation.is_empty(), "(Standard)").clicked() {
                                f.collation.clear();
                            }
                            let cs = f.charset.clone();
                            for c in collations.iter().filter(|c| cs.is_empty() || c.1 == cs) {
                                if ui.selectable_label(f.collation == c.0, c.0.as_str()).clicked() {
                                    f.collation = c.0.clone();
                                    f.charset = c.1.clone();
                                }
                            }
                        });
                    ui.end_row();
                }
                ui.label("Generiert (Ausdruck):");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut f.gen_expr).desired_width(230.0).hint_text("leer = normale Spalte"));
                    egui::ComboBox::from_id_salt("design_gen").selected_text(f.gen_kind.as_str()).width(90.0).show_ui(ui, |ui| {
                        for k in GEN_KINDS {
                            ui.selectable_value(&mut f.gen_kind, k.to_string(), *k);
                        }
                    });
                });
                ui.end_row();
            });
            ui.add_space(6.0);
            if is_int(&t) {
                ui.checkbox(&mut f.auto_inc, "Automatische Nummerierung");
            }
            if is_numeric(&t) {
                ui.checkbox(&mut f.unsigned, "Ohne Vorzeichen");
                ui.checkbox(&mut f.zerofill, "Mit Nullen auffüllen");
            }
            if matches!(t.as_str(), "timestamp" | "datetime") {
                ui.checkbox(&mut f.on_update, "Bei Aktualisierung CURRENT_TIMESTAMP setzen");
            }
        });
    }

    fn ui_indexes(&mut self, ui: &mut egui::Ui) {
        let names: Vec<String> = self.d.fields.iter().map(|r| r.f.name.trim().to_string()).collect();
        let rows = &mut self.d.indexes;
        let sel = &mut self.sel_index;
        TableBuilder::new(ui)
            .id_salt("design_idx")
            .striped(true)
            .resizable(true)
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(14.0))
            .column(Column::initial(180.0).at_least(60.0).clip(true))
            .column(Column::initial(240.0).at_least(80.0).clip(true))
            .column(Column::initial(110.0).at_least(60.0))
            .column(Column::initial(90.0).at_least(60.0))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                for t in ["", "Name", "Felder", "Typ", "Methode", "Kommentar"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(24.0, rows.len(), |mut row| {
                    let i = row.index();
                    row.set_selected(*sel == Some(i));
                    let ix = &mut rows[i].cur;
                    let mut pick = false;
                    let (_, m) = row.col(|ui| {
                        if *sel == Some(i) {
                            ui.label("▶");
                        }
                    });
                    pick |= m.clicked();
                    row.col(|ui| {
                        pick |= ui.add(egui::TextEdit::singleline(&mut ix.name).desired_width(f32::INFINITY)).has_focus();
                    });
                    row.col(|ui| {
                        pick |= list_edit(ui, ("ixcols", i), &mut ix.cols, &names);
                    });
                    row.col(|ui| {
                        pick |= str_combo(ui, ("ixkind", i), &mut ix.kind, INDEX_KINDS);
                    });
                    row.col(|ui| {
                        pick |= str_combo(ui, ("ixmeth", i), &mut ix.method, INDEX_METHODS);
                    });
                    row.col(|ui| {
                        pick |= ui.add(egui::TextEdit::singleline(&mut ix.comment).desired_width(f32::INFINITY)).has_focus();
                    });
                    if pick {
                        *sel = Some(i);
                    }
                });
            });
    }

    fn ui_fks(&mut self, ui: &mut egui::Ui) {
        let names: Vec<String> = self.d.fields.iter().map(|r| r.f.name.trim().to_string()).collect();
        let own = self.d.table.clone().unwrap_or_else(|| self.d.name.trim().to_string());
        let mut tables = self.tables.clone();
        if !own.is_empty() && !tables.contains(&own) {
            tables.push(own.clone());
            tables.sort();
        }
        let schema_cols = &self.schema_cols;
        let rows = &mut self.d.fks;
        let sel = &mut self.sel_fk;
        TableBuilder::new(ui)
            .id_salt("design_fk")
            .striped(true)
            .resizable(true)
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(14.0))
            .column(Column::initial(160.0).at_least(60.0).clip(true))
            .column(Column::initial(160.0).at_least(60.0).clip(true))
            .column(Column::initial(110.0).at_least(60.0).clip(true))
            .column(Column::initial(150.0).at_least(60.0))
            .column(Column::initial(160.0).at_least(60.0).clip(true))
            .column(Column::initial(110.0).at_least(60.0))
            .column(Column::initial(110.0).at_least(60.0))
            .header(22.0, |mut h| {
                for t in ["", "Name", "Felder", "Ref. Datenbank", "Ref. Tabelle", "Ref. Felder", "Beim Löschen", "Beim Aktualisieren"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(24.0, rows.len(), |mut row| {
                    let i = row.index();
                    row.set_selected(*sel == Some(i));
                    let fk = &mut rows[i].cur;
                    let mut pick = false;
                    let (_, m) = row.col(|ui| {
                        if *sel == Some(i) {
                            ui.label("▶");
                        }
                    });
                    pick |= m.clicked();
                    row.col(|ui| {
                        pick |= ui.add(egui::TextEdit::singleline(&mut fk.name).desired_width(f32::INFINITY)).has_focus();
                    });
                    row.col(|ui| {
                        pick |= list_edit(ui, ("fkcols", i), &mut fk.cols, &names);
                    });
                    row.col(|ui| {
                        pick |= ui
                            .add(egui::TextEdit::singleline(&mut fk.ref_db).desired_width(f32::INFINITY).hint_text("(diese)"))
                            .has_focus();
                    });
                    row.col(|ui| {
                        let r = egui::ComboBox::from_id_salt(("fkref", i))
                            .selected_text(fk.ref_table.as_str())
                            .width(ui.available_width() - 4.0)
                            .show_ui(ui, |ui| {
                                for t in &tables {
                                    ui.selectable_value(&mut fk.ref_table, t.clone(), t.as_str());
                                }
                            });
                        pick |= r.response.clicked();
                    });
                    let ref_cols: Vec<String> = if fk.ref_table == own && fk.ref_db.trim().is_empty() {
                        names.clone()
                    } else {
                        schema_cols.get(&fk.ref_table).cloned().unwrap_or_default()
                    };
                    row.col(|ui| {
                        pick |= list_edit(ui, ("fkrcols", i), &mut fk.ref_cols, &ref_cols);
                    });
                    row.col(|ui| {
                        pick |= str_combo(ui, ("fkdel", i), &mut fk.on_delete, FK_RULES);
                    });
                    row.col(|ui| {
                        pick |= str_combo(ui, ("fkupd", i), &mut fk.on_update, FK_RULES);
                    });
                    if pick {
                        *sel = Some(i);
                    }
                });
            });
    }

    fn ui_triggers(&mut self, ui: &mut egui::Ui) {
        let list_h = (ui.available_height() * 0.35).max(100.0);
        {
            let rows = &mut self.d.triggers;
            let sel = &mut self.sel_trg;
            ui.allocate_ui(egui::vec2(ui.available_width(), list_h), |ui| {
                TableBuilder::new(ui)
                    .id_salt("design_trg")
                    .striped(true)
                    .resizable(true)
                    .auto_shrink([false, false])
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .column(Column::exact(14.0))
                    .column(Column::initial(220.0).at_least(60.0).clip(true))
                    .column(Column::initial(110.0).at_least(60.0))
                    .column(Column::initial(110.0).at_least(60.0))
                    .header(22.0, |mut h| {
                        for t in ["", "Name", "Zeitpunkt", "Ereignis"] {
                            h.col(|ui| {
                                ui.strong(t);
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(24.0, rows.len(), |mut row| {
                            let i = row.index();
                            row.set_selected(*sel == Some(i));
                            let t = &mut rows[i].cur;
                            let mut pick = false;
                            let (_, m) = row.col(|ui| {
                                if *sel == Some(i) {
                                    ui.label("▶");
                                }
                            });
                            pick |= m.clicked();
                            row.col(|ui| {
                                pick |= ui.add(egui::TextEdit::singleline(&mut t.name).desired_width(f32::INFINITY)).has_focus();
                            });
                            row.col(|ui| {
                                pick |= str_combo(ui, ("trgt", i), &mut t.timing, &["BEFORE", "AFTER"]);
                            });
                            row.col(|ui| {
                                pick |= str_combo(ui, ("trge", i), &mut t.event, &["INSERT", "UPDATE", "DELETE"]);
                            });
                            if pick {
                                *sel = Some(i);
                            }
                        });
                    });
            });
        }
        ui.separator();
        let Some(i) = self.sel_trg.filter(|&i| i < self.d.triggers.len()) else {
            ui.weak("Trigger auswählen, um die Definition zu bearbeiten.");
            return;
        };
        ui.label("Definition (FOR EACH ROW …):");
        egui::ScrollArea::vertical().id_salt("design_trg_body").auto_shrink([false, false]).show(ui, |ui| {
            let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, w: f32| sqlhl::layouter(ui, buf, w);
            ui.add(
                egui::TextEdit::multiline(&mut self.d.triggers[i].cur.body)
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .min_size(egui::vec2(0.0, ui.available_height()))
                    .layouter(&mut layouter),
            );
        });
    }

    fn ui_options(&mut self, ui: &mut egui::Ui) {
        let o = &mut self.d.opts;
        let collations = &self.collations;
        let cur_cs = collations.iter().find(|c| c.0 == o.collation).map(|c| c.1.clone()).unwrap_or_default();
        let mut engines = self.engines.clone();
        if !o.engine.is_empty() && !engines.contains(&o.engine) {
            engines.push(o.engine.clone());
        }
        egui::Grid::new("design_opts").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
            ui.label("Engine:");
            egui::ComboBox::from_id_salt("opt_engine").selected_text(o.engine.as_str()).width(220.0).show_ui(ui, |ui| {
                for e in &engines {
                    ui.selectable_value(&mut o.engine, e.clone(), e.as_str());
                }
            });
            ui.end_row();
            ui.label("Zeichensatz:");
            egui::ComboBox::from_id_salt("opt_cs")
                .selected_text(if cur_cs.is_empty() { "(Datenbankstandard)" } else { cur_cs.as_str() })
                .width(220.0)
                .show_ui(ui, |ui| {
                    for cs in &self.charsets {
                        if ui.selectable_label(&cur_cs == cs, cs.as_str()).clicked() {
                            o.collation = collations.iter().find(|c| &c.1 == cs && c.2).map(|c| c.0.clone()).unwrap_or_default();
                        }
                    }
                });
            ui.end_row();
            ui.label("Sortierung:");
            egui::ComboBox::from_id_salt("opt_coll")
                .selected_text(if o.collation.is_empty() { "(Datenbankstandard)" } else { o.collation.as_str() })
                .width(220.0)
                .show_ui(ui, |ui| {
                    for c in collations.iter().filter(|c| cur_cs.is_empty() || c.1 == cur_cs) {
                        ui.selectable_value(&mut o.collation, c.0.clone(), c.0.as_str());
                    }
                });
            ui.end_row();
            ui.label("Auto-Inkrement:");
            ui.add(egui::TextEdit::singleline(&mut o.auto_inc).desired_width(120.0));
            ui.end_row();
            ui.label("Zeilenformat:");
            egui::ComboBox::from_id_salt("opt_rowf")
                .selected_text(if o.row_format.is_empty() { "(Standard)" } else { o.row_format.as_str() })
                .width(220.0)
                .show_ui(ui, |ui| {
                    for f in ROW_FORMATS {
                        ui.selectable_value(&mut o.row_format, f.to_string(), if f.is_empty() { "(Standard)" } else { f });
                    }
                });
            ui.end_row();
        });
    }

    fn ui_partitions(&mut self, ui: &mut egui::Ui) {
        let p = &mut self.d.part;
        if p.readonly {
            ui.label(format!("Partitionierung „{}“ wird im Entwurf nicht unterstützt und bleibt unverändert.", p.method));
            ui.label(format!("Ausdruck: {}", p.expr));
            return;
        }
        egui::Grid::new("design_part").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
            ui.label("Partitionieren nach:");
            egui::ComboBox::from_id_salt("part_method")
                .selected_text(if p.method.is_empty() { "(keine Partitionierung)" } else { p.method.as_str() })
                .width(220.0)
                .show_ui(ui, |ui| {
                    for m in PART_METHODS {
                        ui.selectable_value(&mut p.method, m.to_string(), if m.is_empty() { "(keine Partitionierung)" } else { m });
                    }
                });
            ui.end_row();
            if p.method.is_empty() {
                return;
            }
            let cols = p.method.ends_with("COLUMNS") || p.method.ends_with("KEY");
            ui.label(if cols { "Spalten:" } else { "Ausdruck:" });
            ui.add(
                egui::TextEdit::singleline(&mut p.expr)
                    .desired_width(330.0)
                    .hint_text(if cols { "z. B. kunde_id, datum" } else { "z. B. YEAR(datum)" }),
            );
            ui.end_row();
            if part_explicit(&p.method) {
                ui.label("Subpartitionieren nach:");
                egui::ComboBox::from_id_salt("part_sub")
                    .selected_text(if p.sub_method.is_empty() { "(keine)" } else { p.sub_method.as_str() })
                    .width(220.0)
                    .show_ui(ui, |ui| {
                        for m in SUBPART_METHODS {
                            ui.selectable_value(&mut p.sub_method, m.to_string(), if m.is_empty() { "(keine)" } else { m });
                        }
                    });
                ui.end_row();
                if !p.sub_method.is_empty() {
                    ui.label("Subpartition-Ausdruck:");
                    ui.add(egui::TextEdit::singleline(&mut p.sub_expr).desired_width(330.0));
                    ui.end_row();
                    ui.label("Anzahl Subpartitionen:");
                    ui.add(egui::TextEdit::singleline(&mut p.sub_count).desired_width(80.0));
                    ui.end_row();
                }
            } else {
                ui.label("Anzahl Partitionen:");
                ui.add(egui::TextEdit::singleline(&mut p.count).desired_width(80.0));
                ui.end_row();
            }
        });
        ui.weak("Hinweis: Geänderte Partitionierung baut die Tabelle beim Speichern komplett neu auf.");
        if !part_explicit(&p.method) {
            return;
        }
        ui.separator();
        let range = p.method.starts_with("RANGE");
        let parts = &mut p.parts;
        let sel = &mut self.sel_part;
        TableBuilder::new(ui)
            .id_salt("design_parts")
            .striped(true)
            .resizable(true)
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(14.0))
            .column(Column::initial(160.0).at_least(60.0).clip(true))
            .column(Column::initial(260.0).at_least(80.0).clip(true))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                let v = if range { "Werte kleiner als" } else { "Werte (Liste)" };
                for t in ["", "Name", v, "Kommentar"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(24.0, parts.len(), |mut row| {
                    let i = row.index();
                    row.set_selected(*sel == Some(i));
                    let pt = &mut parts[i];
                    let mut pick = false;
                    let (_, m) = row.col(|ui| {
                        if *sel == Some(i) {
                            ui.label("▶");
                        }
                    });
                    pick |= m.clicked();
                    row.col(|ui| {
                        pick |= ui.add(egui::TextEdit::singleline(&mut pt.name).desired_width(f32::INFINITY)).has_focus();
                    });
                    row.col(|ui| {
                        let hint = if range { "z. B. 2024 oder MAXVALUE" } else { "z. B. 1, 2, 3" };
                        pick |= ui
                            .add(egui::TextEdit::singleline(&mut pt.values).desired_width(f32::INFINITY).hint_text(hint))
                            .has_focus();
                    });
                    row.col(|ui| {
                        pick |= ui.add(egui::TextEdit::singleline(&mut pt.comment).desired_width(f32::INFINITY)).has_focus();
                    });
                    if pick {
                        *sel = Some(i);
                    }
                });
            });
    }

    fn ui_sql(&mut self, ui: &mut egui::Ui) {
        let text = match self.d.sql(&self.db, &|c| self.charset_of(c)) {
            Ok(v) if v.is_empty() => "-- Keine Änderungen".to_string(),
            Ok(v) => v.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n\n"),
            Err(e) => format!("-- {e}"),
        };
        egui::ScrollArea::vertical().id_salt("design_sql").auto_shrink([false, false]).show(ui, |ui| {
            let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, w: f32| sqlhl::layouter(ui, buf, w);
            ui.add(
                egui::TextEdit::multiline(&mut text.as_str())
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .min_size(egui::vec2(0.0, ui.available_height()))
                    .layouter(&mut layouter),
            );
        });
    }
}

/// Typ-Auswahl; passt Länge/Dezimalstellen an den neuen Typ an.
fn type_combo(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, f: &mut Field) -> egui::Response {
    let mut changed = None;
    let r = egui::ComboBox::from_id_salt(id)
        .selected_text(f.typ.as_str())
        .width(ui.available_width() - 4.0)
        .height(400.0)
        .show_ui(ui, |ui| {
            for t in TYPES {
                if ui.selectable_label(f.typ == *t, *t).clicked() {
                    changed = Some(t.to_string());
                }
            }
        });
    if let Some(t) = changed {
        if t != f.typ {
            if !takes_len(&t) {
                f.len.clear();
            } else if matches!(t.as_str(), "varchar" | "varbinary") && f.len.is_empty() {
                f.len = "255".into();
            } else if matches!(t.as_str(), "enum" | "set") || matches!(f.typ.as_str(), "enum" | "set") {
                f.len.clear();
            }
            if !takes_dec(&t) {
                f.dec.clear();
            }
            if !is_numeric(&t) {
                f.unsigned = false;
                f.zerofill = false;
            }
            if !is_int(&t) {
                f.auto_inc = false;
            }
            if !is_text(&t) {
                f.charset.clear();
                f.collation.clear();
            }
            f.typ = t;
        }
    }
    r.response
}

/// Auswahl aus festen Werten; liefert `true` bei Klick.
fn str_combo(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, v: &mut String, opts: &[&str]) -> bool {
    egui::ComboBox::from_id_salt(id)
        .selected_text(v.as_str())
        .width(ui.available_width() - 4.0)
        .show_ui(ui, |ui| {
            for o in opts {
                ui.selectable_value(v, o.to_string(), *o);
            }
        })
        .response
        .clicked()
}

/// Kommagetrennte Feldliste mit Auswahlmenü; liefert `true` bei Interaktion.
fn list_edit(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, list: &mut String, names: &[String]) -> bool {
    let mut pick = false;
    ui.push_id(id, |ui| {
        let w = ui.available_width() - 28.0;
        pick |= ui.add(egui::TextEdit::singleline(list).desired_width(w.max(40.0))).has_focus();
        let r = ui.menu_button("…", |ui| {
            let cur = split_list(list);
            for n in names {
                let mut on = cur.iter().any(|c| col_base(c) == n);
                if ui.checkbox(&mut on, n.as_str()).clicked() {
                    toggle_in_list(list, n);
                }
            }
        });
        pick |= r.response.clicked();
    });
    pick
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_cs(_: &str) -> Option<String> {
        None
    }

    fn field(name: &str, typ: &str, len: &str) -> Field {
        Field { name: name.into(), typ: typ.into(), len: len.into(), ..Default::default() }
    }

    fn loaded(fields: Vec<Field>, pk: &[&str]) -> Design {
        let mut d = Design { table: Some("t".into()), orig_fields: fields.clone(), orig_pk: pk.iter().map(|s| s.to_string()).collect(), ..Default::default() };
        d.fields = fields
            .into_iter()
            .map(|f| {
                let pk = pk.iter().position(|p| *p == f.name).map(|p| p as u32 + 1);
                FieldRow { orig: Some(f.name.clone()), f, pk }
            })
            .collect();
        d
    }

    #[test]
    fn parse_column_types() {
        assert_eq!(parse_type("int(10) unsigned zerofill"), ("int".into(), "10".into(), "".into(), true, true));
        assert_eq!(parse_type("decimal(10,2)"), ("decimal".into(), "10".into(), "2".into(), false, false));
        assert_eq!(parse_type("enum('a','b)c')"), ("enum".into(), "'a','b)c'".into(), "".into(), false, false));
        assert_eq!(parse_type("bigint unsigned"), ("bigint".into(), "".into(), "".into(), true, false));
        assert_eq!(parse_type("text"), ("text".into(), "".into(), "".into(), false, false));
    }

    #[test]
    fn column_definition() {
        let mut f = field("preis", "decimal", "10");
        f.dec = "2".into();
        f.unsigned = true;
        f.default = "0".into();
        f.comment = "Netto".into();
        assert_eq!(col_sql(&f), "`preis` decimal(10,2) UNSIGNED NOT NULL DEFAULT 0 COMMENT 'Netto'");
        let mut g = field("name", "varchar", "64");
        g.nullable = true;
        g.default = "abc".into();
        assert_eq!(col_sql(&g), "`name` varchar(64) NULL DEFAULT 'abc'");
        assert_eq!(default_sql("current_timestamp()"), Some("current_timestamp()".into()));
        assert_eq!(default_sql("''"), Some("''".into()));
        assert_eq!(default_sql(""), None);
    }

    #[test]
    fn unchanged_gives_no_sql() {
        let d = loaded(vec![field("id", "int", "11"), field("name", "varchar", "20")], &["id"]);
        assert_eq!(d.sql("db", &no_cs).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn alter_diff() {
        let mut d = loaded(vec![field("id", "int", "11"), field("a", "varchar", "20"), field("b", "int", "11")], &["id"]);
        // umbenennen, löschen, hinzufügen, PK erweitern
        d.fields[1].f.name = "a2".into();
        d.fields.remove(2);
        d.fields.push(FieldRow { f: field("c", "int", "11"), orig: None, pk: Some(2) });
        let sql = d.sql("db", &no_cs).unwrap();
        assert_eq!(
            sql,
            vec!["ALTER TABLE `t`\n  DROP PRIMARY KEY,\n  DROP COLUMN `b`,\n  CHANGE COLUMN `a` `a2` varchar(20) NOT NULL,\n  \
                  ADD COLUMN `c` int(11) NOT NULL AFTER `a2`,\n  ADD PRIMARY KEY (`id`, `c`)"
                .to_string()]
        );
    }

    #[test]
    fn move_column() {
        let mut d = loaded(vec![field("a", "int", ""), field("b", "int", ""), field("c", "int", "")], &[]);
        d.fields.swap(1, 2); // a, c, b
        let sql = d.sql("db", &no_cs).unwrap();
        assert_eq!(sql.len(), 1);
        assert!(sql[0].contains("CHANGE COLUMN `c` `c` int NOT NULL AFTER `a`"), "{}", sql[0]);
        assert!(sql[0].contains("CHANGE COLUMN `b` `b` int NOT NULL AFTER `c`"), "{}", sql[0]);
    }

    #[test]
    fn index_fk_trigger_diff() {
        let mut d = loaded(vec![field("id", "int", ""), field("k", "int", "")], &["id"]);
        let ix = Index { name: "ix_k".into(), cols: "k".into(), kind: "INDEX".into(), ..Default::default() };
        d.orig_indexes = vec![ix.clone()];
        d.indexes = vec![Row::loaded(ix)];
        d.indexes[0].cur.kind = "UNIQUE".into();
        d.fks.push(Row::new(Fk {
            name: "fk_k".into(),
            cols: "k".into(),
            ref_table: "other".into(),
            ref_cols: "id".into(),
            on_delete: "CASCADE".into(),
            on_update: "RESTRICT".into(),
            ..Default::default()
        }));
        let tr = Trigger { name: "trg".into(), timing: "BEFORE".into(), event: "INSERT".into(), body: "SET NEW.k = 1".into() };
        d.orig_triggers = vec![tr.clone()];
        let sql = d.sql("db", &no_cs).unwrap();
        assert_eq!(
            sql,
            vec![
                "ALTER TABLE `t`\n  DROP INDEX `ix_k`,\n  ADD UNIQUE INDEX `ix_k` (`k`),\n  \
                 ADD CONSTRAINT `fk_k` FOREIGN KEY (`k`) REFERENCES `other` (`id`) ON DELETE CASCADE ON UPDATE RESTRICT"
                    .to_string(),
                "DROP TRIGGER `db`.`trg`".to_string(),
            ]
        );
    }

    #[test]
    fn create_table() {
        let mut d = Design::new_table();
        d.name = "neu".into();
        d.fields[0].f.name = "id".into();
        d.fields[0].f.auto_inc = true;
        d.fields[0].pk = Some(1);
        d.indexes.push(Row::new(Index { name: "ix".into(), cols: "id".into(), kind: "INDEX".into(), ..Default::default() }));
        d.opts.collation = "utf8mb4_general_ci".into();
        let cs = |_: &str| Some("utf8mb4".to_string());
        let sql = d.sql("db", &cs).unwrap();
        assert_eq!(
            sql,
            vec!["CREATE TABLE `neu` (\n  `id` int(11) NOT NULL AUTO_INCREMENT,\n  PRIMARY KEY (`id`),\n  INDEX `ix` (`id`)\n) \
                  ENGINE=InnoDB DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci"
                .to_string()]
        );
    }

    /// Gegen echten Server: Laden ist verlustfrei (unveränderter Entwurf → kein SQL),
    /// und eine neu angelegte Tabelle lädt ohne Differenz zurück.
    #[test]
    fn itest_roundtrip() {
        use crate::db::itest::{cfg, wait};
        let Some(cfg) = cfg() else { return };
        let mut w = Worker::spawn(cfg, egui::Context::default());
        let load = |w: &mut Worker, t: &str| {
            w.send(None, load_stmts("shop", t), Mode::StopOnError, usize::MAX);
            let r = wait(w);
            assert!(r.error.is_none(), "{:?}", r.error);
            let cs: Vec<(String, String)> = r.set(6).map(|s| (0..s.rows.len()).map(|i| (s.get(i, "Collation").into(), s.get(i, "Charset").into())).collect()).unwrap_or_default();
            (Design::from_result(&r, "shop", t), cs)
        };
        for t in ["kunden", "bestellungen", "log_ohne_pk", "dateien"] {
            let (d, cs) = load(&mut w, t);
            assert!(!d.fields.is_empty(), "{t}: keine Felder");
            let charset_of = |c: &str| cs.iter().find(|x| x.0 == c).map(|x| x.1.clone());
            assert_eq!(d.sql("shop", &charset_of).unwrap(), Vec::<String>::new(), "{t}");
        }

        let mut d = Design::new_table();
        d.name = "mdbm_design_test".into();
        d.fields[0].f.name = "id".into();
        d.fields[0].f.auto_inc = true;
        d.fields[0].pk = Some(1);
        let mut f = DesignTab::new_field();
        f.f.name = "name".into();
        f.f.default = "abc".into();
        f.f.comment = "Kommentar".into();
        d.fields.push(f);
        d.indexes.push(Row::new(Index { name: "ix_name".into(), cols: "name(10)".into(), kind: "INDEX".into(), ..Default::default() }));
        d.triggers.push(Row::new(Trigger { name: "mdbm_trg".into(), timing: "BEFORE".into(), event: "INSERT".into(), body: "SET NEW.name = UPPER(NEW.name)".into() }));
        d.part = Partitioning { method: "HASH".into(), expr: "id".into(), count: "2".into(), ..Default::default() };
        let mut stmts = vec!["DROP TABLE IF EXISTS mdbm_design_test".to_string()];
        stmts.extend(d.sql("shop", &no_cs).unwrap());
        w.send(Some("shop".into()), stmts.into_iter().map(Stmt::plain).collect(), Mode::StopOnError, usize::MAX);
        let r = wait(&mut w);
        assert!(r.error.is_none(), "{:?}", r.error);

        let (mut d, cs) = load(&mut w, "mdbm_design_test");
        let charset_of = |c: &str| cs.iter().find(|x| x.0 == c).map(|x| x.1.clone());
        assert_eq!(d.sql("shop", &charset_of).unwrap(), Vec::<String>::new());
        assert_eq!(d.fields[1].f.default, "'abc'");
        assert_eq!(d.indexes[0].cur.cols, "name(10)");
        assert_eq!((d.part.method.as_str(), d.part.count.as_str()), ("HASH", "2"));

        // Ändern: Feld umbenennen + neues Feld vorne
        d.fields[1].f.name = "name2".into();
        let mut f = DesignTab::new_field();
        f.f.name = "vorne".into();
        d.fields.insert(0, f);
        let alter = d.sql("shop", &charset_of).unwrap();
        w.send(Some("shop".into()), alter.into_iter().map(Stmt::plain).collect(), Mode::StopOnError, usize::MAX);
        let r = wait(&mut w);
        assert!(r.error.is_none(), "{:?}", r.error);
        let (d, cs) = load(&mut w, "mdbm_design_test");
        let charset_of = |c: &str| cs.iter().find(|x| x.0 == c).map(|x| x.1.clone());
        let names: Vec<&str> = d.fields.iter().map(|r| r.f.name.as_str()).collect();
        assert_eq!(names, ["vorne", "id", "name2"]);
        assert_eq!(d.sql("shop", &charset_of).unwrap(), Vec::<String>::new());

        w.send(Some("shop".into()), vec![Stmt::plain("DROP TABLE mdbm_design_test")], Mode::StopOnError, usize::MAX);
        wait(&mut w);
    }

    #[test]
    fn partition_diff() {
        let mut d = loaded(vec![field("id", "int", ""), field("jahr", "int", "")], &["id", "jahr"]);
        assert!(d.sql("db", &no_cs).unwrap().is_empty());
        d.part = Partitioning {
            method: "RANGE".into(),
            expr: "jahr".into(),
            parts: vec![
                Partition { name: "p2024".into(), values: "2025".into(), ..Default::default() },
                Partition { name: "pmax".into(), values: "MAXVALUE".into(), comment: "Rest".into() },
            ],
            ..Default::default()
        };
        assert_eq!(
            d.sql("db", &no_cs).unwrap(),
            vec!["ALTER TABLE `t` PARTITION BY RANGE(jahr) (\n  PARTITION `p2024` VALUES LESS THAN (2025),\n  \
                  PARTITION `pmax` VALUES LESS THAN MAXVALUE COMMENT = 'Rest'\n)"
                .to_string()]
        );
        d.orig_part = d.part.clone();
        d.part = Partitioning::default();
        assert_eq!(d.sql("db", &no_cs).unwrap(), vec!["ALTER TABLE `t` REMOVE PARTITIONING".to_string()]);
        d.part = Partitioning { method: "HASH".into(), expr: "id".into(), count: "4".into(), ..Default::default() };
        assert_eq!(d.sql("db", &no_cs).unwrap(), vec!["ALTER TABLE `t` PARTITION BY HASH(id) PARTITIONS 4".to_string()]);
        d.part.method = "LIST".into();
        assert!(d.sql("db", &no_cs).is_err(), "LIST ohne Partitionen muss abgelehnt werden");
    }

    #[test]
    fn column_list_helpers() {
        assert_eq!(cols_sql("a, b(10)"), "`a`, `b`(10)");
        let mut l = "a, b(10)".to_string();
        toggle_in_list(&mut l, "b");
        assert_eq!(l, "a");
        toggle_in_list(&mut l, "c");
        assert_eq!(l, "a, c");
    }
}
