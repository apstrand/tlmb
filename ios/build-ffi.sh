#!/bin/sh
# Xcode Run Script phase. GUI-launched Xcode has a PATH of /usr/bin:/bin,
# so rustup's cargo is invisible unless we put it back.
set -euo pipefail

export PATH="${HOME}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:${PATH:-/usr/bin:/bin}"
if [ -f "${HOME}/.cargo/env" ]; then
  # rustup writes this; it prepends ~/.cargo/bin.
  # shellcheck disable=SC1091
  . "${HOME}/.cargo/env"
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found (looked in ${HOME}/.cargo/bin)." >&2
  echo "Install Rust from https://rustup.rs, then: rustup target add aarch64-apple-ios aarch64-apple-ios-sim" >&2
  echo "PATH=${PATH}" >&2
  exit 1
fi

cd "${SRCROOT}/.."

ARCHS_ARR=${ARCHS:-arm64}
ARCH=$(echo "${ARCHS_ARR}" | awk '{print $1}')
if [ "${PLATFORM_NAME:-iphonesimulator}" = "iphonesimulator" ]; then
  if [ "${ARCH}" = "x86_64" ]; then
    TARGET=x86_64-apple-ios
  else
    TARGET=aarch64-apple-ios-sim
  fi
else
  TARGET=aarch64-apple-ios
fi

export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-18.0}"
SDKROOT_PATH=$(xcrun --sdk "${PLATFORM_NAME:-iphonesimulator}" --show-sdk-path)
export SDKROOT="${SDKROOT_PATH}"

echo "Building mailbox-ffi for ${TARGET} with $(command -v cargo)"
cargo build -p mailbox-ffi --release --target "${TARGET}"
cp -f "target/${TARGET}/release/libmailbox_ffi.a" "${BUILT_PRODUCTS_DIR}/libmailbox_ffi.a"
