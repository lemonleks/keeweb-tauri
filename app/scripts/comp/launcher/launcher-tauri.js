import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { open, save } from '@tauri-apps/plugin-dialog';
import { writeText, readText, clear } from '@tauri-apps/plugin-clipboard-manager';
import { openUrl } from '@tauri-apps/plugin-opener';
import { Events } from 'framework/events';
import { StartProfiler } from 'comp/app/start-profiler';
import { Locale } from 'util/locale';
import { Logger } from 'util/logger';
import { noop } from 'util/fn';

const logger = new Logger('launcher');
const paths = {};
const fileExistence = new Map();
let focused = false;
let maximized = false;
let hasOpenFiles = false;

function nativeError(error) {
    if (error instanceof Error) {
        return error;
    }
    const err = new Error(String(error));
    const code = /^([A-Z][A-Z0-9_]+):/.exec(err.message);
    if (code) {
        err.code = code[1];
    }
    return err;
}

function pathRoot(path) {
    return Launcher.platform() === 'win32'
        ? /^(?:[A-Za-z]:\\|\\\\[^\\]+\\[^\\]+\\?|\\)/.exec(path)?.[0] || ''
        : path.startsWith('/')
        ? '/'
        : '';
}

const Launcher = {
    name: 'tauri',
    startup: null,
    get version() {
        return this.startup?.version;
    },
    autoTypeSupported: true,
    thirdPartyStoragesSupported: true,
    clipboardSupported: true,
    platform() {
        return this.startup?.platform;
    },
    arch() {
        return this.startup?.arch;
    },
    openLink(href) {
        if (/^(http|https|ftp|sftp|mailto):/i.test(href)) {
            return openUrl(href).catch((err) => logger.error('Error opening link', err));
        }
    },
    devTools: true,
    openDevTools() {
        return invoke('open_devtools');
    },
    getSaveFileName(defaultPath, callback) {
        return save({
            title: Locale.launcherSave,
            defaultPath,
            filters: [{ name: Locale.launcherFileFilter, extensions: ['kdbx'] }]
        }).then(callback, (err) => {
            logger.error('Error showing save dialog', err);
            callback(null);
        });
    },
    openFileChooser(callback) {
        return open({
            multiple: false,
            directory: false,
            filters: [{ name: Locale.launcherFileFilter, extensions: ['kdbx'] }]
        })
            .then(async (path) => {
                if (!path) {
                    return callback(null, null);
                }
                const bytes = await invoke('fs_read', { path });
                const file = new File([bytes], this.parsePath(path).file);
                file.path = path;
                callback(null, file);
            })
            .catch((err) => callback(nativeError(err)));
    },
    getUserDataPath(fileName) {
        return this.joinPath(paths.userData, fileName || '');
    },
    getTempPath(fileName) {
        return this.joinPath(paths.temp, fileName || '');
    },
    getDocumentsPath(fileName) {
        return this.joinPath(paths.documents, fileName || '');
    },
    getAppPath(fileName) {
        return this.joinPath(paths.app, fileName || '');
    },
    getWorkDirPath(fileName) {
        return this.joinPath(paths.workDir, fileName || '');
    },
    joinPath(...parts) {
        const windows = this.platform() === 'win32';
        const sep = windows ? '\\' : '/';
        let path = parts.filter((part) => part !== '').join(sep);
        if (windows) {
            path = path.replace(/\//g, sep);
        }
        const root = pathRoot(path);
        const result = [];
        for (const part of path.slice(root.length).split(sep)) {
            if (!part || part === '.') {
                continue;
            }
            if (part === '..' && result.length && result[result.length - 1] !== '..') {
                result.pop();
            } else if (part !== '..' || !root) {
                result.push(part);
            }
        }
        const joined = root + result.join(sep);
        return (joined || '.') + (path.endsWith(sep) && result.length ? sep : '');
    },
    writeFile(path, data, callback = noop) {
        const bytes =
            typeof data === 'string' ? new TextEncoder().encode(data) : new Uint8Array(data);
        return invoke('fs_write', bytes, { headers: { path: encodeURIComponent(path) } }).then(
            () => {
                fileExistence.set(path, true);
                callback();
            },
            (err) => callback(nativeError(err))
        );
    },
    readFile(path, encoding, callback) {
        return invoke(encoding ? 'fs_read_text' : 'fs_read', { path }).then(
            (data) => callback(encoding ? data : new Uint8Array(data), undefined),
            (err) => callback(undefined, nativeError(err))
        );
    },
    fileExists(path, callback) {
        return invoke('fs_exists', { path }).then(
            (exists) => {
                fileExistence.set(path, exists);
                callback(exists);
            },
            (err) => {
                logger.error('Error checking file existence', path, err);
                callback(false);
            }
        );
    },
    fileExistsSync(path) {
        return fileExistence.get(path) === true;
    },
    deleteFile(path, callback = noop) {
        return invoke('fs_delete', { path }).then(
            () => {
                fileExistence.set(path, false);
                callback();
            },
            (err) => callback(nativeError(err))
        );
    },
    statFile(path, callback) {
        return invoke('fs_stat', { path }).then(
            (stats) => callback({ ...stats, mtime: new Date(stats.mtime) }, undefined),
            (err) => callback(undefined, nativeError(err))
        );
    },
    mkdir(path, callback = noop) {
        return invoke('fs_mkdir', { path }).then(
            () => callback(),
            (err) => callback(nativeError(err))
        );
    },
    parsePath(fileName) {
        const sep = this.platform() === 'win32' ? '\\' : '/';
        let path = sep === '\\' ? fileName.replace(/\//g, sep) : fileName;
        const root = pathRoot(path);
        while (path.length > root.length && path.endsWith(sep)) {
            path = path.slice(0, -1);
        }
        const split = path.lastIndexOf(sep);
        return {
            path: fileName,
            dir:
                path.length <= root.length
                    ? root || '.'
                    : split < 0
                    ? '.'
                    : path.slice(0, split) || root,
            file: path.slice(Math.max(split + 1, root.length))
        };
    },
    createFsWatcher(path) {
        let id;
        let closed = false;
        const callbacks = [];
        const onChange = (event) => {
            if (!closed && event.id === id) {
                for (const callback of callbacks) {
                    callback(event.kind, this.parsePath(event.path).file);
                }
            }
        };
        Events.on('fs-watch', onChange);
        const started = invoke('fs_watch_start', { path })
            .then((watcherId) => {
                id = watcherId;
                if (closed) {
                    return invoke('fs_watch_stop', { id });
                }
            })
            .catch((err) => {
                closed = true;
                Events.off('fs-watch', onChange);
                logger.error('Error watching directory', path, err);
            });
        return {
            on(event, callback) {
                if (event === 'change') {
                    callbacks.push(callback);
                }
                return this;
            },
            close() {
                if (closed) {
                    return started;
                }
                closed = true;
                Events.off('fs-watch', onChange);
                return id === undefined
                    ? started
                    : invoke('fs_watch_stop', { id }).catch((err) => {
                          logger.error('Error stopping directory watcher', path, err);
                      });
            }
        };
    },
    loadConfig(name) {
        return invoke('load_config', { name });
    },
    saveConfig(name, data) {
        return invoke('save_config', { name, data });
    },
    preventExit(e) {
        e.preventDefault?.();
        e.returnValue = false;
        return false;
    },
    exit() {
        this.exitRequested = true;
        return this.requestExit();
    },
    async requestExit() {
        await this.clipboardClearPending;
        await invoke('set_hook_before_quit', { hooked: false });
        return invoke('quit_app');
    },
    requestRestartAndUpdate(updateFilePath) {
        this.pendingUpdateFile = updateFilePath;
        return this.requestExit();
    },
    cancelRestart() {
        this.quitRequested = false;
        this.pendingUpdateFile = undefined;
    },
    get restartPending() {
        return !!this.pendingUpdateFile;
    },
    setClipboardText(text) {
        return writeText(text);
    },
    getClipboardText() {
        return readText();
    },
    clearClipboardText() {
        return clear();
    },
    quitOnRealQuitEventIfMinimizeOnQuitIsEnabled() {
        return !!(this.pendingUpdateFile || this.quitRequested);
    },
    minimizeApp() {
        return invoke('minimize_app', {
            labels: {
                restore: Locale.menuRestoreApp.replace('{}', 'KeeWeb'),
                quit: Locale.menuQuitApp.replace('{}', 'KeeWeb')
            }
        });
    },
    canDetectOsSleep() {
        return this.platform() !== 'linux';
    },
    updaterEnabled() {
        // ponytail: this fork has no release feed; wire tauri-plugin-updater when it does.
        return false;
    },
    resolveProxy(url, callback) {
        return invoke('resolve_proxy', { url }).then(callback, (err) => {
            logger.error('Error resolving proxy', err);
            callback(null);
        });
    },
    hideApp() {
        return invoke('hide_app');
    },
    isAppFocused() {
        return focused;
    },
    showMainWindow() {
        return invoke('show_main_window');
    },
    spawn(config) {
        const ts = logger.ts();
        const { complete, noStdOutLogging, throwOnStdErr, ...processConfig } = config;
        invoke('spawn_process', { config: processConfig }).then(
            ({ code, stdout, stderr }) => {
                stdout = stdout || '';
                stderr = stderr || '';
                const error =
                    code !== 0 ? 'Exit code ' + code : throwOnStdErr && stderr ? stderr : null;
                const msg = 'spawn ' + config.cmd + ': ' + code + ', ' + logger.ts(ts);
                if (error) {
                    logger.error(msg + '\n' + stdout + '\n' + stderr);
                } else {
                    logger.info(msg + (stdout && !noStdOutLogging ? '\n' + stdout : ''));
                }
                complete?.(error, stdout, code);
            },
            (err) => {
                logger.error('spawn error: ' + config.cmd + ', ' + logger.ts(ts), err);
                complete?.(nativeError(err));
            }
        );
    },
    checkOpenFiles() {
        this.readyToOpenFiles = true;
        if (this.pendingFileToOpen) {
            this.openFile(this.pendingFileToOpen);
            delete this.pendingFileToOpen;
        }
    },
    openFile(file) {
        if (this.readyToOpenFiles) {
            Events.emit('launcher-open-file', file);
        } else {
            this.pendingFileToOpen = file;
        }
    },
    setGlobalShortcuts(appSettings) {
        const shortcuts = {};
        for (const name of [
            'autoType',
            'copyPassword',
            'copyUser',
            'copyUrl',
            'copyOtp',
            'restoreApp'
        ]) {
            shortcuts[name] = appSettings['globalShortcut' + name[0].toUpperCase() + name.slice(1)];
        }
        return invoke('set_global_shortcuts', { shortcuts }).catch((err) => {
            logger.error('Error registering global shortcuts', err);
        });
    },
    minimizeMainWindow() {
        return getCurrentWindow().minimize();
    },
    maximizeMainWindow() {
        return getCurrentWindow().maximize();
    },
    restoreMainWindow() {
        return getCurrentWindow().unmaximize();
    },
    mainWindowMaximized() {
        return maximized;
    }
};

Events.on('files-open-state', (nextHasOpenFiles) => {
    if (hasOpenFiles === nextHasOpenFiles) {
        return;
    }
    hasOpenFiles = nextHasOpenFiles;
    invoke('set_has_open_files', { hasOpenFiles }).catch((err) =>
        logger.error('Error reporting open files', err)
    );
});

Events.on('launcher-exit-request', () => {
    Launcher.quitRequested = true;
    setTimeout(() => Events.emit('launcher-before-quit'), 0);
});
Events.on('launcher-minimize', () => setTimeout(() => Events.emit('app-minimized'), 0));
Events.on('launcher-maximize', () => {
    maximized = true;
    setTimeout(() => Events.emit('app-maximized'), 0);
});
Events.on('launcher-unmaximize', () => {
    maximized = false;
    setTimeout(() => Events.emit('app-unmaximized'), 0);
});
Events.on('main-window-focus', () => {
    focused = true;
});
Events.on('main-window-blur', () => {
    focused = false;
});
Events.on('launcher-started-minimized', () => setTimeout(() => Launcher.minimizeApp(), 0));
Events.on('start-profile', (data) => StartProfiler.reportAppProfile(data));
Events.once('app-ready', () =>
    setTimeout(() => {
        invoke('window_ready').catch((err) => logger.error('Error reporting window ready', err));
        Launcher.checkOpenFiles();
        if (Launcher.startup.openFile) {
            Launcher.openFile({
                data: Launcher.startup.openFile,
                key: Launcher.startup.openKeyfile
            });
        }
        if (Launcher.startup.startMinimized) {
            Launcher.minimizeApp();
        }
    }, 0)
);

const eventsReady = listen('app-event', (e) => {
    const { name, data } = e.payload;
    if (name === 'launcher-open-file') {
        Launcher.openFile(data);
    } else {
        Events.emit(name, data);
    }
});

Launcher.ready = Promise.all([
    invoke('set_has_open_files', { hasOpenFiles }),
    invoke('get_startup_info').then(async (startup) => {
        Launcher.startup = startup;
        if (startup.platform === 'darwin') {
            const path = '/usr/local/bin/ykman';
            fileExistence.set(path, await invoke('fs_exists', { path }));
        }
    }),
    ...['userData', 'temp', 'documents', 'app', 'workDir'].map(async (kind) => {
        paths[kind] = await invoke('get_path', { kind });
    }),
    eventsReady.then(async () => {
        focused = await invoke('is_app_focused');
        maximized = await getCurrentWindow().isMaximized();
        await invoke('set_hook_before_quit', { hooked: true });
    })
]);

export { Launcher };
