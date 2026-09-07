use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager, State};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{mpsc, Notify},
    task::JoinHandle,
};

const MAX_INCOMING_DATA: usize = 10_000;
const KWC_CHROME: &str = "chrome-extension://pikpfmjfkekaeinceagbebpfkmkdlcjk/";
const KWC_EDGE: &str = "chrome-extension://nmggpehkjmeaeocmaijenpejbepckinm/";
const KWC_FIREFOX: &str = "keeweb-connect-addon@keeweb.info";
const KWC_SAFARI: &str = "safari-keeweb-connect";
const KPXC_CHROME: &str = "chrome-extension://oboonakemofpalcgghocfoadofidjkkk/";
const KPXC_EDGE: &str = "chrome-extension://pdffhmdngciaglkoonimfcmckehcpafo/";
const KPXC_FIREFOX: &str = "keepassxc-browser@keepassxc.org";

#[derive(Default)]
pub struct ExtConnectorState {
    inner: Arc<Mutex<Connector>>,
    lifecycle: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct Connector {
    listener: Option<JoinHandle<()>>,
    socket_path: Option<PathBuf>,
    generation: u64,
    next_socket_id: u32,
    writers: HashMap<u32, mpsc::Sender<Vec<u8>>>,
    sockets: HashMap<u32, SocketControl>,
}

struct SocketControl {
    abort: JoinHandle<()>,
    notifications: Option<bool>, // None until the handshake completes.
    response_ready: Arc<Notify>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorConfig {
    apple_team_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionInfo {
    connection_id: u32,
    app_name: String,
    extension_name: &'static str,
    pid: Value,
    supports_notifications: bool,
}

#[tauri::command]
pub async fn browser_extension_connector_start(
    app: AppHandle,
    state: State<'_, ExtConnectorState>,
    config: ConnectorConfig,
) -> Result<(), String> {
    let _lifecycle = state.lifecycle.lock().await;
    if state.inner.lock().listener.is_some() {
        return Ok(());
    }
    let path_app = app.clone();
    let path = tauri::async_runtime::spawn_blocking(move || socket_path(&path_app, &config))
        .await.map_err(|err| err.to_string())??;

    #[cfg(unix)]
    let listener = {
        let prepare_path = path.clone();
        let listener = tauri::async_runtime::spawn_blocking(move || prepare_unix_socket(&prepare_path))
            .await.map_err(|err| err.to_string())??;
        tokio::net::UnixListener::from_std(listener).map_err(|err| err.to_string())?
    };
    #[cfg(windows)]
    let listener = tokio::net::windows::named_pipe::ServerOptions::new()
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .create(&path).map_err(|err| err.to_string())?;

    #[cfg(any(unix, windows))]
    {
        let mut inner = state.inner.lock();
        inner.generation += 1;
        let generation = inner.generation;
        inner.socket_path = Some(path.clone());
        let shared = state.inner.clone();
        inner.listener = Some(tokio::spawn(async move {
            listen(app, shared, listener, path, generation).await;
        }));
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    Err("Not supported on this platform".into())
}

#[tauri::command]
pub async fn browser_extension_connector_stop(state: State<'_, ExtConnectorState>) -> Result<(), String> {
    let _lifecycle = state.lifecycle.lock().await;
    let (listener, sockets, path) = {
        let mut inner = state.inner.lock();
        inner.generation += 1;
        let listener = inner.listener.take();
        if let Some(listener) = &listener {
            listener.abort();
        }
        let sockets: Vec<_> = inner.sockets.drain().map(|(_, socket)| {
            socket.abort.abort();
            socket.abort
        }).collect();
        inner.writers.clear();
        (listener, sockets, inner.socket_path.take())
    };
    // Await cancellation before allowing restart, especially for Windows first-pipe-instance ownership.
    if let Some(listener) = listener {
        let _ = listener.await;
    }
    for socket in sockets {
        let _ = socket.await;
    }
    #[cfg(unix)]
    if let Some(path) = path {
        tauri::async_runtime::spawn_blocking(move || remove_socket(&path))
            .await.map_err(|err| err.to_string())??;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[tauri::command]
pub fn browser_extension_connector_socket_result(
    state: State<'_, ExtConnectorState>, socket_id: u32, result: Value,
) -> Result<(), String> {
    let inner = state.inner.lock();
    if let Some(socket) = inner.sockets.get(&socket_id).filter(|socket| socket.notifications.is_some()) {
        if let Some(writer) = inner.writers.get(&socket_id) {
            if let Err(err) = writer.try_send(frame(&result)?) {
                socket.abort.abort();
                return Err(err.to_string());
            }
            socket.response_ready.notify_one();
        }
    }
    Ok(())
}

#[tauri::command]
pub fn browser_extension_connector_socket_event(
    state: State<'_, ExtConnectorState>, data: Value,
) -> Result<(), String> {
    let bytes = frame(&data)?;
    let inner = state.inner.lock();
    let mut error = None;
    for (id, socket) in &inner.sockets {
        if socket.notifications == Some(true) {
            if let Some(writer) = inner.writers.get(id) {
                if let Err(err) = writer.try_send(bytes.clone()) {
                    socket.abort.abort();
                    error = Some(err.to_string());
                }
            }
        }
    }
    error.map_or(Ok(()), Err)
}

#[tauri::command]
pub fn browser_extension_connector_close_socket(
    state: State<'_, ExtConnectorState>, socket_id: u32,
) -> Result<(), String> {
    if let Some(socket) = state.inner.lock().sockets.get(&socket_id) {
        socket.abort.abort();
    }
    Ok(())
}

fn socket_path(app: &AppHandle, config: &ConnectorConfig) -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    {
        if config.apple_team_id.is_empty() || !config.apple_team_id.bytes().all(|c| c.is_ascii_alphanumeric()) {
            return Err("Invalid Apple team ID".into());
        }
        Ok(app.path().home_dir().map_err(|err| err.to_string())?
            .join("Library/Group Containers")
            .join(format!("{}.keeweb/conn.sock", config.apple_team_id)))
    }
    #[cfg(target_os = "linux")]
    {
        let _ = (app, config);
        unsafe extern "C" { fn getuid() -> u32; }
        Ok(std::env::temp_dir().join(format!("keeweb-connect-{}.sock", unsafe { getuid() })))
    }
    #[cfg(windows)]
    {
        let _ = (app, config);
        let username = std::env::var("USERNAME").map_err(|err| err.to_string())?;
        let path = format!(r"\\.\pipe\keeweb-connect-{username}");
        if path.encode_utf16().count() > 256 {
            return Err("Browser extension pipe name is too long".into());
        }
        Ok(path.into())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = (app, config);
        Err("Not supported on this platform".into())
    }
}

#[cfg(unix)]
fn remove_socket(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(path).map_err(|err| err.to_string()),
        Ok(_) => Err(format!("Refusing to remove a non-socket file: {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}

#[cfg(unix)]
fn prepare_unix_socket(path: &Path) -> Result<std::os::unix::net::UnixListener, String> {
    use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};
    if path.as_os_str().as_bytes().len() > 104 {
        return Err("Browser extension socket name is too long".into());
    }
    #[cfg(target_os = "macos")]
    std::fs::create_dir_all(path.parent().ok_or("Missing socket directory")?).map_err(|err| err.to_string())?;
    remove_socket(path)?;
    let listener = std::os::unix::net::UnixListener::bind(path).map_err(|err| err.to_string())?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|err| err.to_string())?;
    listener.set_nonblocking(true).map_err(|err| err.to_string())?;
    Ok(listener)
}

#[cfg(unix)]
async fn listen(app: AppHandle, shared: Arc<Mutex<Connector>>, listener: tokio::net::UnixListener, _path: PathBuf, generation: u64) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => connect_socket(&app, &shared, stream, generation),
            Err(err) => {
                listener_failed(&shared, generation, err);
                break;
            }
        }
    }
}

#[cfg(windows)]
async fn listen(app: AppHandle, shared: Arc<Mutex<Connector>>, mut listener: tokio::net::windows::named_pipe::NamedPipeServer, path: PathBuf, generation: u64) {
    loop {
        if let Err(err) = listener.connect().await {
            listener_failed(&shared, generation, err);
            break;
        }
        match tokio::net::windows::named_pipe::ServerOptions::new().reject_remote_clients(true).create(&path) {
            Ok(next) => {
                let stream = std::mem::replace(&mut listener, next);
                connect_socket(&app, &shared, stream, generation);
            }
            Err(err) => {
                listener_failed(&shared, generation, err);
                break;
            }
        }
    }
}

fn listener_failed(shared: &Mutex<Connector>, generation: u64, error: std::io::Error) {
    eprintln!("Browser extension listener failed: {error}");
    let mut inner = shared.lock();
    if inner.generation == generation {
        inner.listener = None;
    }
}

struct SocketCleanup {
    app: AppHandle,
    shared: Arc<Mutex<Connector>>,
    socket_id: u32,
}

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        {
            let mut inner = self.shared.lock();
            inner.writers.remove(&self.socket_id);
            inner.sockets.remove(&self.socket_id);
        }
        crate::emit_app_event(&self.app, "browser-extension-socket-closed", json!({ "socketId": self.socket_id }));
    }
}

fn connect_socket<S>(app: &AppHandle, shared: &Arc<Mutex<Connector>>, stream: S, generation: u64)
where S: AsyncRead + AsyncWrite + Unpin + Send + 'static {
    let mut inner = shared.lock();
    if inner.generation != generation || inner.listener.is_none() {
        return;
    }
    let Some(socket_id) = inner.next_socket_id.checked_add(1) else { return; };
    inner.next_socket_id = socket_id;
    // Bound pending writes so an unresponsive browser cannot retain unlimited credential responses.
    let (writer, mut outgoing) = mpsc::channel::<Vec<u8>>(16);
    let response_ready = Arc::new(Notify::new());
    let cleanup = SocketCleanup { app: app.clone(), shared: shared.clone(), socket_id };
    let ready = response_ready.clone();
    let task = tokio::spawn(async move {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let result = tokio::select! {
            result = read_requests(&cleanup, &mut reader, ready) => result,
            result = async {
                while let Some(bytes) = outgoing.recv().await {
                    writer.write_all(&bytes).await.map_err(|err| err.to_string())?;
                }
                Ok::<_, String>(())
            } => result,
        };
        if let Err(err) = result {
            eprintln!("Browser extension socket {socket_id}: {err}");
        }
        drop(cleanup);
    });
    inner.writers.insert(socket_id, writer);
    inner.sockets.insert(socket_id, SocketControl {
        abort: task, notifications: None, response_ready,
    });
}

