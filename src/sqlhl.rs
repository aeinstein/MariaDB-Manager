//! Schneller, handgeschriebener SQL-Tokenizer für das Syntax-Highlighting.

use eframe::egui::{self, text::LayoutJob, Color32, FontId, TextFormat};

const KEYWORDS: &[&str] = &[
    "ADD", "ALL", "ALTER", "ANALYZE", "AND", "AS", "ASC", "AUTO_INCREMENT", "BEFORE", "BEGIN", "BETWEEN", "BY",
    "CALL", "CASCADE", "CASE", "CHANGE", "CHARACTER", "CHARSET", "CHECK", "COLLATE", "COLUMN", "COMMENT", "COMMIT",
    "CONSTRAINT", "CREATE", "CROSS", "CURRENT_TIMESTAMP", "DATABASE", "DATABASES", "DECLARE", "DEFAULT", "DEFINER",
    "DELETE", "DELIMITER", "DESC", "DESCRIBE", "DISTINCT", "DO", "DROP", "DUPLICATE", "EACH", "ELSE", "ELSEIF",
    "END", "ENGINE", "EVENT", "EXISTS", "EXPLAIN", "FALSE", "FOR", "FOREIGN", "FROM", "FULL", "FUNCTION", "GRANT",
    "GROUP", "HAVING", "IF", "IGNORE", "IN", "INDEX", "INNER", "INSERT", "INTERVAL", "INTO", "IS", "JOIN", "KEY",
    "KEYS", "KILL", "LEFT", "LIKE", "LIMIT", "LOCK", "NATURAL", "NOT", "NULL", "OFFSET", "ON", "OPTIMIZE", "OR",
    "ORDER", "OUTER", "OVER", "PARTITION", "PRIMARY", "PRIVILEGES", "PROCEDURE", "REFERENCES", "REGEXP", "RENAME",
    "REPAIR", "REPLACE", "RETURN", "RETURNS", "REVOKE", "RIGHT", "ROLLBACK", "ROW", "ROWS", "SCHEMA", "SELECT",
    "SET", "SHOW", "START", "STATUS", "TABLE", "TABLES", "TEMPORARY", "THEN", "TO", "TRANSACTION", "TRIGGER",
    "TRUE", "TRUNCATE", "UNION", "UNIQUE", "UNLOCK", "UNSIGNED", "UPDATE", "USE", "USER", "USING", "VALUES",
    "VARIABLES", "VIEW", "WHEN", "WHERE", "WHILE", "WITH", "ZEROFILL", "RECURSIVE", "WINDOW", "RETURNING",
];

const TYPES: &[&str] = &[
    "BIGINT", "BINARY", "BIT", "BLOB", "BOOL", "BOOLEAN", "CHAR", "DATE", "DATETIME", "DECIMAL", "DOUBLE", "ENUM",
    "FLOAT", "INT", "INTEGER", "JSON", "LONGBLOB", "LONGTEXT", "MEDIUMBLOB", "MEDIUMINT", "MEDIUMTEXT", "NUMERIC",
    "REAL", "SMALLINT", "TEXT", "TIME", "TIMESTAMP", "TINYBLOB", "TINYINT", "TINYTEXT", "VARBINARY", "VARCHAR",
    "YEAR", "UUID", "INET4", "INET6",
];

pub fn is_keyword(w: &str) -> bool {
    let up = w.to_ascii_uppercase();
    KEYWORDS.binary_search(&up.as_str()).is_ok() || KEYWORDS.contains(&up.as_str())
}

fn is_type(w: &str) -> bool {
    let up = w.to_ascii_uppercase();
    TYPES.contains(&up.as_str())
}

struct Palette {
    text: Color32,
    keyword: Color32,
    typ: Color32,
    string: Color32,
    number: Color32,
    comment: Color32,
    ident: Color32,
    func: Color32,
}

fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            text: Color32::from_rgb(0xd4, 0xd4, 0xd4),
            keyword: Color32::from_rgb(0x56, 0x9c, 0xd6),
            typ: Color32::from_rgb(0x4e, 0xc9, 0xb0),
            string: Color32::from_rgb(0xce, 0x91, 0x78),
            number: Color32::from_rgb(0xb5, 0xce, 0xa8),
            comment: Color32::from_rgb(0x6a, 0x99, 0x55),
            ident: Color32::from_rgb(0x9c, 0xdc, 0xfe),
            func: Color32::from_rgb(0xdc, 0xdc, 0xaa),
        }
    } else {
        Palette {
            text: Color32::from_rgb(0x1e, 0x1e, 0x1e),
            keyword: Color32::from_rgb(0x00, 0x00, 0xc8),
            typ: Color32::from_rgb(0x26, 0x7f, 0x99),
            string: Color32::from_rgb(0xa3, 0x15, 0x15),
            number: Color32::from_rgb(0x09, 0x86, 0x58),
            comment: Color32::from_rgb(0x00, 0x80, 0x00),
            ident: Color32::from_rgb(0x80, 0x00, 0x80),
            func: Color32::from_rgb(0x79, 0x5e, 0x26),
        }
    }
}

pub fn highlight(text: &str, font: FontId, dark: bool, wrap_width: f32) -> LayoutJob {
    let p = palette(dark);
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap_width;
    let fmt = |c: Color32| TextFormat { font_id: font.clone(), color: c, ..Default::default() };
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let c = b[i];
        let color;
        if c == b'\'' || c == b'"' || c == b'`' {
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
            color = if c == b'`' { p.ident } else { p.string };
        } else if (c == b'-' && b.get(i + 1) == Some(&b'-')) || c == b'#' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            color = p.comment;
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            color = p.comment;
        } else if c.is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                i += 1;
            }
            color = p.number;
        } else if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80) {
                i += 1;
            }
            let w = &text[start..i];
            let next_paren = text[i..].trim_start_matches(' ').starts_with('(');
            color = if is_type(w) {
                p.typ
            } else if is_keyword(w) {
                p.keyword
            } else if next_paren {
                p.func
            } else {
                p.text
            };
        } else {
            // Restliche Zeichen (auch Mehrbyte-UTF-8 außerhalb von Wörtern) als Block übernehmen
            i += 1;
            while i < b.len() && !text.is_char_boundary(i) {
                i += 1;
            }
            color = p.text;
        }
        // Grenzen sicherstellen (Strings können mitten in UTF-8 enden, falls unvollständig)
        let mut end = i.min(b.len());
        while !text.is_char_boundary(end) {
            end += 1;
        }
        i = end;
        job.append(&text[start..end], 0.0, fmt(color));
    }
    job
}

/// Layouter für `TextEdit::layouter`.
pub fn layouter(ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap_width: f32) -> std::sync::Arc<egui::Galley> {
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let job = highlight(buf.as_str(), font, ui.visuals().dark_mode, wrap_width);
    ui.ctx().fonts_mut(|f| f.layout_job(job))
}
