use std::{io::ErrorKind, path::Path};

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use rand::RngCore;
use tauri::State;

use crate::Shell;

pub struct ConfigStore {
    key: Option<[u8; 48]>,
}

impl ConfigStore {
    pub fn new(directory: &Path, portable: bool) -> Result<Self, String> {
        if portable {
            return Ok(Self { key: None });
        }
        // Keytar stores a UTF-8 blob at service/account on Windows, not keyring's
        // default username.service target or UTF-16 password representation.
        #[cfg(target_os = "windows")]
        let entry = keyring::Entry::new_with_target("KeeWeb/settings-key", "KeeWeb", "settings-key").map_err(|err| err.to_string())?;
        #[cfg(not(target_os = "windows"))]
        let entry = keyring::Entry::new("KeeWeb", "settings-key").map_err(|err| err.to_string())?;
        let stored_key = entry.get_secret();
        #[cfg(target_os = "linux")]
        let stored_key = if matches!(&stored_key, Err(keyring::Error::NoEntry)) {
            match legacy_linux_key()? {
                Some(key) => {
                    entry.set_secret(&key).map_err(|err| err.to_string())?;
                    Ok(key)
                }
                None => stored_key,
            }
        } else { stored_key };
        let (key, created) = match stored_key {
            Ok(password) => {
                let decoded = hex::decode(password).map_err(|err| format!("Invalid settings key: {err}"))?;
                let key: [u8; 48] = decoded.try_into().map_err(|_| "Settings key must contain 48 bytes")?;
                (key, false)
            }
            Err(keyring::Error::NoEntry) => {
                let mut key = [0; 48];
                rand::rngs::OsRng.fill_bytes(&mut key);
                entry.set_secret(hex::encode(key).as_bytes()).map_err(|err| err.to_string())?;
                (key, true)
            }
            Err(err) => return Err(format!("Cannot read settings key: {err}")),
        };
        let store = Self { key: Some(key) };
        if created {
            for name in ["file-info", "app-settings", "runtime-data", "update-info", "plugin-gallery", "plugins"] {
                let path = directory.join(format!("{name}.json"));
                match std::fs::read(&path) {
                    Ok(data) => {
                        store.save(directory, name, &String::from_utf8_lossy(&data))?;
                        std::fs::remove_file(path).map_err(|err| err.to_string())?;
                    }
                    Err(err) if err.kind() == ErrorKind::NotFound => {}
                    Err(err) => return Err(err.to_string()),
                }
            }
        }
        Ok(store)
    }

    fn path(&self, directory: &Path, name: &str) -> Result<std::path::PathBuf, String> {
        if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_') {
            return Err("Invalid config name".into());
        }
        let extension = if self.key.is_some() { "dat" } else { "json" };
        Ok(directory.join(format!("{name}.{extension}")))
    }

    pub fn load(&self, directory: &Path, name: &str) -> Result<Option<String>, String> {
        let data = match std::fs::read(self.path(directory, name)?) {
            Ok(data) => data,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(format!("Error reading config {name}: {err}")),
        };
        let data = if let Some(key) = &self.key {
            match cbc::Decryptor::<aes::Aes256>::new((&key[..32]).into(), (&key[32..]).into())
                .decrypt_padded_vec_mut::<Pkcs7>(&data)
            {
                Ok(data) => data,
                Err(err) => {
                    // Electron ignores damaged config ciphertext instead of preventing startup.
                    eprintln!("Error reading config data (config ignored) {name}: {err}");
                    return Ok(None);
                }
            }
        } else {
            data
        };
        Ok(Some(String::from_utf8_lossy(&data).into_owned()))
    }

    pub fn save(&self, directory: &Path, name: &str, data: &str) -> Result<(), String> {
        let path = self.path(directory, name)?;
        if let Some(key) = &self.key {
            let encrypted = cbc::Encryptor::<aes::Aes256>::new((&key[..32]).into(), (&key[32..]).into())
                .encrypt_padded_vec_mut::<Pkcs7>(data.as_bytes());
            std::fs::write(path, encrypted)
        } else {
            std::fs::write(path, data)
        }
        .map_err(|err| format!("Error writing config {name}: {err}"))
    }
}

#[cfg(target_os = "linux")]
fn legacy_linux_key() -> Result<Option<Vec<u8>>, String> {
    use dbus_secret_service::{EncryptionType, SecretService};
    // Keytar identifies Linux credentials with "account"; keyring uses "username".
    let service = SecretService::connect(EncryptionType::Dh).map_err(|err| err.to_string())?;
    let matches = service.search_items(std::collections::HashMap::from([
        ("service", "KeeWeb"), ("account", "settings-key"),
    ])).map_err(|err| err.to_string())?;
    if matches.locked.len() + matches.unlocked.len() > 1 {
        return Err("Multiple legacy settings keys found in Secret Service".into());
    }
    for item in matches.locked {
        item.unlock().map_err(|err| err.to_string())?;
        return item.get_secret().map(Some).map_err(|err| err.to_string());
    }
    match matches.unlocked.into_iter().next() {
        Some(item) => item.get_secret().map(Some).map_err(|err| err.to_string()),
        None => Ok(None),
    }
}

#[tauri::command]
pub fn load_config(shell: State<'_, Shell>, config: State<'_, ConfigStore>, name: String) -> Result<Option<String>, String> {
    config.load(&shell.user_data_dir, &name)
}

#[tauri::command]
pub fn save_config(shell: State<'_, Shell>, config: State<'_, ConfigStore>, name: String, data: String) -> Result<(), String> {
    config.save(&shell.user_data_dir, &name, &data)
}
