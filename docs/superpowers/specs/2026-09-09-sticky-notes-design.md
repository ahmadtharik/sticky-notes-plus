# Sticky Notes+ — Design

**Date:** 2026-09-09
**Status:** Approved for implementation planning

A sticky-notes application for Windows modeled on Microsoft Sticky Notes, extended with checklists, inline date chips with reminders, and optional Google Calendar mirroring. Windows-first, lightweight, 24/7 background operation, with the door left open for later macOS/Linux expansion. (Open-sourcing may happen later and does not constrain this design.)

---

## 1. Goals & constraints

- Feel like MS Sticky Notes: a board of notes + notes detachable into floating windows, tray-resident, reminders that are never missed.
- Lightweight: the app should be able to run 24/7. MS Sticky Notes idles at ~12 MB RAM; our realistic target is the "unnoticeable in Task Manager" band — a key driver of the stack choice (§2) and a risk to validate by early spike (§9).
- Windows 11 first (WebView2/Win32 toasts). No bundled runtime: use the OS-provided WebView2 (built into Win11, auto-updating evergreen runtime elsewhere).
- Reminder precision is the minute; the PC-off edge case is handled by catch-up, not by polling.

## 2. Stack: Tauri 2 (Rust core + web frontend)

- **Rust core** owns everything logic-heavy: SQLite storage, reminder engine, Windows toasts (Win32), tray icon, global hotkey, start-with-Windows registration, Google Calendar API client (OAuth), DPAPI token encryption.
- **Web frontend** (WebView2): board window (note grid, search, archive) and per-note floating windows. Rich text via TipTap/ProseMirror — mixed formatting, interactive inline nodes (date chips), image inserts, checklist rendering.
- Rejected alternatives:
  - **Pure-Rust native UI (Slint/Iced/egui):** lightest RAM, but no mature rich-text-with-inline-interactive-nodes story — we'd build a text editor from scratch. Slint is also AGPL-licensed.
  - **Electron:** too heavy (200 MB+) for the 24/7 goal.
  - **Tauri with no framework:** viable but we'd hand-roll DOM work that a small compiled framework (Svelte-scale) absorbs cheaply. Framework choice is settled in the implementation plan; the architecture doesn't depend on it.

## 3. Feature set (v1)

**Notes**
- Board window: grid of all notes; single board (no multiple boards); archive; soft-deleted view.
- Floating windows: any note can detach into its own frameless, always-on-top window; detach by drag or "pop out" button; re-attach by "collapse to board" or dragging onto the board. Window positions/sizes persisted.
- Content: rich text (bold/italic/underline, text color/size within a note), pastel note-color palette per note, images (paste/insert), checklist items.
- Pin (top of board) and archive per note. Archived notes **still fire reminders** (out of sight ≠ out of mind). Delete is soft (hidden from board/search; restorable/purgeable from a "Deleted" view in settings).
- Search: board search box, live filtering over title + content via SQLite FTS5.

**Checklists**
- Items inside a note; toggle complete; items order-stable.
- Completing an item is a first-class event: it cancels any reminders attached to that item (see §6).

**Date chips & reminders** (the core extension)
- Trigger: `/` slash-command menu in the editor (Date & time, Checklist item, Insert image) — the MS Sticky Notes convention; `Ctrl+D` is an alias. No auto-detection of keywords like "by".
- A chip is an **inline pill** (e.g. `📅 Fri, Sep 18 · 5:00 PM`) that is structured data, not text. Click re-opens the picker to edit; small × removes. Multiple chips per line allowed.
- Binding is positional and automatic: chip inside a checklist item ⇒ reminder belongs to the item; chip in plain text ⇒ belongs to the note.
- When due: native Windows toast (works from tray/hidden), actions **Open note** (reveal app, open the note window, highlight the item) and **Done** (complete the item / dismiss). One toast per due time — no re-nagging.
- Recurring: daily/weekly per chip; keeps firing until the item is checked or the chip is removed.
- Missed (PC/app off at due time): on startup and on system wake, one summary toast ("N reminders were missed"); affected chips enter the `overdue` state.
- Overdue is a **derived, visual-only** state for live-missed reminders: red chip accent + "⚠ n overdue" badge on the board card; clears when the item is completed. Overdue items are never re-notified.

