mod db;

use tauri::Manager;

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