async fn read_requests<R: AsyncRead + Unpin>(socket: &SocketCleanup, reader: &mut R, ready: Arc<Notify>) -> Result<(), String> {
    let mut pending = Vec::new();
    let handshake = read_message(reader, &mut pending).await?;
    let id = socket.socket_id;
    let info = tauri::async_runtime::spawn_blocking(move || connection_info(id, handshake))
        .await.map_err(|err| err.to_string())??;
    {
        let mut inner = socket.shared.lock();
        let control = inner.sockets.get_mut(&id).ok_or("Socket closed during handshake")?;
        control.notifications = Some(info.supports_notifications);
        crate::emit_app_event(&socket.app, "browser-extension-socket-connected", json!({
            "socketId": id, "connectionInfo": info,
        }));
    }
    let mut client_id = None;
    loop {
        let request = read_message(reader, &mut pending).await?;
        validate_request(&request, &mut client_id)?;
        crate::emit_app_event(&socket.app, "browser-extension-socket-request", json!({ "socketId": id, "request": request }));
        // Only a result releases the next request; notifications must not bypass this ordering.
        loop {
            tokio::select! {
                biased;
                _ = ready.notified() => break,
                // Keep detecting peer closure while the webview is displaying a credential prompt.
                result = read_chunk(reader, &mut pending) => result?,
            }
        }
    }
}