**Google Calendar sync (optional, built last in v1)**
- One-time OAuth (browser flow, localhost redirect); refresh token stored encrypted (Windows DPAPI).
- When a timed reminder is created with mirroring enabled (global toggle or per chip), a matching Google Calendar event is created with a notification at the same time — giving a second notification channel, including phone push.
- Recurring chips ⇒ recurring events; item completion or "Done" cancels the event.
- Isolated module: only the reminder engine and settings touch it; the rest of the app is unaware of Google. Offline ⇒ local toasts remain the always-working baseline.

**App behavior**
- System tray (left-click toggles board; right-click: New note, Show board, Google account status/connect/disconnect, Settings, Exit). Icon accented when anything is overdue.
- Hide-to-tray on window close (setting, default on).
- Start with Windows (setting, default off): starts hidden in tray.
- Global hotkey Ctrl+Alt+S (rebindable): new note window with focus ready to type.
- Theme: follows Windows light/dark by default; manual override in settings.
- Single-instance: a second launch focuses the existing app.
- Explicit Exit: flush saves, cancel scheduled wakes, close windows.

## 4. Architecture

One process, two layers, one database:

- **Rust core:** storage (SQLite), reminder engine (exact-time scheduler + safety rescan), platform layer (toasts, tray, hotkey, startup registration), calendar module.
- **Web frontend:** board window + per-note windows; all persistence/notifications flow through typed Tauri commands/events.
- **One SQLite file** in the app-data folder is the single source of truth.

Key decisions:
1. Note content is stored **structured** (blocks/runs/chips, not raw HTML) so the reminder engine and reconciliation never parse text.
2. The **reminder engine lives in Rust**, not the UI — it keeps working with all windows hidden/closed.
3. Windows are a thin concern: many small Tauri windows; geometry persisted.
4. **Calendar sync is an isolated module** speaking only to the reminder engine and settings.

## 5. Data model

```
meta                   -- schema_version, last catch-up scan time (idempotent startup)
settings               -- key → JSON value (theme, hotkey, start-with-Windows,
                         hide-to-tray, catch-up notifications on/off, mirroring default)
calendar_connections   -- id, account_email, refresh_token (DPAPI-encrypted),
                         access_token (encrypted cache), token_expires_at, mirror_reminders
notes                  -- id, title, content (JSON blocks), color, is_pinned,
                         is_archived, deleted_at (soft delete),
                         window_x/y/w/h, is_floating, created_at, modified_at
date_chips             -- id, note_id, item_id (NULL ⇒ note-level), due_at,
                         repeat (none|daily|weekly), state (pending|fired|done|overdue),
                         last_fired_at, calendar_event_id
checklist_items        -- id, note_id, text, checked, checked_at, sort_order
attachments            -- id, note_id, file_name, mime_type, file_size, data_path, created_at
notes_fts              -- FTS5 virtual table over title + derived plain text
```

**Note content JSON** — a list of blocks:

```json
{ "blocks": [
  { "type": "text", "runs": [
      { "t": "Finish report", "bold": true },
      { "t": " by " },
      { "chip": "<chip-id>" }
  ]},
  { "type": "checklist", "items": [
      { "id": "item-1", "text": "draft", "checked": true },
      { "id": "item-2", "text": "review", "chip": "<chip-id>" }
  ]},
  { "type": "image", "ref": "<attachment-id>" }
]}
```

**Dual-source-of-truth rule:** chips and item state each exist as (a) metadata rows (queryable; source of truth for reminding/completion) and (b) references inside the content JSON (position; source of truth for rendering). **Every save reconciles both directions**: JSON-referenced chip missing from `date_chips` → insert; row not referenced → mark `done`/delete. Same for items: a `checklist_items` row whose item id no longer appears in the JSON is deleted, taking any chips attached to it with it (mark `done`). The two can never drift.

**Derived, never stored:** overdue (chip due + not done), search index text, "n overdue" badge counts.

## 6. Reminder engine

**No fixed polling interval.** The engine keeps upcoming chips sorted by due time and sleeps until the *next exact due time* (minus a 1–2 s buffer), fires, then resleeps. A 1-minute safety rescan additionally runs as a net for edge cases (PC asleep, clock change, app just started). Users get exact-minute firing with no user-configurable interval; granularity is the minute, as picked in the picker.

**Chip state machine:**

