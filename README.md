# mailarchive

Sehr kompakter IMAP → Maildir-Sync in Rust (pull-only, wie `mbsync`), inkrementell und
eventbasiert (IMAP IDLE), für beliebig viele Accounts.

- Ein JSON-Config listet die Accounts; jeder bekommt ein eigenes Verzeichnis, eine eigene
  Verbindung und einen eigenen Thread: `<maildir>/<name>/INBOX/{cur,new,tmp}`.
- Jeder IMAP-Ordner wird lokal ein Maildir, verschachtelt nach der Server-Hierarchie
  (`Archive/2024` → `<name>/Archive/2024/`).
- Mails werden unverändert als Text (RFC 5322) gespeichert, Dateiname im mbsync-Stil
  (`…,U=<uid>:2,<flags>`); `U=` ist hier die IMAP-UID. Ein fremdes Maildir (Dateien ohne
  `.uidvalidity`) wird deshalb weder übernommen noch angefasst, nur gemeldet.
- Pro Ordner merkt sich `.uidvalidity`, zu welcher UIDVALIDITY die UIDs im Verzeichnis
  gehören; sie wird beim ersten Kontakt geschrieben, damit eine abgebrochene Erst-Sync beim
  nächsten Lauf weiterläuft. Was fehlt, ergibt sich aus dem Vergleich Verzeichnis ↔ Server
  (`UID FETCH 1:*`), Flags werden gespiegelt (`sync_flags`, Standard an).
- **Archiv, kein Spiegel:** verschwindet eine Mail serverseitig, bleibt die lokale Kopie
  liegen. Nur mit `"expunge": true` wird sie auch lokal gelöscht — und auch dann nur, wenn
  der Server tatsächlich Mails aufgezählt hat; ein bloßes `0 EXISTS` leert das Archiv nie
  (ein absichtlich geleerter Ordner wird lokal also nie geleert).
- Ändert sich UIDVALIDITY, wird der Ordner **nicht** angefasst (kein Neuholen, kein
  Löschen) und eine Warnung ausgegeben — die Entscheidung trifft der Mensch: Ordner
  beiseite schieben, dann holt der nächste Lauf ihn frisch.
- Jede Mail wird nach `tmp/` geschrieben, `fsync`t und erst dann unter ihren endgültigen
  Namen verschoben (Verzeichnis-`fsync` inklusive). Ein Stromausfall kann so keine leeren
  Dateien mit gültigem Namen hinterlassen.
- Ein Ordner, den der Server nicht öffnen lässt (`NO`/`BAD`, z.B. Shared Folder ohne
  Rechte), wird mit Meldung übersprungen, die übrigen laufen weiter. Ordner, deren Name
  kein gültiges Verzeichnis unterhalb des Accounts ergibt oder mit einem anderen
  kollidiert, ebenso.
- **Hooks:** `hooks.mail_received` ist ein Shell-Kommando, das nach jeder gespeicherten Mail
  läuft (auch beim Erst-Sync für jede Mail); `%%mail_path%%` wird durch den bereits
  shell-quotierten Pfad ersetzt. Ein fehlschlagender Hook wird nur gemeldet.
- Nach dem Sync wartet jeder Account per IDLE auf seiner INBOX; bei Ereignis wird die INBOX
  sofort synchronisiert, nach Ablauf von `idle_secs` alle Ordner (inkl. neu angelegter).
  Verbindungsabbrüche → Reconnect mit Backoff (30 s … 15 min bei sofortigem Scheitern,
  z.B. falschem Passwort). Außerhalb von IDLE gilt ein Socket-Timeout von 120 s, damit
  eine halboffene Verbindung den Account nicht ewig blockiert.

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

```jsonc
{
  "maildir": "~/Mail", // Kommentare (// und /* */) sind erlaubt
  "idle_secs": 1500,
  "accounts": [
    { "name": "web.de", "host": "imap.web.de", "user": "me@web.de", "pass_env": "WEBDE_PASS" }
  ],
  "hooks": { "mail_received": "notmuch new && notify-send Mail %%mail_path%%" }
}
```

Pro Account: `name` (auch der Verzeichnisname), `host`, `user`, optional `port` (993),
`tls` (true = implizites TLS wie auf Port 993; STARTTLS wird nicht unterstützt), `folders`
(leer = alle), `expunge` (false), `sync_flags` (true). Unbekannte Felder sind ein Fehler,
damit Tippfehler nicht still ignoriert werden; ebenso ein `--account`, den es nicht gibt.

Das Passwort kommt aus `pass_cmd` (Kommando, das es ausgibt) > `pass_env` (Name einer
Umgebungsvariable) > `pass` (direkt in der Datei, dann `chmod 600`) > `MAILARCHIVE_PASS_<NAME>`.
Eine `.env` im Arbeitsverzeichnis wird vor dem Lesen der Config geladen.

## Releases

Jeder Push auf `main` baut Release-Binaries für Linux amd64 und macOS arm64 (Apple Silicon)
als Workflow-Artefakte; ein Tag `v*` hängt sie zusätzlich an ein GitHub Release. Das
Linux-Binary linkt OpenSSL dynamisch (`libssl.so.3`).

## Aufbau

| Datei | Inhalt |
| --- | --- |
| `src/main.rs` | CLI, Config laden, ein Thread pro Account |
| `src/config.rs` | JSON-Config (mit Kommentaren), Validierung, Passwortquellen, Hooks |
| `src/sync.rs` | Verbindung, IDLE-Schleife, Ordner-Sync |
| `src/maildir.rs` | Dateinamen, Flags, `scan`, durable writes |

`cargo test` fährt den echten Binary gegen gescriptete Fake-IMAP-Server: verschwundene
Mails bleiben, UIDVALIDITY-Wechsel löscht nichts, abgebrochene Erst-Sync läuft weiter,
kaputte Ordner blockieren die anderen nicht, `0 EXISTS` leert nichts, Login-Fehler werden
nicht gehämmert; dazu Unit-Tests für Config (inkl. Kommentare und Hooks), Maildir-Pfade und Backoff.
