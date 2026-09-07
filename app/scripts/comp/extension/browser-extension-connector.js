import { invoke } from '@tauri-apps/api/core';
import { Events } from 'framework/events';
import { Launcher } from 'comp/launcher';
import { Logger } from 'util/logger';
import { ProtocolImpl } from './protocol-impl';
import { RuntimeInfo } from 'const/runtime-info';
import { AppSettingsModel } from 'models/app-settings-model';
import { Features } from 'util/features';

const WebConnectionInfo = {
    connectionId: 1,
    extensionName: 'KeeWeb Connect',
    supportsNotifications: true
};

const SupportedExtensions = [
    { alias: 'KWC', name: 'KeeWeb Connect' },
    { alias: 'KPXC', name: 'KeePassXC-Browser' }
];
const SupportedBrowsers = ['Chrome', 'Firefox', 'Edge', 'Other'];
if (Features.isMac) {
    SupportedBrowsers.unshift('Safari');
}

const logger = new Logger('browser-extension-connector');
if (!localStorage.debugBrowserExtension) {
    logger.level = Logger.Level.Info;
}

const connections = new Map();
const pendingBrowserMessages = [];
let processingBrowserMessage = false;

const BrowserExtensionConnector = {
    started: false,
    logger,

    init(appModel) {
        const sendEvent = this.sendEvent.bind(this);
        ProtocolImpl.init({ appModel, logger, sendEvent });

        this.browserWindowMessage = this.browserWindowMessage.bind(this);

        if (Launcher) {
            Events.on('browser-extension-socket-connected', ({ socketId, connectionInfo }) =>
                this.socketConnected(socketId, connectionInfo)
            );
            Events.on('browser-extension-socket-closed', ({ socketId }) =>
                this.socketClosed(socketId)
            );
            Events.on('browser-extension-socket-request', ({ socketId, request }) =>
                this.socketRequest(socketId, request)
            );

            AppSettingsModel.on('change', () => this.appSettingsChanged());
        }

        if (this.isEnabled()) {
            this.start();
        }
    },

    start() {
        if (Launcher) {
            this.startDesktopAppListener();
        } else {
            this.startWebMessageListener();
        }

        this.started = true;
    },

    stop() {
        if (Launcher) {
            this.stopDesktopAppListener();
        } else {
            this.stopWebMessageListener();
        }

        ProtocolImpl.cleanup();
        connections.clear();

        this.started = false;
    },

    appSettingsChanged() {
        if (this.isEnabled()) {
            if (!this.started) {
                this.start();
            }
        } else if (this.started) {
            this.stop();
        }
    },

    isEnabled() {
        if (!Launcher) {
            return true;
        }
        for (const ext of SupportedExtensions) {
            for (const browser of SupportedBrowsers) {
                if (AppSettingsModel[`extensionEnabled${ext.alias}${browser}`]) {
                    return true;
                }
            }
        }
        return false;
    },

    startWebMessageListener() {
        window.addEventListener('message', this.browserWindowMessage);
        logger.info('Started');
    },

    stopWebMessageListener() {
        window.removeEventListener('message', this.browserWindowMessage);
    },

    enable(browser, extension, enabled) {
        return invoke('browser_extension_connector_enable', {
            browser,
            extension,
            enabled
        }).catch((err) => logger.error('Error installing extension', err));
    },

    async startDesktopAppListener() {
        return invoke('browser_extension_connector_start', {
            config: { appleTeamId: RuntimeInfo.appleTeamId }
        }).catch((err) => logger.error('Error starting browser extension connector', err));
    },

    stopDesktopAppListener() {
        return invoke('browser_extension_connector_stop').catch((err) =>
            logger.error('Error stopping browser extension connector', err)
        );
    },

    browserWindowMessage(e) {
        if (e.origin !== location.origin) {
            return;
        }
        if (e.source !== window) {
            return;
        }
        if (e?.data?.kwConnect !== 'request') {
            return;
        }
        logger.debug('Extension -> KeeWeb', e.data);
        pendingBrowserMessages.push(e.data);
        this.processBrowserMessages();
    },

    async processBrowserMessages() {
        if (!pendingBrowserMessages.length || processingBrowserMessage) {
            return;
        }

        if (!connections.has(WebConnectionInfo.connectionId)) {
            connections.set(WebConnectionInfo.connectionId, WebConnectionInfo);
        }

        processingBrowserMessage = true;

        const request = pendingBrowserMessages.shift();

        const response = await ProtocolImpl.handleRequest(request, WebConnectionInfo);

        processingBrowserMessage = false;

        if (response) {
            this.sendWebResponse(response);
        }

        this.processBrowserMessages();
    },

    sendWebResponse(response) {
        logger.debug('KeeWeb -> Extension', response);
        response.kwConnect = 'response';
        postMessage(response, window.location.origin);
    },

    sendSocketEvent(data) {
        return invoke('browser_extension_connector_socket_event', { data }).catch((err) =>
            logger.error('Error sending browser extension event', err)
        );
    },

    sendSocketResult(socketId, data) {
        return invoke('browser_extension_connector_socket_result', {
            socketId,
            result: data
        }).catch((err) => logger.error('Error sending browser extension response', err));
    },

    sendEvent(data) {
        if (!this.isEnabled() || !connections.size) {
            return;
        }
        if (Launcher) {
            this.sendSocketEvent(data);
        } else {
            this.sendWebResponse(data);
        }
    },

    socketConnected(socketId, connectionInfo) {
        connections.set(socketId, connectionInfo);
    },

    socketClosed(socketId) {
        connections.delete(socketId);
        ProtocolImpl.deleteConnection(socketId);
    },

    async socketRequest(socketId, request) {
        let result;

        const connectionInfo = connections.get(socketId);
        if (connectionInfo) {
            result = await ProtocolImpl.handleRequest(request, connectionInfo);
        } else {
            const message = `Connection not found: ${socketId}`;
            result = ProtocolImpl.errorToResponse({ message }, request);
        }

        this.sendSocketResult(socketId, result);
    },

    get sessions() {
        return ProtocolImpl.sessions;
    },

    terminateConnection(connectionId) {
        connectionId = +connectionId;
        if (Launcher) {
            return invoke('browser_extension_connector_close_socket', {
                socketId: connectionId
            }).catch((err) => logger.error('Error closing browser extension connection', err));
        } else {
            ProtocolImpl.deleteConnection(connectionId);
        }
    },

    getClientPermissions(clientId) {
        return ProtocolImpl.getClientPermissions(clientId);
    },

    setClientPermissions(clientId, permissions) {
        ProtocolImpl.setClientPermissions(clientId, permissions);
    }
};

export { BrowserExtensionConnector, SupportedExtensions, SupportedBrowsers };