```
                 ┌──────────┐  due time reached, app running
                 │ pending  │──────────────────────────────►  toast  ──► fired
                 └────┬─────┘                                   │
                      │ app/PC off at due time                  │ item checked /
                      ▼                                          │ chip removed
                 ┌──────────┐  startup/wake catch-up            ▼
                 │ overdue  │◄── one summary toast, chips marked ┌─────────┐
                 │ (missed) │                                    │  done   │
                 └──────────┘                                    └─────────┘
 Recurring chips: on fire (live or catch-up) re-enter `pending` with the
 next due time — exactly one notification per occurrence. A recurring chip
 missed while off fires one catch-up notice, then reschedules from now (no
 back-filling).
```

**Rules**
- One toast per due time, ever. Live-overdue items are never re-notified; they stay visually red until resolved.
- Completion is the terminal transition: item checked (by user or toast "Done") ⇒ all chips on it → `done`, recurring stops, mirrored Google event cancelled — one transaction.
- Chip edited (new time) while pending ⇒ reschedule; while fired/overdue ⇒ behaves as a new pending occurrence if moved to the future.
- Clock jumps forward while running are caught by the safety rescan (chip now in the past but still `pending` ⇒ missed path).

**Saves:** debounced autosave (~1 s after typing pause; also on focus loss / minimize / hide / exit) for text; **immediate** saves for first-class events (chip added/moved/removed, item toggled, pin, color). Crash safety via SQLite WAL; worst case after a crash is losing ≤ ~1 s of keystrokes — same as MS Sticky Notes.

## 7. App behavior & flows

**Startup:** single-instance check → open DB + migrate → catch-up scan (missed chips ⇒ summary toast) → restore state. Auto-start at login starts **hidden in tray** (quiet, reminders armed); explicit launch shows board + floating windows at saved positions.

**Tray:** left-click toggles board; right-click menu per §3. Exit = flush saves, cancel wakes, close all windows.

**Detach/attach:** drag card off grid (or pop-out button) → frameless always-on-top window at drop point, geometry persisted; "collapse to board" or drag onto board returns it to the grid, position remembered.

**Reminder end-to-end:** `/` → Date & time → chip inserted (immediate save; optional Google event) → engine wakes at the exact minute → toast (**Open note** / **Done**) → chip `fired` → *Open* reveals the app, opens the note window, highlights the item; *Done* or ticking the box completes the item, `done` chip, cancels recurring + Google event. Ignored ⇒ red/overdue on the board until resolved.

**Small flows:** new note (board button, `N`, or global hotkey) → blank note, cursor ready; search box live-filters the grid; delete ⇒ soft-deleted (restorable in settings "Deleted" view); archive ⇒ reversible, reminders keep running.

## 8. Testing strategy

- **TDD for the Rust core** (the logic-heavy parts): reminder engine (state machine, exact scheduling, catch-up, recurring, clock jumps), storage + reconciliation (the dual-source rule), soft delete/archive, settings.
- **Calendar module:** test against a mock/recorded Google API surface; token handling covered by unit tests.
- **UI:** hand-verified key flows plus automated tests for the critical ones (chip create/edit/remove, detach/attach, toast actions).

## 9. Risks & de-risking spikes (run first, at implementation start)

1. **Multi-window WebView2 RAM.** Many floating note windows may each carry renderer overhead; 10–20 windows could breach the lightweight goal. *Spike:* open 20 note windows, measure RAM. *Mitigations (only if needed):* cap concurrent floating windows, or render long-inactive notes board-only.
2. **Inline chips in a rich-text editor.** Standard pattern (TipTap inline node) but caret handling + popover anchoring are fiddly. *Spike:* one working chip in one editor, nothing else.

Both are throwaway probes that tune details; neither should change the architecture.

## 10. Build order (each phase ends with a usable app)

| # | Phase |
|---|---|
| 0 | Spikes: multi-window RAM probe; one chip in one editor |
| 1 | Skeleton: Tauri app, board window, SQLite + migrations, note CRUD (text only), debounced autosave |
| 2 | Rich text: formatting, note colors, checklist items, date-chip editor → full chip support |
| 3 | Floating windows: detach/attach, persisted geometry, always-on-top |
| 4 | Reminders: engine (exact scheduling + rescan), toasts, recurring, catch-up, overdue visuals, soft delete/archive |
| 5 | App behavior: tray, global hotkey, start-with-Windows, light/dark themes |
| 6 | Attachments (images: paste/insert, metadata) |
| 7 | Google Calendar sync (OAuth, token store, mirroring, event lifecycle) — intentionally last |