# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Projekt

Nativer MariaDB-Client im Stil von Navicat 8, geschrieben in Rust mit egui/eframe (glow/OpenGL). Eine einzelne Binary ohne Laufzeit-Abhängigkeiten für Windows, Linux und macOS. UI-Texte, Kommentare und Fehlermeldungen sind durchgehend **deutsch** – neue Texte ebenfalls auf Deutsch schreiben.

## Befehle

```sh
cargo build --release          # Release-Build (fat LTO, panic=abort – dauert entsprechend)
cargo run                      # Debug-Start (Abhängigkeiten werden auch im Dev-Profil mit opt-level 2 gebaut)
cargo test                     # Unit-Tests (split_sql, Quoting, GRANT/REVOKE-Diff)
cargo test split_delimiter     # Einzelnen Test ausführen (Filter nach Testname)
```

Integrationstests gegen einen echten Server:

```sh
mariadb -uroot -p --default-character-set=utf8mb4 < tests/testdata.sql   # legt DB „shop“ an
MDBM_TEST_HOST=127.0.0.1 MDBM_TEST_USER=root MDBM_TEST_PASS=… cargo test -- --nocapture
```

Ohne `MDBM_TEST_HOST` kehren die Integrationstests sofort zurück und gelten als **bestanden** – ein grünes `cargo test` sagt dann nichts über DB-Verhalten aus. Die Helfer `cfg()` und `wait()` liegen in `db::itest` (`src/db.rs`) und werden auch von `users.rs` genutzt. CI (`.github/workflows/ci.yml`) testet gegen MariaDB 10.11, 11.8 und 12.3; ein Tag `v*` erzeugt ein GitHub-Release.

## Architektur

**Threading-Modell (zentral):** Jede DB-Verbindung ist ein `db::Worker` – ein eigener Thread mit eigener `mysql::Conn`, angesprochen über `mpsc`-Kanäle. Der UI-Thread blockiert nie:
- `Worker::send(db, stmts, mode, max_rows)` liefert ein **Tag** (`u64`) zurück; das `JobResult` kommt später mit demselben Tag.
- Der Aufrufer merkt sich das Tag (z. B. `tag_load`/`tag_commit` in `DataTab`, `pending: HashMap<u64, Pending>` in `app::ConnNode`) und fragt jeden Frame per `poll()` nicht-blockierend ab.
- Der Worker ruft nach jedem Job `ctx.request_repaint()` auf, damit egui im Leerlauf nicht neu zeichnen muss.
- `Mode` steuert Fehlerverhalten: `StopOnError`, `Transaction` (Rollback bei Fehler), `Continue`.
- Abbrechen läuft über `Worker::cancel()` → `KILL QUERY <id>` über eine kurzlebige Zweitverbindung.
- Reconnect passiert automatisch (Ping vor jedem Job).

Ein Job enthält oft mehrere Statements (z. B. Daten + Schlüsselinfo, Version + Datenbankliste); die Ergebnisse werden positionsbasiert über `JobResult::set(n)` / `take_set(n)` ausgelesen. Beim Ändern einer Statement-Liste die zugehörigen Indizes im Poll-Code mit anpassen.

**Werte:** Alle Zellwerte sind `Option<String>` (`None` = NULL); Binärdaten werden als Hex dargestellt und sind schreibgeschützt. Benutzerdaten gehen immer als Prepared-Statement-Parameter (`Stmt::with`), nie per String-Verkettung. Bezeichner werden mit `db::qi`, String-Literale (wo Parameter nicht möglich sind, z. B. GRANT) mit `db::qs` gequotet. `db::split_sql` zerlegt Skripte inkl. `DELIMITER`-Unterstützung.

**Module:**
- `app.rs` – Hauptfenster: Verbindungsbaum, Objektliste, Tab-Leiste, Dialoge. Jede Verbindung im Baum hat einen eigenen Worker für Metadaten-Abfragen.
- `datatab.rs`, `querytab.rs`, `users.rs` – die drei Tab-Typen. **Jeder Tab erzeugt einen eigenen Worker** (eigene Verbindung), damit lange Abfragen andere Tabs nicht blockieren.
- `grid.rs` – virtualisiertes Datengrid (nur sichtbare Zeilen werden gezeichnet); Änderungen werden in `GridEdits` gesammelt und von `DataTab::commit` in einer Transaktion geschrieben. Zeilenidentifikation: PRIMARY KEY → erster UNIQUE-Index → Vollvergleich mit `LIMIT 1`.
- `users.rs` – Benutzer-/Rechteverwaltung auf Server-, DB- und Tabellenebene; `build_sql` erzeugt nur die Differenz als GRANT/REVOKE (Unit-Test `grant_revoke_diff`).
- `sqlhl.rs` – Syntax-Highlighting als egui-`layouter`.
- `config.rs` – `Settings` als JSON im Benutzerprofil (Pfad via `directories`, Qualifier `de.itnox`).
- `designtab.rs` – Tabellenentwurf (Felder, Indizes, FKs, Trigger, Optionen, Partitionierung); `Design::sql` erzeugt nur die Differenz.
- `cellview.rs` – Detailbereich unter dem Grid (mehrzeiliger Text, Binärwerte als Bild/Hex).
- `queries.rs` – gespeicherte Abfragen als `.sql` unter `<Konfigordner>/queries/<Verbindung>/<Datenbank>/`; im Baum als Kategorie `Cat::Queries`.
- `export.rs` – CSV-Export (`;`, LF, Werte gequotet); Tabellen werden auf eigener Verbindung in die Datei gestreamt.
- `import.rs` – Import-Assistent für CSV/TXT (Erkennung von Codierung/Trennern, Vorschau, Feldzuordnung); Import in eigenem Thread, Mehrzeilen-INSERT in einer Transaktion.
- `navicat.rs` – Import aus Navicat 8 (Registry + Abfrage-Ordner, Passwort-Entschlüsselung Blowfish); wird beim ersten Start einmalig angeboten (`Settings::navicat_asked`).
- `icons.rs` – farbige Vektor-Icons im Navicat-Stil (Painter, keine Bilddateien).

**UI-Aktionsmuster in `app.rs`:** Während des Zeichnens (Baum, Kontextmenüs, Objektliste) werden Änderungen nicht direkt ausgeführt, sondern als `Act`-Varianten in `self.acts` gesammelt und anschließend in `apply_acts` abgearbeitet – das umgeht Borrow-Konflikte mit `&mut self`. Neue Interaktionen im Baum/in Menüs nach diesem Muster ergänzen.

## Hinweise

- `main.rs` setzt `windows_subsystem = "windows"` nur im Release – im Release-Build unter Windows gibt es keine Konsolenausgabe.
- eframe/egui sind mit `default-features = false` eingebunden; neue egui-Features ggf. explizit in `Cargo.toml` aktivieren.
- macOS-Paketierung nutzt `packaging/macos/Info.plist` (Platzhalter `@VERSION@` wird in der CI aus `Cargo.toml` ersetzt).
