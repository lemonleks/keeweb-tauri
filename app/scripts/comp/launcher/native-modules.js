import { invoke as invokeCommand } from '@tauri-apps/api/core';
import { Events } from 'framework/events';
import { Launcher } from 'comp/launcher';

let NativeModules;

function nativeError(error) {
    return error instanceof Error
        ? error
        : Object.assign(
              new Error(typeof error === 'string' ? error : error.message),
              typeof error === 'object' ? error : {}
          );
}

function invoke(command, args) {
    return invokeCommand(command, args).catch((error) => {
        throw nativeError(error);
    });
}

if (Launcher) {
    let callbackId = 0;
    const callbacks = new Map();

    Events.on('native-modules-yubikey-chalresp-result', ({ callbackId, error, result }) => {
        const callback = callbacks.get(callbackId);
        if (!callback) {
            return;
        }
        if (error) {
            error = nativeError(error);
            if (error.code === 'YK_ENOKEY') {
                error.noKey = true;
            } else if (error.code === 'YK_ETIMEOUT') {
                error.timeout = true;
            }
        }
        if (!error?.touchRequested) {
            callbacks.delete(callbackId);
        }
        callback(error, result && new Uint8Array(result));
    });

    NativeModules = {
        startUsbListener() {
            return invoke('usb_listener_start');
        },
        stopUsbListener() {
            return invoke('usb_listener_stop');
        },
        getYubiKeys(config) {
            return invoke('yubikey_list', { config });
        },
        yubiKeyChallengeResponse(yubiKey, challenge, slot, callback) {
            const id = ++callbackId;
            callbacks.set(id, callback);
            return invoke('yubikey_challenge_response', {
                yubikey: yubiKey,
                challenge: Array.from(challenge),
                slot,
                callbackId: id
            }).catch((error) => {
                if (callbacks.delete(id)) {
                    callback(error);
                }
            });
        },
        yubiKeyCancelChallengeResponse() {
            return invoke('yubikey_cancel_challenge_response');
        },
        async argon2(password, salt, options) {
            return new Uint8Array(
                await invoke('argon2', {
                    password: Array.from(new Uint8Array(password)),
                    salt: Array.from(new Uint8Array(salt)),
                    options
                })
            );
        },
        hardwareCryptoDeleteKey() {
            return invoke('hardware_crypto_delete_key');
        },
        async hardwareEncrypt(data) {
            return new Uint8Array(await invoke('hardware_encrypt', { data: Array.from(data) }));
        },
        async hardwareDecrypt(data, touchIdPrompt) {
            return new Uint8Array(
                await invoke('hardware_decrypt', {
                    data: Array.from(data),
                    touchIdPrompt
                })
            );
        },
        kbdGetActiveWindow(options) {
            return invoke('kbd_get_active_window', { options });
        },
        kbdGetActivePid() {
            return invoke('kbd_get_active_pid');
        },
        kbdShowWindow(id) {
            return invoke('kbd_show_window', { id });
        },
        kbdText(text) {
            return invoke('kbd_text', { text });
        },
        kbdTextAsKeys(text, modifiers = []) {
            return invoke('kbd_text_as_keys', { text, modifiers });
        },
        kbdKeyPress(code, modifiers = []) {
            return invoke('kbd_key_press', { code, modifiers });
        },
        kbdShortcut(code) {
            return invoke('kbd_shortcut', { code });
        },
        kbdKeyMoveWithModifier(down, modifiers = []) {
            return invoke('kbd_key_move_with_modifier', { down, modifiers });
        },
        kbdKeyPressWithCharacter(character, code, modifiers = []) {
            return invoke('kbd_key_press_with_character', { character, code, modifiers });
        },
        kbdEnsureModifierNotPressed() {
            return invoke('kbd_ensure_modifier_not_pressed');
        }
    };

    global.NativeModules = NativeModules;
}

export { NativeModules };
