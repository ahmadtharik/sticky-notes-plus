# Spike: Multi-window WebView2 RAM probe

**Date:** 2026-09-11
**Method:** feature-gated probe (`cargo run --features probe` / `tauri dev --features probe`) spawns 20 frameless 200×260 always-on-top windows, each loading the full app page. Measured in **debug builds** via Task Manager / CIM `WorkingSetSize`, filtering WebView2 processes by our app's user-data dir (`com.sticky-notes.app`).

## Numbers

| Scenario | Processes | Total RAM |
|---|---|---|
| Board only (baseline) | 1 app + a few WebView2 | **~76 MB** (user-measured, Task Manager) |
| Board + 20 probe windows | 1 app + 21–26 WebView2 | **~575 MB** (21 × ~17.7–26 MB, user-measured) |

Derived: **≈ 25 MB per floating window** (debug build). WebView2's shared process model is in effect (fewer processes than windows).

**Reference (user-measured, same machine):** Microsoft Sticky Notes with 20 notes ≈ **70–80 MB total**. Our board-only baseline (~76 MB) roughly equals MS's footprint for 20 native notes; 20 floating windows costs ~575 MB here vs ~75 MB in MS. The gap is the webview rendering stack vs native WinUI — inherent to the Tauri choice, not fixable by optimization (release builds shave some, but not 7×).

## Analysis

- Tauri/WebView2 cannot approach MS Sticky Notes' ~12 MB (native WinUI); the realistic steady-state floor for this stack is ~76 MB (board) + ~25 MB per detached note, debug builds. Release builds typically measure lower.
- A comparable Electron build would be ~2–3 GB in the 20-window scenario — Tauri is still an order of magnitude lighter.
- 20 simultaneously detached windows is a worst case; typical use is the board plus a handful of active notes: 5 floating notes ≈ **~200 MB** (debug).

## Verdict

**OK, WITH mitigation (required in Plan 3).** The footprint is acceptable for 24/7 use at typical window counts, but unbounded floating windows would grow RAM without limit. Plan 3 must implement:

1. **Board-only rendering for inactive notes** — a detached note idle beyond a threshold (e.g. 30 min, or on board refocus) collapses back to the board, releasing its WebView window.
2. **Cap on concurrent floating windows** (suggested: 10) — exceeding it auto-collapses the least-recently-used notes.

Re-run this probe (feature `probe`) after Plan 3 lands to verify the cap/inactive-collapse behavior holds RAM flat with many notes.