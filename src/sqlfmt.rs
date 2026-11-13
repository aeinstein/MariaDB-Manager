//! SQL-Formatierer für den SQL-Editor: Zeilenumbruch vor Hauptklauseln, Einrückung, ein
//! Listenelement je Zeile, Schlüsselwörter in Großbuchstaben. Umgebrochen wird nur DML
//! (SELECT/WITH/INSERT/REPLACE/UPDATE/DELETE); andere Statements und Skripte mit `DELIMITER`
//! bekommen nur Schlüsselwörter in Großbuchstaben.

use crate::sqlhl;

const IND: &str = "    ";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Word,
    Quoted,
    Number,
    LineComment,
    BlockComment,
    Open,
    Close,
    Comma,
    Semi,
    Other,
}

#[derive(Clone, Copy, Debug)]
struct Tok<'a> {
    kind: Kind,
    text: &'a str,
    start: usize,
    /// Stand im Original Leerraum davor?
    space: bool,
}

impl Tok<'_> {
    fn is(&self, w: &str) -> bool {
        self.kind == Kind::Word && self.text.eq_ignore_ascii_case(w)
    }

    fn is_any(&self, ws: &[&str]) -> bool {
        ws.iter().any(|w| self.is(w))
    }

    fn is_comment(&self) -> bool {
        matches!(self.kind, Kind::LineComment | Kind::BlockComment)
    }

    fn end(&self) -> usize {
        self.start + self.text.len()
    }
}

fn is_word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c == b'@' || c >= 0x80
}

