mod db;
mod service;
#[cfg(test)]
mod fts_probe_scratch;

#[cfg(feature = "probe")]
mod probe;

use std::sync::{Arc, Mutex};

use tauri::Manager;

/// Shared database handle for all commands (Arc so services can clone it).
pub type DbState = Arc<Mutex<db::Db>>;

#[derive(serde::Serialize)]
struct AppInfo {
  version: String,
  app_data_dir: String,
}

#[tauri::command]
fn get_app_info(app: tauri::AppHandle) -> Result<AppInfo, String> {
  let data_dir = app
    .path()
    .app_data_dir()
    .map_err(|e| e.to_string())?;
  Ok(AppInfo {
    version: env!("CARGO_PKG_VERSION").to_string(),
    app_data_dir: data_dir.to_string_lossy().to_string(),
  })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .setup(|app| {
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
    .invoke_handler(tauri::generate_handler![get_app_info])
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}