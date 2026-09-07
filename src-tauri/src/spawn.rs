use std::{collections::HashMap, io::{Read, Write}, process::{Command, Stdio}};

use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct SpawnOptions {
    cwd: Option<String>,
    env: Option<HashMap<String, String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnConfig {
    cmd: String,
    #[serde(default)]
    args: Vec<String>,
    options: Option<SpawnOptions>,
    data: Option<String>,
    trim: Option<bool>,
    #[serde(default)]
    throw_on_std_err: bool,
}

#[derive(Serialize)]
pub struct SpawnResult {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

#[tauri::command]
pub async fn spawn_process(config: SpawnConfig) -> Result<SpawnResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut command = Command::new(&config.cmd);
        command.args(config.args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(options) = config.options {
            if let Some(cwd) = options.cwd {
                command.current_dir(cwd);
            }
            if let Some(env) = options.env {
                command.env_clear().envs(env);
            }
        }
        let mut child = command.spawn().map_err(|err| err.to_string())?;
        let mut stdin = child.stdin.take().ok_or("Missing child stdin")?;
        let mut stdout = child.stdout.take().ok_or("Missing child stdout")?;
        let mut stderr = child.stderr.take().ok_or("Missing child stderr")?;
        // Drain output while writing stdin: a child can fill a pipe before consuming its input.
        std::thread::scope(|scope| {
            let input = scope.spawn(move || {
                if let Some(data) = config.data {
                    stdin.write_all(data.as_bytes())?;
                }
                drop(stdin);
                Ok::<_, std::io::Error>(())
            });
            let output = scope.spawn(move || {
                let mut bytes = Vec::new();
                stdout.read_to_end(&mut bytes)?;
                Ok::<_, std::io::Error>(bytes)
            });
            let mut errors = Vec::new();
            let mut buffer = [0; 4096];
            let read_error = loop {
                match stderr.read(&mut buffer) {
                    Ok(0) => break None,
                    Ok(count) => {
                        errors.extend_from_slice(&buffer[..count]);
                        if config.throw_on_std_err {
                            let _ = child.kill();
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(err) => {
                        let _ = child.kill();
                        break Some(err);
                    }
                }
            };
            let status = child.wait().map_err(|err| err.to_string())?;
            input.join().map_err(|_| "Child stdin writer panicked")?.map_err(|err| err.to_string())?;
            let bytes = output.join().map_err(|_| "Child stdout reader panicked")?.map_err(|err| err.to_string())?;
            if let Some(err) = read_error {
                return Err(err.to_string());
            }
            let mut stdout = String::from_utf8_lossy(&bytes).into_owned();
            let mut stderr = String::from_utf8_lossy(&errors).into_owned();
            if config.trim != Some(false) {
                stdout = stdout.trim().to_owned();
                stderr = stderr.trim().to_owned();
            }
            Ok(SpawnResult { code: status.code(), stdout, stderr })
        })
    }).await.map_err(|err| err.to_string())?
}