fn tokenize(s: &str) -> Vec<Tok<'_>> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut space = false;
    while i < b.len() {
        let c = b[i];
        let start = i;
        if c.is_ascii_whitespace() {
            i += 1;
            space = true;
            continue;
        }
        let kind = if c == b'\'' || c == b'"' || c == b'`' {
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' && c != b'`' {
                    i += 2;
                    continue;
                }
                if b[i] == c {
                    i += 1;
                    if i < b.len() && b[i] == c {
                        i += 1;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            Kind::Quoted
        } else if (c == b'-' && b.get(i + 1) == Some(&b'-') && b.get(i + 2).is_none_or(|x| x.is_ascii_whitespace()))
            || c == b'#'
        {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            Kind::LineComment
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            Kind::BlockComment
        } else if c.is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                i += 1;
            }
            Kind::Number
        } else if is_word_byte(c) {
            while i < b.len() && is_word_byte(b[i]) {
                i += 1;
            }
            Kind::Word
        } else {
            i += 1;
            match c {
                b'(' => Kind::Open,
                b')' => Kind::Close,
                b',' => Kind::Comma,
                b';' => Kind::Semi,
                _ => Kind::Other,
            }
        };
        // Unvollständige Strings/Kommentare enden am Textende; Mehrbyte-Zeichen nie zerteilen
        let mut end = i.min(b.len());
        while !s.is_char_boundary(end) {
            end += 1;
        }
        i = end;
        out.push(Tok { kind, text: &s[start..end], start, space });
        space = false;
    }
    out
}

/// Ausgabepuffer mit Zeilen-/Einrückungsverwaltung.
struct Out {
    s: String,
    /// Einrückung der aktuellen Zeile
    indent: usize,
    /// Aktuelle Zeile noch leer?
    at_start: bool,
    /// Nach einem Zeilenkommentar muss das nächste Token in eine neue Zeile.
    force_nl: bool,
}

impl Out {
    fn new() -> Self {
        Self { s: String::new(), indent: 0, at_start: true, force_nl: false }
    }

    fn newline(&mut self, indent: usize) {
        if self.at_start {
            // Noch leere Zeile nur neu einrücken
            let p = self.s.rfind('\n').map_or(0, |p| p + 1);
            self.s.truncate(p);
        } else {
            let t = self.s.trim_end_matches(' ').len();
            self.s.truncate(t);
            self.s.push('\n');
        }
        for _ in 0..indent {
            self.s.push_str(IND);
        }
        self.indent = indent;
        self.at_start = true;
        self.force_nl = false;
    }

    fn push(&mut self, text: &str, space: bool) {
        if self.force_nl {
            self.newline(self.indent);
        }
        if space && !self.at_start {
            self.s.push(' ');
        }
        self.s.push_str(text);
        self.at_start = false;
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Clause {
    None,
    /// Ein Element je Zeile (SELECT, SET, GROUP BY, ORDER BY, VALUES …)
    List,
    /// FROM/UPDATE mit JOINs
    From,
    /// WHERE/HAVING: AND/OR in neuer Zeile
    Cond,
    Other,
}

/// Zustand einer Abfrageebene (Hauptabfrage oder Unterabfrage in Klammern).
#[derive(Clone, Copy)]
struct Level {
    clause: Clause,
    /// Klammertiefe dieser Ebene
    depth: usize,
    /// Einrückung der Klauseln
    base: usize,
    /// Nach SELECT/SET … kommt das erste Element in eine neue Zeile.
    list_pending: bool,
    /// Einrückung für AND/OR
    cond_indent: usize,
    /// In einer JOIN-Bedingung (`ON …`)
    on: bool,
    /// Nach BETWEEN gehört das nächste AND zum Bereich.
    between: bool,
}

impl Level {
    fn new(depth: usize, base: usize) -> Self {
        Self { clause: Clause::None, depth, base, list_pending: false, cond_indent: base + 1, on: false, between: false }
    }
}

const SELECT_MODS: &[&str] = &[
    "DISTINCT",
    "ALL",
    "DISTINCTROW",
    "HIGH_PRIORITY",
    "STRAIGHT_JOIN",
    "SQL_CALC_FOUND_ROWS",
    "SQL_NO_CACHE",
    "SQL_CACHE",
    "SQL_SMALL_RESULT",
    "SQL_BIG_RESULT",
    "SQL_BUFFER_RESULT",
];

/// Hauptklausel an Position `i`: Art und Anzahl der Wörter.
fn clause_at(t: &[Tok], i: usize, is_first: bool, is_insert: bool, cur: Clause) -> Option<(Clause, usize)> {
    let w = |k: usize, s: &str| t.get(i + k).is_some_and(|x| x.is(s));
    if w(0, "SELECT") {
        return Some((Clause::List, 1));
    }
    if w(0, "FROM") {
        return Some((Clause::From, 1));
    }
    if w(0, "WHERE") || w(0, "HAVING") {
        return Some((Clause::Cond, 1));
    }
    if (w(0, "GROUP") || w(0, "ORDER")) && w(1, "BY") {
        return Some((Clause::List, 2));
    }
    if w(0, "LIMIT") || w(0, "WINDOW") {
        return Some((Clause::Other, 1));
    }
    if w(0, "UNION") || w(0, "EXCEPT") || w(0, "INTERSECT") {
        return Some((Clause::None, if w(1, "ALL") || w(1, "DISTINCT") { 2 } else { 1 }));
    }
    if w(0, "ON") && w(1, "DUPLICATE") && w(2, "KEY") && w(3, "UPDATE") {
        return Some((Clause::List, 4));
    }
    if w(0, "SET") || w(0, "RETURNING") {
        return Some((Clause::List, 1));
    }
    if (w(0, "VALUES") || w(0, "VALUE")) && is_insert && cur != Clause::List {
        return Some((Clause::List, 1));
    }
    if is_first {
        if w(0, "WITH") {
            return Some((Clause::Other, if w(1, "RECURSIVE") { 2 } else { 1 }));
        }
        if w(0, "DELETE") {
            return Some((Clause::From, if w(1, "FROM") { 2 } else { 1 }));
        }
        if w(0, "UPDATE") {
            return Some((Clause::From, 1));
        }
        if w(0, "INSERT") || w(0, "REPLACE") {
            return Some((Clause::Other, 1));
        }
    }
    None
}

/// Länge eines JOIN-Ausdrucks (`LEFT OUTER JOIN` …) an Position `i`.
fn join_len(t: &[Tok], i: usize) -> Option<usize> {
    const PRE: &[&str] = &["NATURAL", "LEFT", "RIGHT", "FULL", "INNER", "CROSS", "OUTER"];
    let mut k = i;
    while k < t.len() && t[k].is_any(PRE) {
        k += 1;
    }
    t.get(k).is_some_and(|x| x.is("JOIN") || x.is("STRAIGHT_JOIN")).then_some(k - i + 1)
}

fn word_text(tok: &Tok, after_dot: bool) -> String {
    if tok.kind == Kind::Word && !after_dot && sqlhl::is_keyword(tok.text) {
        tok.text.to_ascii_uppercase()
    } else {
        tok.text.to_string()
    }
}

fn format_stmt(t: &[Tok]) -> String {
    let mut o = Out::new();
    let mut lv = Level::new(0, 0);
    let mut depth = 0usize;
    // Je offener Klammer: bei Unterabfragen der Zustand der äußeren Ebene und die Einrückung der Zeile
    let mut parens: Vec<Option<(Level, usize)>> = Vec::new();
    // Offene CASE-Ausdrücke: (Einrückung der Zeile, Klammertiefe)
    let mut cases: Vec<(usize, usize)> = Vec::new();
    let first = t.iter().position(|x| !x.is_comment());
    let is_insert = first.is_some_and(|f| t[f].is("INSERT") || t[f].is("REPLACE"));
    let mut space_next = false;
    let mut i = 0;
    while i < t.len() {
        let tok = t[i];
        let after_dot = i > 0 && t[i - 1].text == ".";
        let at_level = depth == lv.depth;
        let space = tok.space || space_next;
        space_next = false;

        if tok.kind == Kind::Word && at_level && !after_dot {
            if let Some((cl, n)) = clause_at(t, i, Some(i) == first, is_insert, lv.clause) {
                o.newline(lv.base);
                for (k, w) in t[i..i + n].iter().enumerate() {
                    o.push(&w.text.to_ascii_uppercase(), k > 0);
                }
                lv.clause = cl;
                lv.list_pending = cl == Clause::List;
                lv.cond_indent = lv.base + 1;
                lv.on = false;
                lv.between = false;
                i += n;
                continue;
            }
            if lv.clause == Clause::From {
                if let Some(n) = join_len(t, i) {
                    o.newline(lv.base + 1);
                    for (k, w) in t[i..i + n].iter().enumerate() {
                        o.push(&w.text.to_ascii_uppercase(), k > 0);
                    }
                    lv.on = false;
                    i += n;
                    continue;
                }
            }
        }

        if lv.list_pending && at_level && !(tok.kind == Kind::Word && tok.is_any(SELECT_MODS)) {
            o.newline(lv.base + 1);
            lv.list_pending = false;
        }

        let case_here = cases.last().is_some_and(|c| c.1 == depth);
        match tok.kind {
            Kind::Word if (tok.is("AND") || tok.is("OR")) && at_level && !after_dot => {
                let cond = lv.clause == Clause::Cond || (lv.clause == Clause::From && lv.on);
                if lv.between && tok.is("AND") {
                    lv.between = false;
                } else if cond && !case_here {
                    o.newline(if lv.clause == Clause::From { lv.base + 2 } else { lv.cond_indent });
                }
                o.push(&tok.text.to_ascii_uppercase(), space);
            }
            Kind::Word if tok.is("BETWEEN") && !after_dot => {
                if at_level {
                    lv.between = true;
                }
                o.push("BETWEEN", space);
            }
            Kind::Word if tok.is("ON") && at_level && lv.clause == Clause::From && !after_dot => {
                lv.on = true;
                o.push("ON", space);
            }
            Kind::Word if tok.is("CASE") && !after_dot => {
                o.push("CASE", space);
                cases.push((o.indent, depth));
            }
            Kind::Word if (tok.is("WHEN") || tok.is("ELSE")) && case_here && !after_dot => {
                let ind = cases.last().map_or(0, |c| c.0) + 1;
                o.newline(ind);
                o.push(&tok.text.to_ascii_uppercase(), false);
            }
            Kind::Word if tok.is("END") && case_here && !after_dot => {
                let (ind, _) = cases.pop().unwrap_or_default();
                o.newline(ind);
                o.push("END", false);
            }
            Kind::Open => {
                let sub = t[i + 1..].iter().find(|x| !x.is_comment()).is_some_and(|x| x.is("SELECT") || x.is("WITH"));
                o.push("(", space);
                depth += 1;
                if sub {
                    parens.push(Some((lv, o.indent)));
                    lv = Level::new(depth, o.indent + 1);
                } else {
                    parens.push(None);
                }
            }
            Kind::Close => {
                depth = depth.saturating_sub(1);
                cases.retain(|c| c.1 <= depth);
                match parens.pop() {
                    Some(Some((saved, ind))) => {
                        o.newline(ind);
                        o.push(")", false);
                        lv = saved;
                    }
                    _ => o.push(")", tok.space),
                }
            }
            Kind::Comma => {
                o.push(",", false);
                if at_level && lv.clause == Clause::List {
                    o.newline(lv.base + 1);
                } else {
                    space_next = true;
                }
            }
            Kind::LineComment => {
                o.push(tok.text.trim_end(), space);
                o.force_nl = true;
            }
            _ => o.push(&word_text(&tok, after_dot), space),
        }
        i += 1;
    }
    o.s.trim_end().to_string()
}

fn has_delimiter(sql: &str) -> bool {
    sql.lines().any(|l| {
        let t = l.trim_start();
        t.len() >= 10 && t.as_bytes()[..10].eq_ignore_ascii_case(b"delimiter ")
    })
}

/// SQL-Text formatieren (siehe Moduldokumentation).
pub fn format(sql: &str) -> String {
    if has_delimiter(sql) {
        return uppercase_keywords(sql);
    }
    let toks = tokenize(sql);
    // Statements an `;` außerhalb von Klammern trennen: (Tokens, mit Semikolon?)
    let mut stmts: Vec<(&[Tok], bool)> = Vec::new();
    let (mut depth, mut s) = (0usize, 0usize);
    for (k, t) in toks.iter().enumerate() {
        match t.kind {
            Kind::Open => depth += 1,
            Kind::Close => depth = depth.saturating_sub(1),
            Kind::Semi if depth == 0 => {
                stmts.push((&toks[s..k], true));
                s = k + 1;
            }
            _ => {}
        }
    }
    if s < toks.len() {
        stmts.push((&toks[s..], false));
    }
    let mut parts = Vec::new();
    for (st, semi) in stmts {
        let (Some(first), Some(last)) = (st.first(), st.last()) else { continue };
        let lead = st.iter().find(|x| !x.is_comment());
        let dml = lead.is_some_and(|f| {
            f.kind == Kind::Open || f.is_any(&["SELECT", "WITH", "INSERT", "REPLACE", "UPDATE", "DELETE"])
        });
        let mut p = if dml { format_stmt(st) } else { uppercase_keywords(&sql[first.start..last.end()]) };
        if semi {
            if last.kind == Kind::LineComment {
                p.push('\n');
            }
            p.push(';');
        }
        parts.push(p);
    }
    let mut out = parts.join("\n\n");
    if sql.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Nur Schlüsselwörter in Großbuchstaben, sonst Text unverändert.
pub fn uppercase_keywords(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let b = sql.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\'' || c == b'"' || c == b'`' {
            let s = i;
            i += 1;
            while i < b.len() && b[i] != c {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(b.len());
            while !sql.is_char_boundary(i) {
                i += 1;
            }
            out.push_str(&sql[s..i]);
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let s = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let w = &sql[s..i];
            if sqlhl::is_keyword(w) {
                out.push_str(&w.to_ascii_uppercase());
            } else {
                out.push_str(w);
            }
        } else if c == b'-' && b.get(i + 1) == Some(&b'-') {
            let s = i;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            out.push_str(&sql[s..i]);
        } else {
            let s = i;
            i += 1;
            while i < b.len() && !sql.is_char_boundary(i) {
                i += 1;
            }
            out.push_str(&sql[s..i]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::split_sql;

    fn check(src: &str, want: &str) {
        let got = format(src);
        assert_eq!(got, want, "\n--- erhalten ---\n{got}\n");
        assert_eq!(format(&got), got, "nicht idempotent");
        assert_eq!(split_sql(&got).len(), split_sql(src).len());
    }

    #[test]
    fn sqlfmt_select_join_where() {
        check(
            "select a, b as x, concat_ws(' ', c, d) from t1 as t inner join t2 on t.id = t2.id and t2.k=1 \
             left join t3 using (id) where a = 1 and (b = 2 or c = 3) and d between 1 and 5 order by a desc, b",
            "SELECT\n    a,\n    b AS x,\n    concat_ws(' ', c, d)\nFROM t1 AS t\n    INNER JOIN t2 ON t.id = t2.id\n        AND t2.k=1\n    LEFT JOIN t3 USING (id)\nWHERE a = 1\n    AND (b = 2 OR c = 3)\n    AND d BETWEEN 1 AND 5\nORDER BY\n    a DESC,\n    b",
        );
    }

    #[test]
    fn sqlfmt_case_and_subquery() {
        check(
            "SELECT CASE WHEN a = '' OR a IS NULL THEN '' ELSE a END AS k, x FROM t WHERE id IN (SELECT id FROM u WHERE z = 1)",
            "SELECT\n    CASE\n        WHEN a = '' OR a IS NULL THEN ''\n        ELSE a\n    END AS k,\n    x\nFROM t\nWHERE id IN (\n    SELECT\n        id\n    FROM u\n    WHERE z = 1\n)",
        );
    }

    #[test]
    fn sqlfmt_strings_comments_umlauts() {
        check(
            "SELECT '' AS Preisschlüssel, 'a;b' AS `Zählernr.`, x -- Kommentar; mit Semikolon\nFROM t;",
            "SELECT\n    '' AS Preisschlüssel,\n    'a;b' AS `Zählernr.`,\n    x -- Kommentar; mit Semikolon\nFROM t;",
        );
    }

    #[test]
    fn sqlfmt_multiple_and_ddl() {
        check(
            "CREATE TABLE t (id int primary key, n varchar(10));\nselect 1;update t set a=1, b=2 where id=3",
            "CREATE TABLE t (id int PRIMARY KEY, n varchar(10));\n\nSELECT\n    1;\n\nUPDATE t\nSET\n    a=1,\n    b=2\nWHERE id=3",
        );
    }

    #[test]
    fn sqlfmt_insert() {
        check(
            "insert into t (a, b) values (1, 2), (3, 4) on duplicate key update b = values(b)",
            "INSERT INTO t (a, b)\nVALUES\n    (1, 2),\n    (3, 4)\nON DUPLICATE KEY UPDATE\n    b = VALUES(b)",
        );
    }

    #[test]
    fn sqlfmt_long_export_query() {
        let src = "SELECT   v.v_indx AS EBP_ID,  v_dat_artikel AS Tarifname,  '' AS Preisschlüssel,  SUBSTRING(c_tel, 3) AS Telefon,    c.c_email AS` E-Mail Adresse`,    DATE_FORMAT(c_geb, '%d.%m.%Y') AS Geburtsdatum,    IF(v_dat_re_anrede <> '', v_dat_re_anrede,'') AS `abw. Rechnungsadresse Anrede`,    CASE   WHEN getLastIban(c.c_indx) = '' OR getLastIban(c.c_indx) IS NULL THEN ''  WHEN v_dat_inhaber = '' OR v_dat_inhaber IS NULL   THEN CONCAT_WS(' ', v_dat_la_vorname, v_dat_la_name)  ELSE v_dat_inhaber  END AS Kontoinhaber,  0 AS WerbungErlaubt_Telefon , v.v_indx, c.c_indx, u.vp_indx FROM vorgaenge_sap_neckermann AS v  INNER JOIN customer AS c ON v.c_indx = c.c_indx  INNER JOIN users AS u ON v.u_indx = u.u_indx  LEFT JOIN vp ON u.vp_indx = vp.vp_indx   WHERE v_listekz LIKE '%;M4E;%' ORDER BY v.c_indx";
        let got = format(src);
        assert_eq!(format(&got), got);
        assert!(got.contains("\n    IF(v_dat_re_anrede <> '', v_dat_re_anrede, '') AS `abw. Rechnungsadresse Anrede`,\n"));
        assert!(got.contains("\n    END AS Kontoinhaber,\n    0 AS WerbungErlaubt_Telefon,\n    v.v_indx,\n"));
        assert!(got.contains("\nFROM vorgaenge_sap_neckermann AS v\n    INNER JOIN customer AS c ON v.c_indx = c.c_indx\n"));
        assert!(got.ends_with("\nWHERE v_listekz LIKE '%;M4E;%'\nORDER BY\n    v.c_indx"));
    }

    #[test]
    fn sqlfmt_delimiter_untouched() {
        let src = "DELIMITER $$\ncreate procedure p() begin select 1; end$$\nDELIMITER ;";
        assert_eq!(format(src), uppercase_keywords(src));
    }
}
