# Spike: Slint native-UI RAM probe

**Date:** 2026-09-11 (revised same day after renderer-backend + multi-window testing)
**Method:** `spikes/slint-probe`, release build, slint `=1.9.0`. Two workloads: (a) one window rendering 20 note cards; (b) 20 separate small windows, one card each — the MS-Sticky-Notes shape. Each card mimics our content model: bold title line, a text line with a date-chip pill, two checklist rows (one checked). Measured via CIM `WorkingSetSize`, 8–12 s after launch (steady state). All three renderers were compiled into the binary (1.9 default features); switching is a runtime env var, `SLINT_BACKEND`.

## Numbers

| `SLINT_BACKEND` | RAM (20 cards, 1 window) | CPU in first 8 s |
|---|---|---|
| *(unset — 1.9 default)* | 82.4 MB | 11.2 s |
| **`winit-software`** | **49.1 MB** | 18.6 s |
| `winit-femtovg` | 83.7 MB | 15.1 s |
| `winit-skia` | 74.1 MB | 2 s |

**Superseded early readings:** an initial 112–155 MB figure was an artifact of measuring immediately after first render (before Windows releases dirty pages to the standby list); steady state is the table above. The default backend on this machine is not Skia, so the earlier "Skia floor" theory was wrong.

## Floated windows (20 note windows, one card each — the MS-Sticky-Notes shape)

| `SLINT_BACKEND` | RAM (20 windows) |
|---|---|
| **`winit-software`** | **54.5 MB** (+5.5 MB over the 1-window case → ~0.3 MB per extra window) |
| `winit-skia` | 142.8 MB |
| *(unset)* | 144.1 MB |

## Comparison (all = one app, 20 notes)

| Stack | Build | RAM |
|---|---|---|
| Microsoft Sticky Notes (user reference) | release | ~70–80 MB |
| Tauri board-only + 20 floated windows (multi-window-ram spike) | debug | ~575 MB |
| **Slint 1.9, software renderer, 20 windows** | **release** | **~54.5 MB** |

## Analysis

- With the **software renderer**, Slint's footprint for 20 notes — even as 20 separate floated windows in one process — is **~25% below MS Sticky Notes' own number** and ~1/10 of Tauri's. Marginal cost per extra window measured ~0.3 MB (small pixel buffer + scene graph; no per-window GPU context).
- The cost of the software renderer is CPU, not RAM (software rasterization: ~2× CPU time during active rendering in the 1-window run; a mostly-static notes app sits near 0% idle).
- Femtovg measured worst (83.7 MB 1-window) and has an open Windows memory-leak report under repeated redraws — excluded.
- Open product-cost items for Slint remain: a custom text-layout engine for inline chips/bold runs (TextEdit is plain-text only), and Slint 1.9 API churn (no `ScrollViewer`/`viewport`; `color`→`background`; no `font-color`; no window position setters).

## Verdict

**PASS (footprint), with conditions** — reversed from the earlier FAIL, which was based on a non-steady-state measurement and the wrong default renderer. Slint + software renderer (~49 MB single window, 54.5 MB with 20 floated windows) beats both the MS-Sticky-Notes reference and Tauri on every measured axis. Decision inputs still needed:

1. ~~**Multi-window probe**~~ done — ~0.3 MB per extra floated window (table above).
2. **Soak test** (30+ min idle, 20 windows, software backend) to rule out slow leaks — running, results to be appended.
3. **Editor feasibility assessment** — inline mixed-format text + chips in a custom Slint text layout remains the largest engineering unknown (and the main reason Slint was discounted earlier).