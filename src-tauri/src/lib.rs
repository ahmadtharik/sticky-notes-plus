mod commands;
mod db;
#[cfg(test)]
mod fts_probe_scratch;
mod service;

#[cfg(feature = "probe")]
mod probe;

use std::sync::{Arc, Mutex};

use tauri::{Manager, WindowEvent};

/// Shared database handle for all commands (Arc so services can clone it).
pub type DbState = Arc<Mutex<db::Db>>;

#[derive(serde::Serialize)]
struct AppInfo {
    version: String,
    app_data_dir: String,
}

#[tauri::command]
fn get_app_info(app: tauri::AppHandle) -> Result<AppInfo, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(AppInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        app_data_dir: data_dir.to_string_lossy().to_string(),
    })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            // Keep the board's native window alive when the user clicks its
            // close button. Notes use it as their shared list window; hiding
            // avoids destroying its WebView and lets "Notes list" restore it.
            if let Some(board) = app.get_webview_window("board") {
                let board_for_events = board.clone();
                board.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = board_for_events.hide();
                    }
                });
            }
            // Open the database (app-data dir) once, at startup; every command
            // reaches it through State<DbState>.
            let data_dir = app.handle().path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            app.manage(DbState::new(Mutex::new(db::Db::open_in_dir(&data_dir)?)));

            #[cfg(feature = "probe")]
            {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    probe::spawn(&handle, 20);
                });
            }
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_app_info,
            commands::create_note,
            commands::list_notes,
            commands::get_note,
            commands::save_note,
            commands::update_note_meta,
            commands::soft_delete,
            commands::restore_note,
            commands::purge_note,
            commands::save_window_geom,
            commands::open_note_window,
            commands::close_note_window,
            commands::destroy_note_window,
            commands::drag_note_window,
            commands::open_board,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
