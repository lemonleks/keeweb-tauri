use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager, State};

use crate::Shell;

pub fn user_data_dir(app: &AppHandle) -> Result<(PathBuf, bool), String> {
    // Dev isolation: never touch the real KeeWeb profile or keychain when this is set.
    if let Some(dir) = std::env::var_os("KEEWEB_USER_DATA_DIR") {
        let directory = PathBuf::from(dir);
        std::fs::create_dir_all(&directory).map_err(|err| err.to_string())?;
        return Ok((directory.canonicalize().map_err(|err| err.to_string())?, true));
    }
    let home = app.path().home_dir().map_err(|err| err.to_string())?;
    #[cfg(target_os = "macos")]
    let default = home.join("Library/Application Support/KeeWeb");
    #[cfg(target_os = "windows")]
    let default = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or("APPDATA is not set")?
        .join("KeeWeb");
    #[cfg(target_os = "linux")]
    let default = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
        .join("KeeWeb");
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    let default = app.path().app_config_dir().map_err(|err| err.to_string())?;

    let executable = std::env::current_exe().map_err(|err| err.to_string())?;
    let executable_text = executable.to_string_lossy();
    let mut eligible = if cfg!(target_os = "macos") {
        !executable_text.contains("/Applications/")
    } else if cfg!(target_os = "windows") {
        !executable_text.contains("Program Files")
    } else {
        !executable.starts_with("/usr/") && !executable.starts_with("/opt/")
    };
    if cfg!(debug_assertions) {
        if let Ok(value) = std::env::var("KEEWEB_IS_PORTABLE") {
            if let Ok(value) = serde_json::from_str::<bool>(&value) {
                eligible = value;
            }
        }
    }
    let mut directory = default;
    let mut portable = false;
    if eligible {
        // On macOS the config lives beside KeeWeb.app, not inside Contents/MacOS.
        let bundle = executable.ancestors().find(|path| path.extension().is_some_and(|ext| ext == "app"));
        let parent = bundle.unwrap_or(&executable).parent().ok_or("Executable has no parent")?;
        let config_path = parent.join("keeweb-portable.json");
        match std::fs::read_to_string(&config_path) {
            Ok(text) => {
                let config: serde_json::Value = serde_json::from_str(&text).map_err(|err| format!("{}: {err}", config_path.display()))?;
                let value = config.get("userDataDir").and_then(|value| value.as_str()).ok_or("Portable config requires userDataDir")?;
                directory = parent.join(value);
                portable = true;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.to_string()),
        }
    }
    std::fs::create_dir_all(&directory).map_err(|err| err.to_string())?;
    Ok((directory.canonicalize().map_err(|err| err.to_string())?, portable))
}

#[tauri::command]
pub fn get_path(app: AppHandle, shell: State<'_, Shell>, kind: String) -> Result<String, String> {
    let path = match kind.as_str() {
        "userData" => shell.user_data_dir.clone(),
        "temp" => {
            let directory = std::env::temp_dir().join("KeeWeb");
            std::fs::create_dir_all(&directory).map_err(|err| err.to_string())?;
            directory
        }
        "documents" => app.path().document_dir().map_err(|err| err.to_string())?,
        "app" => app.path().resource_dir().map_err(|err| err.to_string())?,
        "workDir" => std::env::current_dir().map_err(|err| err.to_string())?,
        _ => return Err(format!("Unknown path kind: {kind}")),
    };
    path.to_str().map(str::to_owned).ok_or_else(|| format!("Path is not UTF-8: {}", Path::new(&path).display()))
}
