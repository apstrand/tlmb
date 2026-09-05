# mailbox for iOS

The M6 mobile shell of [SPEC.md](../SPEC.md) §3: the Rust core compiled to a
static library, with a thin SwiftUI UI on top. No web view.

```
ios/Mailbox.xcodeproj     open this
crates/mailbox-ffi        the C ABI the app links
```

## Building

Xcode 16+ (this tree was last built with Xcode 26), an iOS 18 simulator or
device, and the `aarch64-apple-ios` / `aarch64-apple-ios-sim` Rust targets:

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
open ios/Mailbox.xcodeproj
```

Hit Run. A build phase compiles `mailbox-ffi` for the active SDK and copies
`libmailbox_ffi.a` into the product. Xcode launched from the Dock does not
inherit your shell `PATH`, so the phase looks for cargo in `~/.cargo/bin`
itself. Set your Development Team in the target if you want to run on a device.

Command line:

```sh
xcodebuild -project ios/Mailbox.xcodeproj -scheme mailbox \
  -destination 'platform=iOS Simulator,name=iPhone 16' \
  -configuration Debug build
```

## What it does

- Add an account (address + password; Gmail / Fastmail / Outlook / Yahoo /
  iCloud / Proton hosts are filled in from the built-in table)
- Password goes to the iOS Keychain, never to `config.toml`
- Unified inbox, pull-to-refresh sync, search with the §6.2 grammar
- Open a thread, archive / delete / flag with swipe
- Compose, reply, reply-all, forward — queued in the durable outbox and
  flushed on the next sync

Background push (the optional notifier of §11) is not in this first cut.
Sync is pull-to-refresh and on launch.
