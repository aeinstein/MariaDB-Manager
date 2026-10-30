# MariaDB Manager

Schlanker, schneller Datenbank-Navigator für MariaDB, aufgebaut wie Navicat 8:
Werkzeugleiste oben, Verbindungsbaum links, Objektliste und Tabs rechts, Statusleiste unten.
Läuft auf **Windows, Linux und macOS** als eine einzige Programmdatei (~11 MB) ohne Laufzeit-Abhängigkeiten.

## Funktionen

**Verbindungen & Objektbaum**
- Verbindungen anlegen, bearbeiten, testen, entfernen (Host oder IP, Port, Benutzer, Passwort, Standard-DB, SSL/TLS)
- Hostnamen werden selbst aufgelöst (IPv4 zuerst, dann alle weiteren Adressen der Reihe nach)
- Baum: Verbindung → Datenbanken → Tabellen / Ansichten / Funktionen & Prozeduren / Ereignisse / Abfragen;
  Einfachklick markiert, Doppelklick öffnet
- Objektliste wie in Navicat (Zeilen, Engine, Daten- und Indexgröße, Auto-Inc., Kollation, Kommentar), Filter;
  Datenbanken einer Verbindung als Liste
- Klick auf eine Kategorie (z. B. *Tabellen*) zeigt rechts eine einfache Liste wie die Listenansicht
  im Explorer; auf Datenbankebene bzw. über die Werkzeugleiste die Detailliste
- **TelliMATE-Modus** (Optionen): Tabellen `vorgaenge_*`, `helper_*` und `tarife_*` stehen im Baum in eigenen Ordnern,
  angezeigt wird nur der Rest des Namens
- Datenbank anlegen, bearbeiten (Zeichensatz/Kollation per `ALTER DATABASE`) und löschen; Tabelle leeren/löschen, DDL anzeigen (öffnet als bearbeitbares SQL)
- **Navicat-Import:** Beim ersten Start werden Verbindungen (inkl. Passwörter) und gespeicherte Abfragen
  aus Navicat angeboten; später erneut über *Optionen → Aus Navicat importieren …*

**Datengrid**
- Virtualisiert: es werden nur die sichtbaren Zeilen gezeichnet
- Seitenweise Laden (Standard: 1000 Zeilen, einstellbar), Gesamtzahl auf Knopfdruck per `COUNT(*)`
- Serverseitige Sortierung (Klick auf den Spaltenkopf), WHERE-Filter, „Nach diesem Wert filtern“
- Bearbeiten: Doppelklick/F2/Enter, „Auf NULL setzen“, neue Zeile, Zeile löschen (Entf)
- Änderungen werden gesammelt und mit **Strg+S** in **einer Transaktion** geschrieben,
  bei Fehler Rollback; Esc verwirft
- Zeilen-Identifikation über PRIMARY KEY, sonst ersten UNIQUE-Index, sonst Vollvergleich mit `LIMIT 1`
- Daten laufen über Prepared-Statement-Parameter (kein String-Escaping, keine SQL-Injection über Zellwerte)
- Zahlen rechtsbündig, NULL grau, BLOB/Binär als Hex (schreibgeschützt)
- **Detailbereich** unter dem Grid für mehrzeilige oder lange Texte; Binärwerte werden als Bild
  (PNG, JPEG, GIF, BMP, WebP, ICO, TIFF) oder als Hex-Dump angezeigt

**Tabellenentwurf**
- Neue Tabellen anlegen und bestehende ändern, mit den Reitern wie in Navicat:
  Felder, Indizes, Fremdschlüssel, Trigger, Optionen, Partitionierung, Kommentar, SQL-Vorschau
- Es wird nur die Differenz als `ALTER TABLE` ausgeführt; die SQL-Vorschau zeigt sie vorher an

**SQL-Editor**
- Syntax-Highlighting, Ausführen (Strg+R / F9), aktuelles Statement bzw. Auswahl (Strg+Enter)
- Mehrere Statements, `DELIMITER` wird unterstützt, ein Ergebnis-Tab pro Result-Set, Meldungen mit Laufzeiten
- Abbrechen per `KILL QUERY` (Stopp-Knopf), „Bei Fehler fortfahren“, Datenbankauswahl pro Tab
- Detailbereich unter dem Ergebnis wie im Datengrid: lange Texte mehrzeilig, Binärwerte als Bild oder Hex-Dump
- **Gespeicherte Abfragen** pro Verbindung und Datenbank (Strg+S, „Speichern unter …“), im Baum unter *Abfragen*

**Import & Export**
- **SQL-Dump** wie `mysqldump` – eingebaut, kein externes Programm nötig – für eine ganze Datenbank oder eine
  einzelne Tabelle/Ansicht (Kontextmenü im Baum): Struktur und/oder Daten, `DROP … IF EXISTS`,
  `CREATE DATABASE`, Ansichten, Trigger, Prozeduren/Funktionen, Ereignisse, mehrzeilige INSERTs (max. 1 MB),
  konsistenter Stand per Transaktion (`--single-transaction`), DEFINER optional entfernen.
  Einspielen mit `mariadb < datei.sql` oder im SQL-Editor
- **CSV-Export** einer ganzen Tabelle/Ansicht (Kontextmenü im Baum oder Button im Datengrid) oder eines
  Abfrageergebnisses: Trenner `;`, Zeilenende LF, Werte in Anführungszeichen, BOM wählbar.
  Tabellen werden auf eigener Verbindung direkt in die Datei gestreamt, mit Fortschritt und Abbrechen
- **Import-Assistent für CSV/TXT:** erkennt Codierung, Zeilen- und Spaltentrenner, zeigt eine Vorschau
  (20 Zeilen), importiert in eine neue oder bestehende Tabelle mit Feldzuordnung (Felder überspringbar);
  der Import läuft in einer Transaktion

