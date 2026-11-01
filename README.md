# Sticky Notes+

A Windows sticky notes app with reminders that sync one way to Google Calendar,
so alerts reach your phone when you're away from your PC.

## Install

Download `StickyNotesPlus.msi` from the
[latest release](https://github.com/ahmadtharik/sticky-notes-plus/releases/latest)
and run it. The installer isn't code-signed yet, so Windows may show a
SmartScreen warning. Later updates can be installed from **Settings > Updates**.

## Features

- Sticky notes with rich text, checklists and images
- Reminders with Windows notifications, snooze and repeat
- Optional Google Calendar sync: reminders with a time become events in a
  dedicated "Sticky Notes" calendar
- Dark mode, a global new-note shortcut and Start with Windows

Privacy policy: https://ahmadtharik.github.io/sticky-notes-plus/privacy.html

## Build from source

The app is written in Rust with Slint.

```powershell
cargo test
cargo build --release --locked
.\packaging\build.ps1   # MSI, requires WiX 4
```

Google Calendar sync needs your own Google OAuth desktop client. Set
`STICKY_NOTES_GOOGLE_CLIENT_ID` and `STICKY_NOTES_GOOGLE_CLIENT_SECRET` before
building.

## Releasing

Update `version` in `Cargo.toml`, commit, then push a matching tag such
as `v0.2.0`. GitHub Actions builds the installer and publishes the release. It
needs the two variables above as repository secrets.

## License

[MIT](LICENSE)
