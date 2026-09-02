# mailarchive

Sehr kompakter IMAP → Maildir-Sync in Rust (pull-only, wie `mbsync`), inkrementell und
eventbasiert (IMAP IDLE).

- Jeder IMAP-Ordner wird lokal ein Maildir (`cur/`, `new/`, `tmp/`), verschachtelt nach
  der Server-Hierarchie (`Archive/2024` → `Archive/2024/`).
- Mails werden unverändert als Text (RFC 5322) gespeichert, Dateiname mbsync-kompatibel
  (`…,U=<uid>:2,<flags>`).
- Pro Ordner merkt sich `.uidvalidity` UIDVALIDITY und höchste UID; Flags und Löschungen
  werden gespiegelt, bei geänderter UIDVALIDITY wird der Ordner neu geholt.
- Nach dem Sync wartet der Client per IDLE auf INBOX; bei Ereignis wird INBOX sofort
  synchronisiert, nach Ablauf von `idle_secs` alle Ordner. Verbindungsabbrüche → Reconnect.

## Nutzung

```sh
cp config.example.toml config.toml   # anpassen
cargo run --release -- config.toml
```

Ein einziger Prozess, keine Datenbank, ~200 Zeilen in `src/main.rs`.
