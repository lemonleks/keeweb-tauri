const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');

const root = path.resolve(__dirname, '..');
const targets = {
    'aarch64-apple-darwin': 'darwin-arm64',
    'x86_64-apple-darwin': 'darwin-x64',
    'x86_64-pc-windows-msvc': 'win32-x64',
    'aarch64-pc-windows-msvc': 'win32-arm64',
    'i686-pc-windows-msvc': 'win32-ia32',
    'x86_64-unknown-linux-gnu': 'linux-x64'
};

const triple =
    process.env.TAURI_ENV_TARGET_TRIPLE ||
    process.env.CARGO_BUILD_TARGET ||
    execFileSync('rustc', ['-vV'], { encoding: 'utf8' }).match(/^host: (.+)$/m)?.[1];
const sourcePlatform = targets[triple];
if (!sourcePlatform) {
    throw new Error(`No native messaging host is available for Rust target ${triple}`);
}

const extension = sourcePlatform.startsWith('win32-') ? '.exe' : '';
const name = 'keeweb-native-messaging-host';
const source = path.join(
    root,
    'node_modules/@keeweb/keeweb-native-messaging-host',
    sourcePlatform,
    name + extension
);
const destination = path.join(root, 'src-tauri/binaries', `${name}-${triple}${extension}`);
fs.mkdirSync(path.dirname(destination), { recursive: true });
fs.copyFileSync(source, destination);
fs.chmodSync(destination, 0o755);
process.stdout.write(`Prepared ${path.relative(root, destination)}\n`);

if (sourcePlatform.startsWith('darwin-')) {
    const teamId = process.env.APPLE_TEAM_ID;
    if (teamId && !/^[A-Z0-9]{10}$/.test(teamId)) {
        throw new Error('APPLE_TEAM_ID must be the 10-character Apple Developer team identifier');
    }
    if (
        process.env.APPLE_SIGNING_IDENTITY &&
        process.env.APPLE_SIGNING_IDENTITY !== '-' &&
        !teamId
    ) {
        throw new Error(
            'Signed macOS builds require APPLE_TEAM_ID for Secure Enclave keychain entitlements'
        );
    }
    const entries = teamId
        ? `
    <key>keychain-access-groups</key>
    <array><string>${teamId}.net.antelle.keeweb</string></array>
    <key>com.apple.application-identifier</key>
    <string>${teamId}.net.antelle.keeweb</string>
    <key>com.apple.developer.team-identifier</key>
    <string>${teamId}</string>
`
        : '';
    fs.writeFileSync(
        path.join(root, 'src-tauri/Entitlements.plist'),
        `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.automation.apple-events</key>
    <true/>
${entries}</dict>
</plist>
`
    );
}
