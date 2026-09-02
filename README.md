# mailarchive

Sehr kompakter IMAP → Maildir-Sync in Rust (pull-only, wie `mbsync`), inkrementell und
eventbasiert (IMAP IDLE).

- Jeder IMAP-Ordner wird lokal ein Maildir (`cur/`, `new/`, `tmp/`), verschachtelt nach
  der Server-Hierarchie (`Archive/2024` → `Archive/2024/`).
- Mails werden unverändert als Text (RFC 5322) gespeichert, Dateiname mbsync-kompatibel
  (`…,U=<uid>:2,<flags>`).
- Pro Ordner merkt sich `.uidvalidity` UIDVALIDITY und höchste UID; Flags werden
  gespiegelt (`sync_flags`, Standard an).
- **Archiv, kein Spiegel:** verschwindet eine Mail serverseitig, bleibt die lokale Kopie
  liegen. Nur mit `expunge = true` wird sie auch lokal gelöscht.
- Ändert sich UIDVALIDITY, wird der Ordner **nicht** angefasst (kein Neuholen, kein
  Löschen) und eine Warnung ausgegeben — die Entscheidung trifft der Mensch: Ordner
  beiseite schieben, dann holt der nächste Lauf ihn frisch.
- Jede Mail wird nach `tmp/` geschrieben, `fsync`t und erst dann unter ihren endgültigen
  Namen verschoben (Verzeichnis-`fsync` inklusive); der Statusdatei-Update passiert erst
  danach. Ein Stromausfall kann so keine leeren Dateien mit gültigem Namen hinterlassen.
- Nach dem Sync wartet der Client per IDLE auf INBOX; bei Ereignis wird INBOX sofort
  synchronisiert, nach Ablauf von `idle_secs` alle Ordner. Verbindungsabbrüche → Reconnect.

## Nutzung

```sh
cp .env.example .env      # anpassen
cargo run --release
```

Konfiguriert wird über CLI-Flags (`--help` zeigt alle), jedes mit Env-Fallback
(`MAILARCHIVE_*`); eine `.env` im Arbeitsverzeichnis wird vorher eingelesen. Das Passwort
hat bewusst **kein** Flag — es stünde sonst in `ps` — sondern kommt aus `MAILARCHIVE_PASS`
oder aus dem Kommando hinter `--pass-cmd`.

```sh
mailarchive --host imap.web.de --user me@web.de --maildir ~/Mail/web.de --folders INBOX,Sent
```

Ein einziger Prozess, keine Datenbank, ~250 Zeilen in `src/main.rs`.
`cargo test` fährt den echten Binary gegen einen Fake-IMAP-Server und prüft, dass nichts
gelöscht wird.