async fn read_message<R: AsyncRead + Unpin>(reader: &mut R, pending: &mut Vec<u8>) -> Result<Value, String> {
    loop {
        if pending.len() >= 4 {
            let length = u32::from_ne_bytes(pending[..4].try_into().unwrap()) as usize;
            if length > MAX_INCOMING_DATA {
                return Err("Incoming browser extension frame exceeds 10000 bytes".into());
            }
            if pending.len() >= length + 4 {
                let message = serde_json::from_slice(&pending[4..length + 4]).map_err(|err| err.to_string())?;
                pending.drain(..length + 4);
                return Ok(message);
            }
        }
        read_chunk(reader, pending).await?;
    }
}

async fn read_chunk<R: AsyncRead + Unpin>(reader: &mut R, pending: &mut Vec<u8>) -> Result<(), String> {
    let mut chunk = [0; MAX_INCOMING_DATA + 1];
    let count = reader.read(&mut chunk).await.map_err(|err| err.to_string())?;
    if count == 0 {
        return Err("Connection closed".into());
    }
    if count > MAX_INCOMING_DATA {
        return Err("Incoming browser extension chunk exceeds 10000 bytes".into());
    }
    pending.extend_from_slice(&chunk[..count]);
    Ok(())
}

fn frame(message: &Value) -> Result<Vec<u8>, String> {
    let mut bytes = vec![0; 4];
    serde_json::to_writer(&mut bytes, message).map_err(|err| err.to_string())?;
    let length = u32::try_from(bytes.len() - 4).map_err(|_| "Browser extension response is too large")?;
    bytes[..4].copy_from_slice(&length.to_ne_bytes());
    Ok(bytes)
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        _ => true,
    }
}

