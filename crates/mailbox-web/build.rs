//! Makes sure the embedded-bundle paths exist, whether or not one was built.
//!
//! §3 says the web interface is compiled into every build, and it is — into
//! every build made the documented way, which is `./build-web.sh` before
//! `cargo build`. What is deliberately *not* done is committing the two and a
//! half megabytes of wasm to git and regenerating it on every change to the
//! interface: that is a repository that grows by a bundle per commit, forever,
//! and git has no way to forget it later.
//!
//! So a clone compiles with empty placeholders, `mailbox serve` runs, the API
//! works, and the page says the interface was not built into this binary and
//! names the command. That is the honest failure, and it is one line away from
//! being fixed by whoever hits it.

use std::path::Path;

fn main() {
    let dist = Path::new("web/dist");
    let _ = std::fs::create_dir_all(dist);
    for name in ["mailbox_bg.wasm", "mailbox.js"] {
        let path = dist.join(name);
        if !path.exists() {
            let _ = std::fs::write(&path, b"");
        }
    }
    println!("cargo:rerun-if-changed=web/dist");
}
