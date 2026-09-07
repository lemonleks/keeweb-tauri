(() => {
    const invoke = window.__TAURI_INTERNALS__.invoke;
    const fmt = (a) =>
        a instanceof Error ? a.stack || a.message : typeof a === 'string' ? a : JSON.stringify(a);
    const send = (level, message) => invoke('dev_log', { level, message }).catch(() => {});
    for (const level of ['log', 'info', 'warn', 'error', 'debug']) {
        const orig = console[level];
        console[level] = (...args) => {
            orig.apply(console, args);
            send(level, args.map(fmt).join(' '));
        };
    }
    window.addEventListener('error', (e) =>
        send('error', `uncaught: ${e.message} @${e.filename}:${e.lineno}`)
    );
    window.addEventListener('unhandledrejection', (e) =>
        send('error', `unhandled: ${fmt(e.reason ?? 'unknown')}`)
    );
})();
