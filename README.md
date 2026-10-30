# MariaDB Manager

Schlanker, schneller Datenbank-Navigator für MariaDB, aufgebaut wie Navicat 8:
Werkzeugleiste oben, Verbindungsbaum links, Objektliste und Tabs rechts, Statusleiste unten.
Läuft auf **Windows, Linux und macOS** als eine einzige Programmdatei (~11 MB) ohne Laufzeit-Abhängigkeiten.

## Funktionen (Version 0.1)

**Verbindungen & Objektbaum**
- Verbindungen anlegen, bearbeiten, testen, entfernen (Host, Port, Benutzer, Passwort, Standard-DB, SSL/TLS)
- Baum: Verbindung → Datenbanken → Tabellen / Ansichten / Funktionen & Prozeduren / Ereignisse
- Objektliste wie in Navicat (Zeilen, Engine, Daten- und Indexgröße, Auto-Inc., Kollation, Kommentar), Filter
- Datenbank anlegen/löschen, Tabelle leeren/löschen, DDL anzeigen (öffnet als bearbeitbares SQL)

**Datengrid**
- Virtualisiert: es werden nur die sichtbaren Zeilen gezeichnet
- Seitenweise Laden (Standard: 1000 Zeilen, einstellbar), optional `COUNT(*)`
- Serverseitige Sortierung (Klick auf den Spaltenkopf), WHERE-Filter, „Nach diesem Wert filtern“
- Bearbeiten: Doppelklick/F2/Enter, „Auf NULL setzen“, neue Zeile, Zeile löschen (Entf)
- Änderungen werden gesammelt und mit **Strg+S** in **einer Transaktion** geschrieben,
  bei Fehler Rollback; Esc verwirft
- Zeilen-Identifikation über PRIMARY KEY, sonst ersten UNIQUE-Index, sonst Vollvergleich mit `LIMIT 1`
- Daten laufen über Prepared-Statement-Parameter (kein String-Escaping, keine SQL-Injection über Zellwerte)
- BLOB/Binär als Hex (schreibgeschützt), Zahlen rechtsbündig, NULL grau

**SQL-Editor**
- Syntax-Highlighting, Ausführen (Strg+R / F9), aktuelles Statement bzw. Auswahl (Strg+Enter)
- Mehrere Statements, `DELIMITER` wird unterstützt, ein Ergebnis-Tab pro Result-Set, Meldungen mit Laufzeiten
- Abbrechen per `KILL QUERY` (Stopp-Knopf), „Bei Fehler fortfahren“, Datenbankauswahl pro Tab

**Benutzer-Manager**
- Benutzer anlegen, umbenennen (Benutzer/Host), Passwort setzen, löschen
- Rechte als Checkboxen auf drei Ebenen: **Server (`*.*`)**, **Datenbank (`db.*`)**, **Tabelle (`db.tabelle`)**,
  jeweils mit GRANT OPTION
- Es werden nur die Unterschiede als `GRANT`/`REVOKE` ausgeführt; die SQL-Vorschau zeigt sie vorher an
- Die Liste der Serverrechte kommt aus `SHOW PRIVILEGES`, passt also zur jeweiligen MariaDB-Version

## Warum schnell

- Natives Rust-Programm, GPU-gerenderte Oberfläche (egui/OpenGL); im Leerlauf 0 % CPU
- Jeder Tab hat eine eigene Verbindung in einem eigenen Thread – eine lange Abfrage blockiert nie die Oberfläche
- Gemessen gegen MariaDB 10.11 lokal: 200.000 Zeilen × 7 Spalten in ca. 0,3 s geladen, eine Seite mit 1000 Zeilen in wenigen Millisekunden

## Tastenkürzel

| Kürzel | Funktion |
|---|---|
| Strg+T | Neue Abfrage |
| Strg+W | Tab schließen |
| Strg+R / F9 | Alles ausführen |
| Strg+Enter | Aktuelles Statement / Auswahl ausführen |
| F5 | Daten neu laden |
| F2 / Enter / Doppelklick | Zelle bearbeiten |
| Entf | Zeile zum Löschen markieren |
| Strg+S | Änderungen übernehmen |
| Esc | Änderungen verwerfen |
| Strg+C | Zellwert kopieren |

(Auf macOS jeweils ⌘ statt Strg.)

## Bauen

Voraussetzung: Rust (stable, https://rustup.rs).

```sh
cargo build --release
# Programm: target/release/mariadb-manager (bzw. .exe)
```

Unter Linux sind keine Entwicklungspakete nötig; zur Laufzeit werden X11 oder Wayland sowie OpenGL genutzt.

Die GitHub-Actions-Pipeline (`.github/workflows/ci.yml`) testet gegen MariaDB 10.11, 11.8 und 12.3 und baut
Linux-, Windows- und macOS-Pakete (Intel + Apple Silicon). Ein Tag `v*` erzeugt automatisch ein Release.

## Tests

```sh
cargo test                                   # Unit-Tests
mariadb -uroot -p < tests/testdata.sql       # Testdaten anlegen (DB „shop“)
MDBM_TEST_HOST=127.0.0.1 MDBM_TEST_USER=root MDBM_TEST_PASS=… cargo test -- --nocapture
```

## Konfiguration

Verbindungen und Optionen liegen als JSON im Benutzerprofil:

- Linux: `~/.config/mariadbmanager/settings.json`
- Windows: `%APPDATA%\itnox\MariaDB Manager\config\settings.json`
- macOS: `~/Library/Application Support/de.itnox.MariaDB-Manager/settings.json`

Passwörter werden nur gespeichert, wenn „Passwort speichern“ aktiv ist – dann im Klartext
(unter Linux/macOS mit Dateirechten 600). Für sensible Server das Häkchen weglassen.

## Noch nicht enthalten (Ideen für die nächsten Versionen)

Tabellen-Designer, Import/Export (CSV, SQL-Dump), SSH-Tunnel, Autovervollständigung im Editor,
Rechte auf Spalten- und Routinenebene, Rollen, Passwortspeicherung im System-Schlüsselbund.