**Strukturabgleich**
- Zwei Datenbanken vergleichen – auch auf verschiedenen Verbindungen (Werkzeugleiste *Abgleich* oder
  Kontextmenü einer Datenbank)
- Verglichen werden Tabellen, Ansichten, Prozeduren, Funktionen und Ereignisse; Liste mit Status
  (gleich, unterschiedlich, nur in Quelle, nur im Ziel) und Auswahl per Häkchen
- **Unterschied anzeigen:** zeilenweiser Vergleich der Struktur von Quelle und Ziel
- Erzeugt das Skript, das das Ziel an die Quelle angleicht: `ALTER TABLE` für Felder, Primärschlüssel, Indizes,
  Fremdschlüssel, Trigger, Optionen und Partitionierung, `CREATE TABLE` für fehlende Tabellen,
  optional `DROP TABLE`; `AUTO_INCREMENT` bleibt unberührt.
  Ansichten werden per `CREATE OR REPLACE VIEW` ersetzt, Routinen und Ereignisse per `DROP … IF EXISTS` + `CREATE`
  (verglichen ohne DEFINER und ohne Vorsatz der eigenen Datenbank)
- Skript kopieren, im SQL-Editor der Zieldatenbank öffnen oder nach Rückfrage **direkt ausführen**
  (je Objekt ein eigener Schritt; erfolgreich abgeglichene Einträge verschwinden aus der Liste)

**Benutzer-Manager**
- Benutzer anlegen, umbenennen (Benutzer/Host), Passwort setzen, löschen
- Rechte als Checkboxen auf drei Ebenen: **Server (`*.*`)**, **Datenbank (`db.*`)**, **Tabelle (`db.tabelle`)**,
  jeweils mit GRANT OPTION
- Es werden nur die Unterschiede als `GRANT`/`REVOKE` ausgeführt; die SQL-Vorschau zeigt sie vorher an
- Die Liste der Serverrechte kommt aus `SHOW PRIVILEGES`, passt also zur jeweiligen MariaDB-Version

**Servermonitor** (wie mytop)
- Kennzahlen: Laufzeit, Abfragen gesamt und pro Sekunde, Threads, Verbindungen, langsame Abfragen,
  Anteil Select/Insert/Update/Delete, Key-Effizienz, Buffer-Pool-Trefferquote, Netzwerkdurchsatz
- Prozessliste mit automatischer Aktualisierung (1–60 s), Sortierung, Filter, Leerlauf ausblenden;
  lange laufende Abfragen farbig hervorgehoben
- Vollständige Abfrage des gewählten Prozesses, `EXPLAIN` (`SHOW EXPLAIN FOR` bzw. `EXPLAIN` auf den Text)
- Abfrage abbrechen (`KILL QUERY`), Verbindung beenden (`KILL`), alle Verbindungen eines Benutzers beenden –
  jeweils mit Rückfrage
- Eigene Reiter für Statuszähler (mit Änderung pro Sekunde, `FLUSH STATUS`) und Servervariablen;
  Variablen per Doppelklick ändern (`SET GLOBAL`, Vorschau der Anweisung, gilt bis zum Server-Neustart)

**Oberfläche**
- Hell/Dunkel folgt dem System (oder fest einstellbar), Schriftgröße und Zeilenhöhe im Datengrid einstellbar
- Schriftart wählbar (Optionen): Arial, Tahoma, Verdana, Microsoft Sans Serif, Segoe UI u. a. – je nachdem,
  was auf dem System installiert ist (Linux: Liberation Sans, DejaVu Sans, Noto Sans …); farbige Icons im Navicat-Stil
- Schriftdarstellung „Scharf“ (Standard): Schriftgrößen je Monitor auf ganze Pixel gerundet (z. B. 13 px bei 100 %,
  16 px bei 125 %), Glyphen auf ganzen Pixeln, kräftigere Kanten. ClearType-Subpixel-Glättung wie im Fenstertitel
  kann egui nicht; abschaltbar für die weichere egui-Darstellung
- Zwischenablage (Kopieren/Einfügen) in allen Eingabefeldern
- Offene Tabs (Daten, Abfragen inkl. ungespeichertem Text, Entwurf, Benutzer, Monitor, Abgleich) werden beim
  Schließen gespeichert und beim nächsten Start wiederhergestellt

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
| Strg+S | Änderungen übernehmen / Abfrage bzw. Tabellenentwurf speichern |
| Esc | Änderungen verwerfen |
| Strg+C | Zellwert kopieren |

(Auf macOS jeweils ⌘ statt Strg.)

## Bauen

Voraussetzung: Rust (stable, https://rustup.rs).

```sh
cargo build --release
# Programm: target/release/mariadb-manager (bzw. .exe)
```

In RustRover liegen fertige Run-Konfigurationen bereit (`.idea/runConfigurations/`):
*Starten (Debug)*, *Starten (Release)*, *Release bauen* und *Tests*. Solange das Programm läuft,
ist die `.exe` unter Windows gesperrt – vor dem Bauen beenden.

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

Gespeicherte Abfragen liegen daneben als `.sql`-Dateien unter `queries/<Verbindung>/<Datenbank>/`.

Passwörter werden nur gespeichert, wenn „Passwort speichern“ aktiv ist – dann im Klartext
(unter Linux/macOS mit Dateirechten 600). Für sensible Server das Häkchen weglassen.

## Noch nicht enthalten (Ideen für die nächsten Versionen)

SQL-Datei einspielen (ohne Umweg über den Editor), SSH-Tunnel, Autovervollständigung im Editor,
Rechte auf Spalten- und Routinenebene, Rollen, Passwortspeicherung im System-Schlüsselbund.
