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

## Building on Linux with xtool

`Package.swift` + `xtool.yml` describe the same app as the Xcode project, for
[xtool](https://github.com/xtool-org/xtool). SwiftPM has no bridging header, so
`MailboxClient.swift` imports the `CMailbox` system-library target
(`CMailbox/module.modulemap` over `crates/mailbox-ffi/include/mailbox.h`).

Cross-compiling the Rust staticlib needs the Darwin SDK sysroot; cc-rs shells
out to `xcrun` otherwise:

```sh
export SDKROOT=$(swift sdk configure --show-configuration darwin aarch64-apple-ios \
  | sed -n 's/^sdkRootPath: //p')
export IPHONEOS_DEPLOYMENT_TARGET=18.0
export CC_aarch64_apple_ios=clang AR_aarch64_apple_ios=llvm-ar
export CFLAGS_aarch64_apple_ios="--target=arm64-apple-ios18.0 -isysroot $SDKROOT"
cargo build -p mailbox-ffi --release --target aarch64-apple-ios

cd ios && xtool dev run
```

`Package.swift` links `target/aarch64-apple-ios/release/libmailbox_ffi.a`, so
rebuild the crate before `xtool dev run` when the FFI changes.

## What it does

- A first launch with no accounts offers the two ways in: set one up here, or
  bring another device's across
- Add an account (address + password; Gmail / Fastmail / Outlook / Yahoo /
  iCloud / Proton hosts are filled in from the built-in table)
- Import the accounts from `mailbox pair --export-file` — the one §8 transport
  that needs no socket, so no LAN, no relay, and no re-typing passwords. §8.2's
  link and relay code are not here yet.
- Password goes to the iOS Keychain, never to `config.toml`
- Unified inbox, pull-to-refresh sync, search with the §6.2 grammar
- Open a thread, archive / delete / flag with swipe
- Compose, reply, reply-all, forward — queued in the durable outbox and
  flushed on the next sync

Background push (the optional notifier of §11) is not in this first cut.
Sync is pull-to-refresh and on launch.
