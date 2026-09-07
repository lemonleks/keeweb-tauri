# lemonkee

Offline KeePass (KDBX) password manager for macOS, forked from KeeWeb. Tauri v2 shell, WKWebView UI (jQuery/Handlebars). No network access by design.

## Audience and scene
One person, at their Mac, opening the vault a few times a day: find an entry, copy a password, auto-type, occasionally edit. Sits next to Passwords.app, Notes, Finder. Both light and dark mode, following the system.

## Mode
Operate. Scanability, native expectations and instant response outrank expression.

## Product truth that must survive any redesign
- Three-pane shell: sidebar (files, groups, tags, trash), entry list, entry details/inspector; settings replaces the list+details area; footer hosts open files, lock, generator, settings.
- Open screen: pick a local `.kdbx`, password, key file, YubiKey; recent files.
- Everything works with keyboard; Cmd-F search, arrows in list, Cmd-C copy field.
- Plugins/themes API of KeeWeb is not a commitment for this fork.

## Brand commitments
- Follows the macOS 26 (Tahoe) Human Interface Guidelines: system font stack, system colors, Liquid-Glass sidebar/toolbar layering, concentric rounded corners, standard control sizes. Reference craft bar: Passwords.app and Notes.
- Name: lemonkee. Icon: lemon.
- No UI transitions (instant state changes); no network.
