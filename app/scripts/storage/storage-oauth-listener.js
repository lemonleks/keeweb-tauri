import EventEmitter from 'events';
import { invoke } from '@tauri-apps/api/core';
import { Events } from 'framework/events';
import { Logger } from 'util/logger';

const DefaultPort = 48149;
const logger = new Logger('storage-oauth-listener');

const StorageOAuthListener = {
    server: null,
    pending: Promise.resolve(),

    listen(storageName) {
        this.stop();
        const listener = new EventEmitter();
        const path = `/oauth-result/${storageName}.html`;
        // Keep the registered provider redirect URI; Rust binds its IPv4 loopback address.
        listener.redirectUri = `http://localhost:${DefaultPort}${path}`;
        const server = {
            onResult: (result) => {
                if (this.server !== server) {
                    return;
                }
                const url = new URL(result.url);
                if (
                    !['localhost', '127.0.0.1'].includes(url.hostname) ||
                    url.port !== String(DefaultPort) ||
                    !url.pathname.startsWith(path)
                ) {
                    return;
                }
                this.stop();
                if (result.error) {
                    logger.error('OAuth error', result.error);
                    listener.emit('error', 'OAuth: ' + result.error);
                } else {
                    logger.info('OAuth result with code received');
                    listener.emit('result', { state: result.state, code: result.code });
                }
            }
        };
        this.server = server;
        Events.on('oauth-listener-result', server.onResult);
        // Serialize stop/start so replacing a pending listener cannot close the new socket.
        this.pending = this.pending
            .then(() => {
                if (this.server === server) {
                    logger.info(`Starting OAuth listener on port ${DefaultPort}...`);
                    return invoke('oauth_listener_start', { port: DefaultPort, path });
                }
            })
            .then(() => {
                if (this.server === server) {
                    listener.emit('ready');
                }
            })
            .catch((err) => {
                if (this.server === server) {
                    this.server = null;
                    Events.off('oauth-listener-result', server.onResult);
                    logger.error('Failed to start OAuth listener', err);
                    listener.emit('error', 'Failed to start OAuth listener: ' + err);
                }
            });
        return listener;
    },

    stop() {
        if (this.server) {
            Events.off('oauth-listener-result', this.server.onResult);
            this.server = null;
            this.pending = this.pending
                .then(() => invoke('oauth_listener_stop'))
                .then(() => logger.info('OAuth listener stopped'))
                .catch((err) => logger.error('Failed to stop OAuth listener', err));
        }
        return this.pending;
    }
};

export { StorageOAuthListener };
