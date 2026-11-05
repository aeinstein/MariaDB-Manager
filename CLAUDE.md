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
- `grid.rs` – virtualisiertes Datengrid (nur sichtbare Zeilen werden gezeichnet); Änderungen werden in `GridEdits` gesammelt und von `DataTab::commit` in einer Transaktion geschrieben. Zeilenidentifikation: PRIMARY KEY → erster UNIQUE-Index → Vollvergleich mit `LIMIT 1` (`grid::pick_key_cols`; SQL-Erzeugung gemeinsam für Daten- und Abfrage-Tab in `grid::EditTarget::stmts`). Neue Zeilen: Einfg bzw. ↓ in der letzten Zeile liefern `GridOut::add_row` (nur bei `GridEdits::allow_insert`), der Tab hängt die Zeile an; leere neue Zeilen zählen nicht als Änderung (`is_dirty` vs. `has_pending`) und erzeugen kein INSERT.
- `querytab.rs` – SQL-Editor; Ergebnisse sind editierbar (UPDATE/DELETE/INSERT), wenn alle Spalten mit Ursprungstabelle aus genau einer Tabelle stammen (`ResView::new`, Spalten über `Col::org` statt Alias). Schlüssel per `SHOW KEYS` nach dem Ausführen; fehlt eine Schlüsselspalte im Ergebnis, Vollvergleich über die Ergebnisspalten. Übernommene Änderungen werden lokal eingespielt, die Abfrage wird nicht neu ausgeführt.
- `users.rs` – Benutzer-/Rechteverwaltung auf Server-, DB- und Tabellenebene; `build_sql` erzeugt nur die Differenz als GRANT/REVOKE (Unit-Test `grant_revoke_diff`).
- `sqlhl.rs` – Syntax-Highlighting als egui-`layouter`.
- `config.rs` – `Settings` als JSON im Benutzerprofil (Pfad via `directories`, Qualifier `de.itnox`).
- `designtab.rs` – Tabellenentwurf (Felder, Indizes, FKs, Trigger, Optionen, Partitionierung); `Design::sql` erzeugt nur die Differenz.
- `cellview.rs` – Detailbereich unter dem Grid (mehrzeiliger Text, Binärwerte als Bild/Hex).
- `queries.rs` – gespeicherte Abfragen als `.sql` unter `<Konfigordner>/queries/<Verbindung>/<Datenbank>/`; im Baum als Kategorie `Cat::Queries`.
- `export.rs` – CSV-Export (`;`, LF, Werte gequotet); Tabellen werden auf eigener Verbindung in die Datei gestreamt.
- `dump.rs` – SQL-Dump wie mysqldump (nativ, kein externes Programm) für Datenbank oder einzelne Tabelle; eigene Verbindung, optional `START TRANSACTION WITH CONSISTENT SNAPSHOT`, Ansichten über Platzhalter, Routinen/Trigger/Ereignisse mit `DELIMITER ;;`. Hintergrund-Thread über `export::spawn` (gemeinsam mit dem CSV-Export). Rundlauf-Integrationstest `itest_dump_roundtrip`.
- `sync.rs` – Strukturabgleich zweier Datenbanken (auch verbindungsübergreifend): Tabellen, Ansichten, Prozeduren, Funktionen, Ereignisse. Ansichten/Routinen/Ereignisse werden per `SHOW CREATE …` (ohne DEFINER, bei Ansichten ohne Qualifizierung mit der eigenen DB) textuell verglichen und bei Unterschied per `CREATE OR REPLACE VIEW` bzw. `DROP … IF EXISTS` + `CREATE` ersetzt. Die Struktur kommt gesammelt per `designtab::schema_stmts`/`parse_schema`; `designtab::sync_sql` setzt den Ziel-Entwurf auf den Quell-Stand und nutzt dann `Design::sql` für die Differenz. Das Skript geht in den SQL-Editor des Ziels oder wird direkt ausgeführt (je Tabelle ein Job auf dem Ziel-Worker; erfolgreiche Einträge verschwinden aus der Liste). Die Differenzanzeige vergleicht `designtab::struct_lines` beider Seiten zeilenweise (`line_diff`).
- `import.rs` – Import-Assistent für CSV/TXT (Erkennung von Codierung/Trennern, Vorschau, Feldzuordnung); Import in eigenem Thread, Mehrzeilen-INSERT in einer Transaktion.
- `backup.rs` – Export/Import der Einstellungen (Verbindungen + gespeicherte Abfragen) als eine JSON-Datei (Kennung `format`), aufrufbar in den Optionen; Passwörter nur auf Wunsch, vorhandene Verbindungen gleichen Namens bleiben beim Import unverändert.
- `navicat.rs` – Import aus Navicat 8 (Registry + Abfrage-Ordner, Passwort-Entschlüsselung Blowfish); wird beim ersten Start einmalig angeboten (`Settings::navicat_asked`).
- `monitor.rs` – Servermonitor wie mytop: Kennzahlen aus `SHOW GLOBAL STATUS`, Prozessliste (Filter, Sortierung, KILL / KILL QUERY, `SHOW EXPLAIN FOR`), Reiter für Status und Servervariablen; Auto-Aktualisierung nur, solange der Tab sichtbar ist.
- `icons.rs` – farbige Vektor-Icons im Navicat-Stil (Painter, keine Bilddateien).
- App-Icon: Quelle `assets/icon.svg`; `icon.png`/`.ico`/`.icns` werden mit dem eigenständigen Werkzeug `cargo run --manifest-path tools/gen-icon/Cargo.toml` erzeugt und eingecheckt. `main.rs::app_icon` lädt das PNG als Fenstericon, `build.rs` bettet unter Windows die ICO per `winresource` in die EXE ein, die CI kopiert die ICNS ins macOS-Bundle und `.desktop` + PNG ins Linux-Archiv.

