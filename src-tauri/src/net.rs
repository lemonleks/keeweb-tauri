use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    sync::{atomic::{AtomicBool, Ordering}, Arc, LazyLock, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::json;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const OAUTH_HTML: &str = "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"UTF-8\"><title>KeeWeb</title></head><body><h1>Authentication is complete, you may close this tab now.</h1></body></html>";
static HTTP: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .http_status_as_error(false)
        // WebDAV uses MOVE and other extension methods.
        .allow_non_standard_methods(true)
        .user_agent(concat!("KeeWeb/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent()
});
static OAUTH_LISTENER: Mutex<Option<OAuthListener>> = Mutex::new(None);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRequestConfig {
    url: String,
    method: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    data: Option<Vec<u8>>,
    timeout_ms: Option<u64>,
}

#[derive(Serialize)]
pub struct HttpResponse {
    status: u16,
    headers: HashMap<String, String>,
    data: Vec<u8>,
}


fn http_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::Timeout(_) => "timeout".to_owned(),
        error => error.to_string(),
    }
}

#[tauri::command]
pub async fn http_request(config: HttpRequestConfig) -> Result<HttpResponse, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut request = ureq::http::Request::builder()
            .method(config.method.as_str())
            .uri(config.url.as_str());
        for (name, value) in config.headers {
            request = request.header(name, value);
        }
        let request = request.body(config.data.unwrap_or_default()).map_err(|err| err.to_string())?;
        let request = HTTP
            .configure_request(request)
            .timeout_global(Some(config.timeout_ms.map(Duration::from_millis).unwrap_or(REQUEST_TIMEOUT)))
            .build();
        let mut response = HTTP.run(request).map_err(http_error)?;
        let status = response.status().as_u16();
        let mut headers = HashMap::<String, String>::new();
        for (name, value) in response.headers() {
            let value = String::from_utf8_lossy(value.as_bytes());
            headers.entry(name.as_str().to_owned())
                .and_modify(|previous| { previous.push_str(", "); previous.push_str(&value); })
                .or_insert_with(|| value.into_owned());
        }
        // Databases can exceed ureq's 10 MB convenience-reader limit.
        let data = response.body_mut().with_config().limit(u64::MAX).read_to_vec().map_err(http_error)?;
        Ok(HttpResponse { status, headers, data })
    }).await.map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn download_to_file(url: String, path: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut response = HTTP.get(&url)
            .config().max_redirects(1).build().call().map_err(http_error)?;
        if response.status() != 200 {
            return Err(format!("HTTP status {}", response.status().as_u16()));
        }
        let path = PathBuf::from(path);
        let name = path.file_name().ok_or("Download path has no file name")?;
        let temporary = path.with_file_name(format!(".{}.download-{:016x}", name.to_string_lossy(), rand::random::<u64>()));
        let mut file = OpenOptions::new().write(true).create_new(true).open(&temporary).map_err(|err| err.to_string())?;
        let result = (|| {
            io::copy(&mut response.body_mut().as_reader(), &mut file)?;
            file.sync_all()?;
            drop(file);
            // Only a completed download may become a reusable cache entry.
            fs::rename(&temporary, &path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(|err| err.to_string())
    }).await.map_err(|err| err.to_string())?
}

struct OAuthListener {
    address: SocketAddr,
    stopped: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

fn stop_listener(listener: &mut Option<OAuthListener>) -> Result<(), String> {
    if let Some(listener) = listener.take() {
        listener.stopped.store(true, Ordering::Release);
        // Wake blocking accept; reads also check the flag every 100 ms.
        let _ = TcpStream::connect_timeout(&listener.address, Duration::from_millis(100));
        listener.thread.join().map_err(|_| "OAuth listener thread panicked")?;
    }
    Ok(())
}

#[tauri::command]
pub async fn oauth_listener_start(app: tauri::AppHandle, port: u16, path: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        if port == 0 || !path.starts_with('/') || path.starts_with("//") || path.contains(['?', '#', '\r', '\n']) {
            return Err("Invalid OAuth listener port or path".to_owned());
        }
        let mut current = OAUTH_LISTENER.lock().map_err(|err| err.to_string())?;
        stop_listener(&mut current)?;
        let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).map_err(|err| err.to_string())?;
        let address = socket.local_addr().map_err(|err| err.to_string())?;
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_stopped = Arc::clone(&stopped);
        let thread = thread::Builder::new().name("oauth-listener".to_owned())
            .spawn(move || run_oauth_listener(socket, &path, &thread_stopped, &app))
            .map_err(|err| err.to_string())?;
        *current = Some(OAuthListener { address, stopped, thread });
        Ok(())
    }).await.map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn oauth_listener_stop() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(|| {
        let mut current = OAUTH_LISTENER.lock().map_err(|err| err.to_string())?;
        stop_listener(&mut current)
    }).await.map_err(|err| err.to_string())?
}

