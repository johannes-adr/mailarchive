# mailarchive

Sehr kompakter IMAP → Maildir-Sync in Rust (pull-only, wie `mbsync`), inkrementell und
eventbasiert (IMAP IDLE), für beliebig viele Accounts.

- Ein JSON-Config listet die Accounts; jeder bekommt ein eigenes Verzeichnis, eine eigene
  Verbindung und einen eigenen Thread: `<maildir>/<name>/INBOX/{cur,new,tmp}`.
- Jeder IMAP-Ordner wird lokal ein Maildir, verschachtelt nach der Server-Hierarchie
  (`Archive/2024` → `<name>/Archive/2024/`).
- Mails werden unverändert als Text (RFC 5322) gespeichert, Dateiname mbsync-kompatibel
  (`…,U=<uid>:2,<flags>`).
- Pro Ordner merkt sich `.uidvalidity` UIDVALIDITY und höchste UID; Flags werden
  gespiegelt (`sync_flags`, Standard an).
- **Archiv, kein Spiegel:** verschwindet eine Mail serverseitig, bleibt die lokale Kopie
  liegen. Nur mit `"expunge": true` wird sie auch lokal gelöscht.
- Ändert sich UIDVALIDITY, wird der Ordner **nicht** angefasst (kein Neuholen, kein
  Löschen) und eine Warnung ausgegeben — die Entscheidung trifft der Mensch: Ordner
  beiseite schieben, dann holt der nächste Lauf ihn frisch.
- Jede Mail wird nach `tmp/` geschrieben, `fsync`t und erst dann unter ihren endgültigen
  Namen verschoben (Verzeichnis-`fsync` inklusive); die Statusdatei wird erst danach
  aktualisiert. Ein Stromausfall kann so keine leeren Dateien mit gültigem Namen
  hinterlassen.
- Nach dem Sync wartet jeder Account per IDLE auf seiner INBOX; bei Ereignis wird die INBOX
  sofort synchronisiert, nach Ablauf von `idle_secs` alle Ordner. Verbindungsabbrüche →
  Reconnect.

## Nutzung

```sh
cp config.example.json config.json   # anpassen
cp .env.example .env                 # Passwörter
cargo run --release -- --input-config config.json
```

`--input-config` ist Standard `config.json`, `--help` zeigt alle Flags (`--maildir`
überschreibt das Wurzelverzeichnis, `--account web.de,gmail` startet nur einzelne
Accounts).

### Config

```json
{
  "maildir": "~/Mail",
  "idle_secs": 1500,
  "accounts": [
    { "name": "web.de", "host": "imap.web.de", "user": "me@web.de", "pass_env": "WEBDE_PASS" }
  ]
}
```

Pro Account: `name` (auch der Verzeichnisname), `host`, `user`, optional `port` (993),
`tls` (true), `folders` (leer = alle), `expunge` (false), `sync_flags` (true). Unbekannte
Felder sind ein Fehler, damit Tippfehler nicht still ignoriert werden.

Das Passwort kommt aus `pass_cmd` (Kommando, das es ausgibt) > `pass_env` (Name einer
Umgebungsvariable) > `pass` (direkt in der Datei, dann `chmod 600`) > `MAILARCHIVE_PASS_<NAME>`.
Eine `.env` im Arbeitsverzeichnis wird vor dem Lesen der Config geladen.

## Aufbau

| Datei | Inhalt |
| --- | --- |
| `src/main.rs` | CLI, Config laden, ein Thread pro Account |
| `src/config.rs` | JSON-Config, Validierung, Passwortquellen |
| `src/sync.rs` | Verbindung, IDLE-Schleife, Ordner-Sync |
| `src/maildir.rs` | Dateinamen, Flags, `scan`, durable writes |

`cargo test` fährt den echten Binary gegen zwei Fake-IMAP-Server und prüft, dass nichts
gelöscht wird; dazu Unit-Tests für Config und Maildir.
