let Launcher;

if (window.__TAURI_INTERNALS__) {
    Launcher = require('./launcher-tauri').Launcher;
}

export { Launcher };
