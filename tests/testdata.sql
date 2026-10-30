-- Testdaten für die Integrationstests (MDBM_TEST_HOST=… cargo test)
SET NAMES utf8mb4;
DROP DATABASE IF EXISTS shop;
CREATE DATABASE shop CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
USE shop;
CREATE TABLE kunden (
  id INT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
  name VARCHAR(100) NOT NULL,
  ort VARCHAR(60),
  email VARCHAR(120),
  umsatz DECIMAL(12,2) DEFAULT 0,
  erstellt DATETIME DEFAULT CURRENT_TIMESTAMP,
  notiz TEXT,
  KEY (ort)
) COMMENT='Kundenstamm';
INSERT INTO kunden (name, ort, email, umsatz, notiz)
  SELECT CONCAT('Kunde ', seq), ELT(1 + seq % 6, 'Oberhausen', 'Essen', 'Köln', 'Düsseldorf', 'München', 'Berlin'),
         CONCAT('kunde', seq, '@example.de'), ROUND(RAND(seq) * 10000, 2), IF(seq % 7 = 0, NULL, 'Größe & Übergröße')
  FROM seq_1_to_200000;
CREATE TABLE bestellungen (
  id BIGINT AUTO_INCREMENT PRIMARY KEY,
  kunde_id INT UNSIGNED NOT NULL,
  datum DATE,
  betrag DECIMAL(10,2),
  status ENUM('offen','bezahlt','storniert') DEFAULT 'offen',
  FOREIGN KEY (kunde_id) REFERENCES kunden(id)
);
INSERT INTO bestellungen (kunde_id, datum, betrag, status)
  SELECT 1 + seq % 200000, DATE '2025-01-01' + INTERVAL seq % 600 DAY, ROUND(RAND(seq) * 500, 2), ELT(1 + seq % 3, 'offen', 'bezahlt', 'storniert')
  FROM seq_1_to_300000;
CREATE TABLE log_ohne_pk (zeit DATETIME, msg VARCHAR(200));
INSERT INTO log_ohne_pk VALUES (NOW(), 'Start'), (NOW(), 'Läuft'), (NULL, 'kein Datum');
CREATE TABLE dateien (id INT PRIMARY KEY, name VARCHAR(50), inhalt BLOB);
INSERT INTO dateien VALUES (1, 'a.bin', UNHEX('DEADBEEF00FF')), (2, 'leer', NULL);
CREATE VIEW v_umsatz_ort AS SELECT ort, COUNT(*) AS anzahl, SUM(umsatz) AS umsatz FROM kunden GROUP BY ort;
DELIMITER $$
CREATE FUNCTION brutto(n DECIMAL(10,2)) RETURNS DECIMAL(10,2) DETERMINISTIC RETURN n * 1.19$$
CREATE PROCEDURE top_kunden(IN lim INT) BEGIN SELECT * FROM kunden ORDER BY umsatz DESC LIMIT lim; END$$
DELIMITER ;
CREATE EVENT aufraeumen ON SCHEDULE EVERY 1 DAY DO DELETE FROM log_ohne_pk WHERE zeit < NOW() - INTERVAL 30 DAY;
ANALYZE TABLE kunden, bestellungen;
