import { Events } from 'framework/events';
import { Launcher } from 'comp/launcher';
import { AppSettingsModel } from 'models/app-settings-model';
import { Logger } from 'util/logger';

const logger = new Logger('copy-paste');

const CopyPaste = {
    simpleCopy: !!(Launcher && Launcher.clipboardSupported),

    copy(text) {
        if (this.simpleCopy) {
            const clipboardSeconds = AppSettingsModel.clipboardSeconds;
            const written = Launcher.setClipboardText(text);
            if (clipboardSeconds > 0) {
                const clearClipboard = () =>
                    written
                        .then(() => Launcher.getClipboardText())
                        .then((current) => {
                            if (current === text) {
                                return Launcher.clearClipboardText();
                            }
                        })
                        .catch((err) => logger.error('Error clearing clipboard', err));
                const onClose = () => {
                    Launcher.clipboardClearPending = Promise.all([
                        Launcher.clipboardClearPending,
                        clearClipboard()
                    ]);
                };
                Events.on('main-window-will-close', onClose);
                setTimeout(() => {
                    clearClipboard();
                    Events.off('main-window-will-close', onClose);
                }, clipboardSeconds * 1000);
            }
            return written.then(
                () => ({ success: true, seconds: clipboardSeconds }),
                (err) => {
                    logger.error('Error copying to clipboard', err);
                    return false;
                }
            );
        } else {
            try {
                if (document.execCommand('copy')) {
                    return { success: true };
                }
            } catch (e) {}
            return false;
        }
    },

    createHiddenInput(text) {
        const hiddenInput = $('<input/>')
            .val(text)
            .attr({ type: 'text', 'class': 'hide-by-pos' })
            .appendTo(document.body);
        hiddenInput[0].selectionStart = 0;
        hiddenInput[0].selectionEnd = text.length;
        hiddenInput.focus();
        hiddenInput.on({
            'copy cut paste'() {
                setTimeout(() => hiddenInput.blur(), 0);
            },
            blur() {
                hiddenInput.remove();
            }
        });
    },

    copyHtml(html) {
        const el = document.createElement('div');
        el.style.userSelect = 'auto';
        el.style.webkitUserSelect = 'auto';
        el.style.mozUserSelect = 'auto';
        el.innerHTML = html;
        document.body.appendChild(el);

        const range = document.createRange();
        range.selectNodeContents(el);
        const sel = window.getSelection();
        sel.removeAllRanges();
        sel.addRange(range);

        const result = document.execCommand('copy');

        el.remove();
        return result;
    }
};

export { CopyPaste };
