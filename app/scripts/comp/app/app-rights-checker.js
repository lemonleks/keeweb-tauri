import { Launcher } from 'comp/launcher';
import { Alerts } from 'comp/ui/alerts';
import { AppSettingsModel } from 'models/app-settings-model';
import { Features } from 'util/features';
import { Locale } from 'util/locale';
import { Logger } from 'util/logger';

const logger = new Logger('app-rights-checker');

const AppRightsChecker = {
    AppPath: '/Applications/KeeWeb.app',

    init() {
        if (!Features.isDesktop || !Features.isMac) {
            return;
        }
        if (AppSettingsModel.skipFolderRightsWarning) {
            return;
        }
        if (!Launcher.getAppPath().startsWith(this.AppPath)) {
            return;
        }
        this.needRunInstaller((needRun) => {
            if (needRun) {
                this.showAlert();
                logger.warn(
                    'Application folder is not root-owned; change ownership manually',
                    this.AppPath
                );
            }
        });
    },

    needRunInstaller(callback) {
        Launcher.statFile(this.AppPath, (stat) => {
            const folderIsRoot = stat && stat.uid === 0;
            callback(!folderIsRoot);
        });
    },

    showAlert() {
        const command = 'sudo chown -R root ' + this.AppPath;
        this.alert = Alerts.alert({
            icon: 'lock',
            header: Locale.appRightsAlert,
            body:
                Locale.appRightsAlertBody1.replace('{}', this.AppPath) +
                '\n' +
                Locale.appRightsAlertBody2,
            pre: command,
            buttons: [
                { result: 'skip', title: Locale.alertDoNotAsk, error: true },
                Alerts.buttons.ok
            ],
            success: (result) => {
                if (result === 'skip') {
                    this.dontAskAnymore();
                }
                this.alert = null;
            }
        });
    },

    dontAskAnymore() {
        this.needRunInstaller((needRun) => {
            if (needRun) {
                AppSettingsModel.skipFolderRightsWarning = true;
            }
        });
    }
};

export { AppRightsChecker };
