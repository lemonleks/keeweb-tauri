import { invoke } from '@tauri-apps/api/core';
import { Launcher } from 'comp/launcher';
import { Logger } from 'util/logger';
import { StringFormat } from 'util/formatting/string-format';

const logger = new Logger('transport');

const Transport = {
    cacheFilePath(fileName) {
        return Launcher.getTempPath(fileName);
    },

    async httpGet(config) {
        const tmpFile = Launcher.getTempPath(config.file || 'http-' + crypto.randomUUID());
        let result;
        try {
            let cached = false;
            if (config.file) {
                if (config.cleanupOldFiles) {
                    const baseTempPath = Launcher.getTempPath();
                    const allFiles = await invoke('fs_read_dir', { path: baseTempPath });
                    for (const file of allFiles) {
                        if (
                            file !== config.file &&
                            StringFormat.replaceVersion(file, '0') ===
                                StringFormat.replaceVersion(config.file, '0')
                        ) {
                            await new Promise((resolve, reject) => {
                                Launcher.deleteFile(Launcher.joinPath(baseTempPath, file), (err) =>
                                    err ? reject(err) : resolve()
                                );
                            });
                        }
                    }
                }
                const stats = await new Promise((resolve, reject) => {
                    Launcher.statFile(tmpFile, (stats, err) => {
                        if (err && err.code !== 'ENOENT') {
                            reject(err);
                        } else {
                            resolve(stats);
                        }
                    });
                });
                cached = config.cache && stats && !stats.isDir && stats.size > 0;
                if (cached) {
                    logger.info('File already downloaded ' + config.url);
                }
            }
            if (!cached) {
                logger.info('GET ' + config.url);
                const proxy = await new Promise((resolve) =>
                    Launcher.resolveProxy(config.url, resolve)
                );
                logger.info(
                    'Request to ' +
                        config.url +
                        ' ' +
                        (proxy ? 'using proxy ' + proxy.host + ':' + proxy.port : 'without proxy')
                );
                await invoke('download_to_file', { url: config.url, path: tmpFile });
            }
            if (config.file) {
                result = tmpFile;
            } else {
                result = await new Promise((resolve, reject) => {
                    Launcher.readFile(tmpFile, null, (data, err) =>
                        err ? reject(err) : resolve(data)
                    );
                });
                if (config.text || config.json) {
                    result = new TextDecoder().decode(result);
                }
                if (config.json) {
                    try {
                        result = JSON.parse(result);
                    } catch (err) {
                        throw new Error('Error parsing JSON: ' + err.message);
                    }
                }
            }
        } catch (err) {
            logger.error('Cannot GET ' + config.url, err);
            if (config.file) {
                await Launcher.deleteFile(tmpFile);
            }
            config.error(err);
            return;
        } finally {
            if (!config.file) {
                await Launcher.deleteFile(tmpFile);
            }
        }
        config.success(result);
    }
};

export { Transport };
