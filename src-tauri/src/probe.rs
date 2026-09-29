//! Dev-only probe: spawns N small always-on-top windows loading the app,
//! to measure per-window WebView2 memory overhead (Task 2 / Plan 1).
//! Build with `--features probe`; never used in normal runs.

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

pub fn spawn(app: &tauri::AppHandle, n: u32) {
    for i in 0..n {
        let label = format!("probe-{i}");
        if app.get_webview_window(&label).is_some() {
            continue;
        }
        WebviewWindowBuilder::new(app, &label, WebviewUrl::App("index.html".into()))
            .decorations(false)
            .always_on_top(true)
            .inner_size(200.0, 260.0)
            .build()
            .expect("probe window");
    }
}