fn validate_request(request: &Value, client_id: &mut Option<Value>) -> Result<(), String> {
    if !truthy(request) {
        return Err("Empty browser extension request".into());
    }
    let id = &request["clientID"];
    if truthy(id) {
        if let Some(previous) = client_id {
            let equal = match (&*previous, id) {
                (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
                (Value::Array(_) | Value::Object(_), _) => false,
                _ => previous == id,
            };
            if !equal {
                return Err("Changing client ID is not allowed".into());
            }
        } else {
            *client_id = Some(id.clone());
        }
    } else if request["action"] != "ping" {
        return Err("Empty client ID in browser extension request".into());
    }
    Ok(())
}

fn dev_origins() -> Vec<String> {
    std::env::var("KEEWEB_BROWSER_EXTENSION_IDS_CHROMIUM").ok()
        .map(|ids| ids.split(',').map(|id| format!("chrome-extension://{id}/")).collect())
        .unwrap_or_default()
}

fn connection_info(socket_id: u32, message: Value) -> Result<ConnectionInfo, String> {
    if !truthy(&message["origin"]) || !truthy(&message["pid"]) {
        return Err("Browser extension handshake requires origin and pid".into());
    }
    let origin = message["origin"].as_str().unwrap_or("");
    let is_safari = origin == KWC_SAFARI;
    if !is_safari && !truthy(&message["ppid"]) {
        return Err("Browser extension handshake requires ppid".into());
    }
    let extension_name = match origin {
        KWC_CHROME | KWC_EDGE | KWC_FIREFOX | KWC_SAFARI => "KeeWeb Connect",
        KPXC_CHROME | KPXC_EDGE | KPXC_FIREFOX => "KeePassXC-Browser",
        origin if dev_origins().iter().any(|known| known == origin) => "KeeWeb Connect",
        _ => "unknown",
    };
    let app_name = if is_safari {
        "Safari".into()
    } else {
        let parent = message["ppid"].as_u64().and_then(|pid| u32::try_from(pid).ok())
            .and_then(|pid| process_info(pid).ok());
        #[cfg(windows)]
        let parent = parent.map(|info| {
            if info.app_name == "cmd" { process_info(info.ppid).unwrap_or(info) } else { info }
        });
        let name = parent.map(|info| info.app_name).unwrap_or_else(|| "Unidentified browser".into());
        match name.as_str() {
            "msedge" => "Microsoft Edge".into(),
            "chrome" => "Google Chrome".into(),
            _ => {
                let mut chars = name.chars();
                chars.next().map(|first| first.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or(name)
            }
        }
    };
    Ok(ConnectionInfo {
        connection_id: socket_id, app_name, extension_name,
        pid: message["pid"].clone(), supports_notifications: !is_safari,
    })
}

struct ProcessInfo {
    app_name: String,
    #[cfg(windows)]
    ppid: u32,
}

fn process_info(pid: u32) -> Result<ProcessInfo, String> {
    #[cfg(unix)]
    {
        let output = Command::new("/bin/ps").args(["-opid=,ppid=,comm=", "-p", &pid.to_string()])
            .output().map_err(|err| err.to_string())?;
        let output = String::from_utf8_lossy(&output.stdout);
        let (found_pid, rest) = output.trim().split_once(char::is_whitespace).ok_or("Bad PS output")?;
        let (_, exec_path) = rest.trim_start().split_once(char::is_whitespace).ok_or("Bad PS output")?;
        if found_pid.parse::<u32>().ok() != Some(pid) {
            return Err("PID mismatch in PS output".into());
        }
        let app_name = exec_path.trim().rsplit('/').next().filter(|name| !name.is_empty()).ok_or("Missing process name")?;
        Ok(ProcessInfo { app_name: app_name.into() })
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let output = Command::new("wmic").args([
            "process", "where", &format!("ProcessId={pid}"), "get",
            "ProcessId,ParentProcessId,ExecutablePath", "/format:value",
        ]).creation_flags(0x08000000).output().map_err(|err| err.to_string())?;
        let output = String::from_utf8_lossy(&output.stdout);
        let mut found_pid = None;
        let mut ppid = 0;
        let mut app_name = String::new();
        for line in output.lines() {
            if let Some((key, value)) = line.trim().split_once('=') {
                match key {
                    "ProcessId" => found_pid = value.parse::<u32>().ok(),
                    "ParentProcessId" => ppid = value.parse().unwrap_or(0),
                    "ExecutablePath" => {
                        app_name = value.trim_matches('"').rsplit('\\').next().unwrap_or("").to_owned();
                        if app_name.to_ascii_lowercase().ends_with(".exe") {
                            app_name.truncate(app_name.len() - 4);
                        }
                    }
                    _ => {}
                }
            }
        }
        if found_pid != Some(pid) || app_name.is_empty() {
            return Err("Cannot identify browser process".into());
        }
        Ok(ProcessInfo { app_name, ppid })
    }
    #[cfg(not(any(unix, windows)))]
    { let _ = pid; Err("Not supported on this platform".into()) }
}

#[tauri::command]
pub async fn browser_extension_connector_enable(
    app: AppHandle, browser: String, extension: String, enabled: bool,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || install_manifest(&app, &browser, &extension, enabled))
        .await.map_err(|err| err.to_string())?
}

fn host_name(extension: &str) -> Result<&'static str, String> {
    match extension {
        "KWC" => Ok("net.antelle.keeweb.keeweb_connect"),
        "KPXC" => Ok("org.keepassxc.keepassxc_browser"),
        _ => Err("Unknown browser extension".into()),
    }
}

