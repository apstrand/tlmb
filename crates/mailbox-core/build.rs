//! Tells cargo when the built-in OAuth2 credentials (§9, §16) have changed.
//!
//! `option_env!` reads the environment at compile time, and cargo does not
//! know that on its own: without these lines a rebuild after exporting a
//! client id keeps the old binary, which is the sort of thing that costs an
//! afternoon.

fn main() {
    for provider in ["GOOGLE", "MICROSOFT", "YAHOO"] {
        println!("cargo:rerun-if-env-changed=MAILBOX_{provider}_CLIENT_ID");
        println!("cargo:rerun-if-env-changed=MAILBOX_{provider}_CLIENT_SECRET");
    }
}
