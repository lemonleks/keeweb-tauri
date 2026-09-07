use std::{io, path::PathBuf, sync::atomic::{AtomicU32, Ordering}, time::UNIX_EPOCH};

use notify::{EventKind, RecursiveMode, Watcher};
use serde::Serialize;
use serde_json::json;
use tauri::{ipc::{InvokeBody, Request, Response}, AppHandle, State};

use crate::{emit_app_event, Shell};

static NEXT_WATCHER_ID: AtomicU32 = AtomicU32::new(1);

fn io_error(err: io::Error) -> String {
    let code = match err.kind() {
        io::ErrorKind::NotFound => "ENOENT",
        io::ErrorKind::PermissionDenied => "EACCES",
        io::ErrorKind::AlreadyExists => "EEXIST",
        io::ErrorKind::NotADirectory => "ENOTDIR",
        io::ErrorKind::IsADirectory => "EISDIR",
        io::ErrorKind::DirectoryNotEmpty => "ENOTEMPTY",
        io::ErrorKind::StorageFull => "ENOSPC",
        io::ErrorKind::ReadOnlyFilesystem => "EROFS",
        io::ErrorKind::InvalidInput => "EINVAL",
        _ => "EIO",
    };
    format!("{code}: {err}")
}

#[tauri::command]
pub async fn fs_read(path: String) -> Result<Response, String> {
    tauri::async_runtime::spawn_blocking(move || std::fs::read(path).map(Response::new).map_err(io_error))
        .await.map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn fs_read_text(path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || std::fs::read_to_string(path).map_err(io_error))
        .await.map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn fs_write(request: Request<'_>) -> Result<(), String> {
    let encoded_path = request.headers().get("path").ok_or("EINVAL: Missing path header")?
        .to_str().map_err(|err| format!("EINVAL: {err}"))?;
    let path = percent_encoding::percent_decode_str(encoded_path).decode_utf8()
        .map_err(|err| format!("EINVAL: {err}"))?.into_owned();
    let data = match request.body() {
        InvokeBody::Raw(data) => data.clone(),
        _ => return Err("EINVAL: Expected a raw byte body".into()),
    };
    tauri::async_runtime::spawn_blocking(move || std::fs::write(path, data).map_err(io_error))
        .await.map_err(|err| err.to_string())?
}

#[tauri::command]
pub fn fs_exists(path: String) -> Result<bool, String> {
    std::path::Path::new(&path).try_exists().map_err(io_error)
}

#[tauri::command]
pub fn fs_delete(path: String) -> Result<(), String> {
    std::fs::remove_file(path).map_err(io_error)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileStat {
    size: u64,
    mtime: i64,
    is_dir: bool,
    uid: Option<u32>,
}

#[tauri::command]
pub fn fs_stat(path: String) -> Result<FileStat, String> {
    let metadata = std::fs::metadata(path).map_err(io_error)?;
    let modified = metadata.modified().map_err(io_error)?;
    let mtime = match modified.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i64,
        Err(err) => -(err.duration().as_millis() as i64),
    };
    #[cfg(unix)]
    let uid = {
        use std::os::unix::fs::MetadataExt;
        Some(metadata.uid())
    };
    #[cfg(not(unix))]
    let uid = None;
    Ok(FileStat { size: metadata.len(), mtime, is_dir: metadata.is_dir(), uid })
}

#[tauri::command]
pub fn fs_mkdir(path: String) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(io_error)
}

#[tauri::command]
pub fn fs_read_dir(path: String) -> Result<Vec<String>, String> {
    std::fs::read_dir(path).map_err(io_error)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()).map_err(io_error))
        .collect()
}

#[tauri::command]
pub fn fs_watch_start(app: AppHandle, shell: State<'_, Shell>, path: String) -> Result<u32, String> {
    let supplied = PathBuf::from(&path);
    let absolute = if supplied.is_absolute() { supplied } else { std::env::current_dir().map_err(io_error)?.join(supplied) };
    let directory_watch = absolute.is_dir();
    let parent = if directory_watch {
        absolute.canonicalize().map_err(io_error)?
    } else {
        absolute.parent().ok_or("EINVAL: File has no parent directory")?.canonicalize().map_err(io_error)?
    };
    let target = if directory_watch {
        None
    } else {
        Some(parent.join(absolute.file_name().ok_or("EINVAL: Missing file name")?))
    };
    let id = NEXT_WATCHER_ID.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .map_err(|_| "Too many file watchers")?;
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        match result {
            Ok(event) => {
                let kind = match event.kind {
                    EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(notify::event::ModifyKind::Name(_)) => "rename",
                    EventKind::Modify(_) | EventKind::Any | EventKind::Other => "change",
                    EventKind::Access(_) => return,
                };
                for changed_path in event.paths {
                    if target.as_ref().is_none_or(|target| target == &changed_path) {
                        emit_app_event(&app, "fs-watch", json!({ "id": id, "path": changed_path, "kind": kind }));
                    }
                }
            }
            Err(err) => emit_app_event(&app, "log", json!(format!("File watcher {id}: {err}"))),
        }
    }).map_err(|err| err.to_string())?;
    // Watching the parent survives atomic replacements of a .kdbx file.
    watcher.watch(&parent, RecursiveMode::NonRecursive).map_err(|err| err.to_string())?;
    shell.watchers.lock().map_err(|err| err.to_string())?.insert(id, watcher);
    Ok(id)
}

#[tauri::command]
pub fn fs_watch_stop(shell: State<'_, Shell>, id: u32) -> Result<(), String> {
    shell.watchers.lock().map_err(|err| err.to_string())?.remove(&id);
    Ok(())
}