**Tabs und eigene Fenster:** Jeder `Tab` hat eine feste `id`; `push_tab` legt an, `focus_tab` zeigt einen vorhandenen Tab (bei herausgelöstem Tab wird sein Fenster nach vorn geholt). Ein Reiter lässt sich aus der Tab-Leiste ziehen (oder per Kontextmenü lösen) → `Tab::detached`, dann zeichnet `tab_windows` ihn per `show_viewport_immediate` in einem eigenen Fenster; das Schließen des Fensters schließt den Tab. Der Inhalt läuft für beide Fälle über `tab_ui` und steckt in `ui.push_id(tab.id)` – IDs in Tab-Modulen daher relativ (`id_salt`, `ui.scope_id().with(..)`) statt global per `Id::unique` vergeben. `active` zeigt nie auf einen gelösten Tab (`fix_active`).

**Objektliste:** Spalten je Kategorie in `obj_cols`, Anzeigewert und Sortierschlüssel in `obj_cell`, Sortierung per Klick auf den Spaltenkopf (`sort_objs`, gespeichert in `Settings::obj_sort`).

**UI-Aktionsmuster in `app.rs`:** Während des Zeichnens (Baum, Kontextmenüs, Objektliste) werden Änderungen nicht direkt ausgeführt, sondern als `Act`-Varianten in `self.acts` gesammelt und anschließend in `apply_acts` abgearbeitet – das umgeht Borrow-Konflikte mit `&mut self`. Neue Interaktionen im Baum/in Menüs nach diesem Muster ergänzen.

## Hinweise

- `main.rs` setzt `windows_subsystem = "windows"` nur im Release – im Release-Build unter Windows gibt es keine Konsolenausgabe.
- eframe/egui sind mit `default-features = false` eingebunden; neue egui-Features ggf. explizit in `Cargo.toml` aktivieren.
- macOS-Paketierung nutzt `packaging/macos/Info.plist` (Platzhalter `@VERSION@` wird in der CI aus `Cargo.toml` ersetzt).