fn native_host_path() -> Result<PathBuf, String> {
    let executable = if cfg!(windows) { "keeweb-native-messaging-host.exe" } else { "keeweb-native-messaging-host" };
    if cfg!(debug_assertions) {
        let platform = match std::env::consts::OS {
            "macos" => "darwin", "windows" => "win32", "linux" => "linux",
            _ => return Err("Not supported on this platform".into()),
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => "x64", "aarch64" => "arm64", "x86" => "ia32",
            _ => return Err("Unsupported native messaging host architecture".into()),
        };
        Ok(Path::new(env!("CARGO_MANIFEST_DIR")).join("../node_modules/@keeweb/keeweb-native-messaging-host")
            .join(format!("{platform}-{arch}")).join(executable))
    } else {
        Ok(std::env::current_exe().map_err(|err| err.to_string())?
            .parent().ok_or("Missing executable directory")?.join(executable))
    }
}

fn create_manifest(browser: &str, extension: &str, host: &Path) -> Result<Value, String> {
    let name = host_name(extension)?;
    let kwc = extension == "KWC";
    let mut manifest = json!({
        "description": if kwc { "KeeWeb native messaging host" } else { "Native messaging host created by KeeWeb" },
        "name": name, "type": "stdio", "path": host,
    });
    if browser == "Firefox" {
        manifest["allowed_extensions"] = json!([if kwc { KWC_FIREFOX } else { KPXC_FIREFOX }]);
    } else {
        let mut origins = if kwc { vec![KWC_CHROME.to_owned(), KWC_EDGE.to_owned()] }
            else { vec![KPXC_CHROME.to_owned(), KPXC_EDGE.to_owned()] };
        if kwc { origins.extend(dev_origins()); }
        manifest["allowed_origins"] = json!(origins);
    }
    Ok(manifest)
}