fn oauth_request(stream: &mut TcpStream, stopped: &AtomicBool) -> io::Result<Option<String>> {
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut request = Vec::new();
    let mut chunk = [0; 1024];
    while !stopped.load(Ordering::Acquire) && Instant::now() < deadline && request.len() < 16 * 1024 {
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(None),
            Ok(count) => {
                request.extend_from_slice(&chunk[..count]);
                if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    return String::from_utf8(request).map(Some).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err));
                }
            }
            Err(err) if matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(None)
}

fn oauth_response(stream: &mut TcpStream, status: &str, body: &str) -> io::Result<()> {
    write!(stream, "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=UTF-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; frame-ancestors 'none'\r\n\r\n{body}", body.len())?;
    stream.flush()
}

fn run_oauth_listener(socket: TcpListener, path: &str, stopped: &AtomicBool, app: &tauri::AppHandle) {
    let port = match socket.local_addr() {
        Ok(address) => address.port(),
        Err(_) => return,
    };
    let mut result = None;
    for stream in socket.incoming() {
        if stopped.load(Ordering::Acquire) {
            break;
        }
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                crate::emit_app_event(app, "oauth-listener-result", json!({ "url": format!("http://127.0.0.1:{port}{path}"), "state": null, "code": null, "error": err.to_string() }));
                break;
            }
        };
        let request = match oauth_request(&mut stream, stopped) {
            Ok(Some(request)) if !stopped.load(Ordering::Acquire) => request,
            _ => continue,
        };
        let mut lines = request.split("\r\n");
        let mut request_line = lines.next().unwrap_or_default().split_whitespace();
        let method = request_line.next().unwrap_or_default();
        let target = request_line.next().unwrap_or_default();
        let version = request_line.next().unwrap_or_default();
        if !matches!(version, "HTTP/1.0" | "HTTP/1.1") || request_line.next().is_some() || !target.starts_with('/') || target.starts_with("//") {
            let _ = oauth_response(&mut stream, "400 Bad Request", "Invalid request");
            continue;
        }
        let host = lines.filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("host"))
            .map(|(_, value)| value.trim()).unwrap_or_default();
        if host != format!("localhost:{port}") && host != format!("127.0.0.1:{port}") {
            let _ = oauth_response(&mut stream, "400 Bad Request", "Invalid host");
            continue;
        }
        let url = match tauri::Url::parse(&format!("http://{host}{target}")) {
            Ok(url) => url,
            Err(_) => {
                let _ = oauth_response(&mut stream, "400 Bad Request", "Invalid URL");
                continue;
            }
        };
        if method != "GET" || !url.path().starts_with(path) {
            let _ = oauth_response(&mut stream, "404 Not Found", "Not found");
            continue;
        }
        let mut state = None;
        let mut code = None;
        let mut error = None;
        for (name, value) in url.query_pairs() {
            let field = match name.as_ref() {
                "state" => &mut state,
                "code" => &mut code,
                "error" => &mut error,
                _ => continue,
            };
            if field.is_none() {
                *field = Some(value.into_owned());
            }
        }
        let _ = oauth_response(&mut stream, "200 OK", OAUTH_HTML);
        result = Some(json!({ "url": url.as_str(), "state": state, "code": code, "error": error }));
        break;
    }
    drop(socket);
    if let Some(result) = result.filter(|_| !stopped.load(Ordering::Acquire)) {
        crate::emit_app_event(app, "oauth-listener-result", result);
    }
}
