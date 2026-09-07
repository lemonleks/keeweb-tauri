use tauri::ipc::Response;

#[tauri::command]
pub async fn hardware_crypto_delete_key() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(platform::delete_key)
        .await
        .map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn hardware_encrypt(data: Vec<u8>) -> Result<Response, String> {
    tauri::async_runtime::spawn_blocking(move || platform::encrypt(data).map(Response::new))
        .await
        .map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn hardware_decrypt(data: Vec<u8>, touch_id_prompt: String) -> Result<Response, String> {
    tauri::async_runtime::spawn_blocking(move || {
        platform::decrypt(data, &touch_id_prompt).map(Response::new)
    })
    .await
    .map_err(|err| err.to_string())?
}

#[cfg(not(target_os = "macos"))]
mod platform {
    const UNSUPPORTED: &str = "Hardware encryption is only supported on macOS";

    pub fn delete_key() -> Result<(), String> {
        Err(UNSUPPORTED.into())
    }

    pub fn encrypt(mut data: Vec<u8>) -> Result<Vec<u8>, String> {
        zeroize::Zeroize::zeroize(&mut data);
        Err(UNSUPPORTED.into())
    }

    pub fn decrypt(_data: Vec<u8>, _prompt: &str) -> Result<Vec<u8>, String> {
        Err(UNSUPPORTED.into())
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::{ptr, sync::Mutex};

    use aes_gcm::{aead::AeadInPlace, Aes256Gcm, KeyInit, Nonce};
    use core_foundation::{
        base::{kCFAllocatorDefault, TCFType, ToVoid},
        boolean::CFBoolean,
        data::CFData,
        dictionary::{CFDictionaryCreateMutableCopy, CFMutableDictionary},
        string::{CFString, CFStringRef},
    };
    use objc2::rc::autoreleasepool;
    use objc2_foundation::NSString;
    use objc2_local_authentication::{LAContext, LAError, LAPolicy};
    use rand::RngCore;
    use security_framework::{
        access_control::{ProtectionMode, SecAccessControl},
        base::Error,
        item::Location,
        key::{Algorithm, GenerateKeyOptions, KeyType, SecKey, Token},
    };
    use security_framework_sys::{
        access_control::{
            kSecAccessControlBiometryCurrentSet, kSecAccessControlOr,
            kSecAccessControlPrivateKeyUsage, kSecAccessControlUserPresence,
            kSecAccessControlWatch,
        },
        base::errSecItemNotFound,
        item::{
            kSecAttrAccessControl, kSecAttrIsPermanent, kSecAttrKeyClass,
            kSecAttrKeyClassPrivate, kSecAttrKeyType, kSecAttrKeyTypeECSECPrimeRandom,
            kSecAttrTokenID, kSecAttrTokenIDSecureEnclave, kSecClass, kSecClassKey,
            kSecPrivateKeyAttrs, kSecPublicKeyAttrs, kSecReturnRef,
            kSecUseAuthenticationContext, kSecUseDataProtectionKeychain,
        },
        keychain_item::SecItemCopyMatching,
    };
    use zeroize::Zeroizing;

    const KEY_TAG: &str = "net.antelle.keeweb.encryption-key";
    // Preserve secure-enclave 0.4.1's wire format for remembered Electron passwords.
    const ALGORITHM: Algorithm = Algorithm::ECIESEncryptionCofactorVariableIVX963SHA256AESGCM;
    const NONCE_LEN: usize = 12;
    const TAG_LEN: usize = 16;

    // Serialize creation/deletion with use of the one application key, including auth dialogs.
    static MEMORY_KEY: Mutex<Option<Zeroizing<[u8; 32]>>> = Mutex::new(None);

    // security-framework-sys does not expose these two public Security.framework constants.
    #[link(name = "Security", kind = "framework")]
    extern "C" {
        static kSecAttrApplicationTag: CFStringRef;
        static kSecUseOperationPrompt: CFStringRef;
    }

    fn emulation() -> Result<Option<bool>, String> {
        match std::env::var("KEEWEB_EMULATE_HARDWARE_ENCRYPTION") {
            Ok(mode) if mode == "persistent" => Ok(Some(true)),
            Ok(mode) if mode == "memory" => Ok(Some(false)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            _ => Err("KEEWEB_EMULATE_HARDWARE_ENCRYPTION must be memory or persistent".into()),
        }
    }

    fn random_key() -> Result<Zeroizing<[u8; 32]>, String> {
        let mut key = Zeroizing::new([0; 32]);
        rand::rngs::OsRng.try_fill_bytes(&mut *key).map_err(|err| err.to_string())?;
        Ok(key)
    }

    fn emulated_cipher(
        memory: &mut Option<Zeroizing<[u8; 32]>>,
        persistent: bool,
        create: bool,
    ) -> Result<Aes256Gcm, String> {
        if persistent {
            let entry = keyring::Entry::new("KeeWeb", "emulated-hardware-key")
                .map_err(|err| err.to_string())?;
            match entry.get_secret() {
                Ok(key) => Aes256Gcm::new_from_slice(&Zeroizing::new(key))
                    .map_err(|_| "Invalid emulated hardware key: expected 32 bytes".into()),
                Err(keyring::Error::NoEntry) if create => {
                    let key = random_key()?;
                    entry.set_secret(&*key).map_err(|err| err.to_string())?;
                    Ok(Aes256Gcm::new((&*key).into()))
                }
                Err(err) => Err(format!("Cannot read emulated hardware key: {err}")),
            }
        } else {
            if memory.is_none() && create {
                *memory = Some(random_key()?);
            }
            let key = memory.as_ref().ok_or("Emulated hardware key not found")?;
            Ok(Aes256Gcm::new((&**key).into()))
        }
    }

    pub fn encrypt(data: Vec<u8>) -> Result<Vec<u8>, String> {
        let mut data = Zeroizing::new(data);
        let mut memory = MEMORY_KEY.lock().map_err(|err| err.to_string())?;
        if let Some(persistent) = emulation()? {
            let cipher = emulated_cipher(&mut memory, persistent, true)?;
            let mut nonce = [0; NONCE_LEN];
            rand::rngs::OsRng.try_fill_bytes(&mut nonce).map_err(|err| err.to_string())?;
            // Emulation envelope: ciphertext || GCM tag (16 bytes) || nonce (12 bytes).
            // Append the nonce instead of copying/shifting the encrypted password.
            data.reserve(TAG_LEN + NONCE_LEN);
            cipher.encrypt_in_place(Nonce::from_slice(&nonce), b"", &mut *data)
                .map_err(|_| "Hardware emulation encryption failed")?;
            data.extend_from_slice(&nonce);
            return Ok(std::mem::take(&mut *data));
        }
        autoreleasepool(|_| {
            let private_key = match find_key(None)? {
                Some(key) => key,
                None => create_key()?,
            };
            let public_key = private_key.public_key().ok_or("Cannot extract Secure Enclave public key")?;
            public_key.encrypt_data(ALGORITHM, &data)
                .map_err(|err| security_error("SecKeyCreateEncryptedData", err.code(), &err.to_string()))
        })
    }

    pub fn decrypt(data: Vec<u8>, prompt: &str) -> Result<Vec<u8>, String> {
        let mut data = Zeroizing::new(data);
        let mut memory = MEMORY_KEY.lock().map_err(|err| err.to_string())?;
        if let Some(persistent) = emulation()? {
            if data.len() < NONCE_LEN + TAG_LEN {
                return Err("Invalid hardware emulation ciphertext".into());
            }
            let cipher = emulated_cipher(&mut memory, persistent, false)?;
            let mut nonce = [0; NONCE_LEN];
            nonce.copy_from_slice(&data[data.len() - NONCE_LEN..]);
            let encrypted_len = data.len() - NONCE_LEN;
            data.truncate(encrypted_len);
            cipher.decrypt_in_place(Nonce::from_slice(&nonce), b"", &mut *data)
                .map_err(|_| "Hardware emulation authentication failed")?;
            return Ok(std::mem::take(&mut *data));
        }
        if prompt.trim().is_empty() {
            return Err("touchIdPrompt cannot be empty".into());
        }
        autoreleasepool(|_| {
            // A fresh context prevents reusing authentication from a previous unlock.
            let context = unsafe { LAContext::new() };
            unsafe { context.setLocalizedReason(&NSString::from_str(prompt)) };
            let result = (|| {
                let key = find_key(Some((&context, prompt)))?
                    .ok_or("SecKeyCreateDecryptedData: Key not found in Secure Enclave")?;
                key.decrypt_data(ALGORITHM, &data)
                    .map_err(|err| security_error("SecKeyCreateDecryptedData", err.code(), &err.to_string()))
            })();
            unsafe { context.invalidate() };
            result
        })
    }

    pub fn delete_key() -> Result<(), String> {
        let mut memory = MEMORY_KEY.lock().map_err(|err| err.to_string())?;
        if let Some(persistent) = emulation()? {
            if persistent {
                let entry = keyring::Entry::new("KeeWeb", "emulated-hardware-key")
                    .map_err(|err| err.to_string())?;
                match entry.delete_credential() {
                    Ok(()) | Err(keyring::Error::NoEntry) => {}
                    Err(err) => return Err(err.to_string()),
                }
            }
            *memory = None;
            return Ok(());
        }
        autoreleasepool(|_| {
            if let Some(key) = find_key(None)? {
                match key.delete() {
                    Ok(()) => {}
                    Err(err) if err.code() == errSecItemNotFound => {}
                    Err(err) => return Err(security_error("SecItemDelete", err.code() as isize, &err.to_string())),
                }
            }
            Ok(())
        })
    }

    fn security_error(operation: &str, code: isize, message: &str) -> String {
        // open-view.js recognizes this phrase as cancellation, rather than a bad password.
        if code == -128 {
            format!("User refused to authenticate with Touch ID ({operation}: {message})")
        } else {
            format!("{operation}: {message} ({code})")
        }
    }

    fn find_key(authentication: Option<(&LAContext, &str)>) -> Result<Option<SecKey>, String> {
        // ItemSearchOptions cannot query kSecAttrApplicationTag or set an operation prompt.
        // Do not substitute application_label: that is the public-key hash, not our tag.
        let tag = CFData::from_buffer(KEY_TAG.as_bytes());
        let mut query = unsafe {
            CFMutableDictionary::from_CFType_pairs(&[
                (kSecClass.to_void(), kSecClassKey.to_void()),
                (kSecAttrKeyClass.to_void(), kSecAttrKeyClassPrivate.to_void()),
                (kSecAttrKeyType.to_void(), kSecAttrKeyTypeECSECPrimeRandom.to_void()),
                (kSecAttrApplicationTag.to_void(), tag.to_void()),
                (kSecAttrTokenID.to_void(), kSecAttrTokenIDSecureEnclave.to_void()),
                (kSecUseDataProtectionKeychain.to_void(), CFBoolean::true_value().to_void()),
                (kSecReturnRef.to_void(), CFBoolean::true_value().to_void()),
            ])
        };
        if let Some((context, prompt)) = authentication {
            let prompt = CFString::new(prompt);
            unsafe {
                query.set(kSecUseOperationPrompt.to_void(), prompt.to_void());
                query.set(kSecUseAuthenticationContext.to_void(),
                    (context as *const LAContext).cast());
            }
        }
        let mut result = ptr::null();
        let status = unsafe { SecItemCopyMatching(query.as_concrete_TypeRef(), &mut result) };
        if status == errSecItemNotFound {
            return Ok(None);
        }
        if status != 0 {
            return Err(security_error("SecItemCopyMatching", status as isize, &Error::from_code(status).to_string()));
        }
        if result.is_null() {
            return Err("SecItemCopyMatching returned no Secure Enclave key".into());
        }
        Ok(Some(unsafe { SecKey::wrap_under_create_rule(result.cast_mut().cast()) }))
    }

    #[allow(deprecated)] // Watch policy and the dictionary bridge preserve the Electron key contract.
    fn create_key() -> Result<SecKey, String> {
        let context = unsafe { LAContext::new() };
        let available = unsafe {
            context.canEvaluatePolicy_error(LAPolicy::DeviceOwnerAuthenticationWithBiometricsOrWatch)
        };
        let biometrics = match available {
            Ok(()) => true,
            // Lockout is temporary; never weaken the access policy because of failed scans.
            Err(err) if err.code() == LAError::BiometryLockout.0 => true,
            Err(err) if err.code() == LAError::BiometryNotAvailable.0
                || err.code() == LAError::BiometryNotEnrolled.0
                || err.code() == LAError::CompanionNotAvailable.0
                || err.code() == LAError::BiometryNotPaired.0
                || err.code() == LAError::BiometryDisconnected.0 => false,
            Err(err) => return Err(format!("Cannot determine biometric availability: {err}")),
        };
        let flags = kSecAccessControlPrivateKeyUsage | if biometrics {
            kSecAccessControlBiometryCurrentSet | kSecAccessControlOr | kSecAccessControlWatch
        } else {
            kSecAccessControlUserPresence
        };
        let access = SecAccessControl::create_with_protection(
            Some(ProtectionMode::AccessibleWhenUnlockedThisDeviceOnly), flags,
        ).map_err(|err| err.to_string())?;
        let mut options = GenerateKeyOptions::default();
        options.set_key_type(KeyType::ec_sec_prime_random())
            .set_size_in_bits(256)
            .set_token(Token::SecureEnclave)
            .set_location(Location::DataProtectionKeychain)
            .set_label(KEY_TAG);
        let attributes = options.to_dictionary();
        let mut attributes = unsafe {
            CFMutableDictionary::wrap_under_create_rule(CFDictionaryCreateMutableCopy(
                kCFAllocatorDefault, 0, attributes.as_concrete_TypeRef(),
            ))
        };
        let tag = CFData::from_buffer(KEY_TAG.as_bytes());
        let private_attributes = unsafe {
            CFMutableDictionary::from_CFType_pairs(&[
                (kSecAttrIsPermanent.to_void(), CFBoolean::true_value().to_void()),
                (kSecAttrApplicationTag.to_void(), tag.to_void()),
                (kSecAttrAccessControl.to_void(), access.to_void()),
            ])
        };
        unsafe {
            attributes.set(kSecPrivateKeyAttrs.to_void(), private_attributes.to_void());
            // Only the enclave private key is persisted, just as in secure-enclave 0.4.1.
            attributes.remove(kSecPublicKeyAttrs.to_void());
        }
        SecKey::generate(attributes.to_immutable())
            .map_err(|err| security_error("SecKeyCreateRandomKey", err.code(), &err.to_string()))
    }
}