#[cfg(not(windows))]
fn manifest_dir(home: &Path, browser: &str) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let directory = match browser {
        "Chrome" => "Library/Application Support/Google/Chrome/NativeMessagingHosts",
        "Firefox" => "Library/Application Support/Mozilla/NativeMessagingHosts",
        "Edge" => "Library/Application Support/Microsoft Edge/NativeMessagingHosts",
        _ => return None,
    };
    #[cfg(target_os = "linux")]
    let directory = match browser {
        "Chrome" => ".config/google-chrome/NativeMessagingHosts",
        "Firefox" => ".mozilla/native-messaging-hosts",
        "Edge" => ".config/microsoft-edge/NativeMessagingHosts",
        _ => return None,
    };
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return Some(home.join(directory));
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    { let _ = (home, browser); None }
}

fn install_manifest(app: &AppHandle, browser: &str, extension: &str, enabled: bool) -> Result<(), String> {
    if !cfg!(any(target_os = "macos", target_os = "linux", windows)) {
        return Err("Not supported on this platform".into());
    }
    // Safari connects directly; Other is for browsers whose manifests users manage themselves.
    if matches!(browser, "Safari" | "Other") { return Ok(()); }
    if !matches!(browser, "Chrome" | "Firefox" | "Edge") { return Err("Unknown browser".into()); }
    let name = host_name(extension)?;
    #[cfg(windows)]
    let registry_key = format!(r"HKCU\Software\{}\NativeMessagingHosts\{name}", match browser {
        "Chrome" => r"Google\Chrome", "Firefox" => "Mozilla", _ => r"Microsoft\Edge",
    });
    #[cfg(windows)]
    let path = app.state::<crate::Shell>().user_data_dir.join(format!("native-messaging-{}.{}.json",
        extension.to_lowercase(), if browser == "Firefox" { "firefox" } else { "chrome" }));
    #[cfg(not(windows))]
    let path = manifest_dir(&app.path().home_dir().map_err(|err| err.to_string())?, browser)
        .ok_or("Not supported on this platform")?.join(format!("{name}.json"));
    if enabled {
        let host = native_host_path()?;
        if !host.is_file() { return Err(format!("Native messaging host not found: {}", host.display())); }
        let manifest = create_manifest(browser, extension, &host)?;
        std::fs::create_dir_all(path.parent().ok_or("Missing manifest directory")?).map_err(|err| err.to_string())?;
        let mut json = Vec::new();
        let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
        manifest.serialize(&mut serde_json::Serializer::with_formatter(&mut json, formatter)).map_err(|err| err.to_string())?;
        std::fs::write(&path, json).map_err(|err| err.to_string())?;
        #[cfg(windows)]
        registry(&["ADD", &registry_key, "/ve", "/d", &path.to_string_lossy(), "/f"])?;
    } else {
        #[cfg(windows)]
        registry(&["DELETE", &registry_key, "/f"])?;
        #[cfg(not(windows))]
        match std::fs::remove_file(path) {
            Ok(()) => {},
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {},
            Err(err) => return Err(err.to_string()),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn registry(args: &[&str]) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let output = Command::new("REG").args(args).creation_flags(0x08000000)
        .output().map_err(|err| err.to_string())?;
    if output.status.success() { Ok(()) }
    else { Err(format!("REG failed: {}", String::from_utf8_lossy(&output.stderr).trim())) }
}
