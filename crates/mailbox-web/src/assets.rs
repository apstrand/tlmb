//! What the browser is served.
//!
//! §3 says the web interface is compiled into every build, so these are
//! compiled in rather than read from a directory beside the binary: a mail
//! client that stops working when a file moves is not self-contained, and a
//! path read at runtime is a path someone can substitute.
//!
//! Three files and no build step. That is a consequence of §16's decision to
//! build the graphical interface from DOM: there is no bundler, no wasm
//! module, and no toolchain to install before `cargo build` produces something
//! that works. `mailbox gui` shows the same three files in a webview.

use crate::http::Response;

const INDEX: &str = include_str!("../web/index.html");
const STYLE: &str = include_str!("../web/style.css");
const SCRIPT: &str = include_str!("../web/mailbox.js");

pub const UNAUTHORIZED: &str = include_str!("../web/unauthorized.html");

pub fn serve(path: &str, token: Option<&str>) -> Response {
    match path {
        "/" | "/index.html" => {
            // The token is written into the page rather than left in the URL or
            // read from the cookie: the cookie is `HttpOnly` by design, and the
            // API wants the token in a header (see `server`).
            let page = INDEX.replace("{{TOKEN}}", token.unwrap_or(""));
            Response::new(200, "text/html; charset=utf-8", page.into_bytes())
                // The page loads nothing from anywhere else, and says so. A
                // sender's HTML renders in a sandboxed frame under its own,
                // stricter policy (§9).
                .with(
                    "Content-Security-Policy",
                    // `style-src` allows inline because a `srcdoc` frame
                    // inherits this policy, and the frame that renders a
                    // message carries its own stylesheet inline — it has a
                    // unique origin and cannot fetch one. The frame then
                    // narrows the policy again for the sender's markup.
                    "default-src 'none'; script-src 'self'; \
                     style-src 'self' 'unsafe-inline'; connect-src 'self'; \
                     img-src 'self' data:; frame-src 'self'; \
                     base-uri 'none'; form-action 'none'",
                )
        }
        "/style.css" => Response::new(200, "text/css; charset=utf-8", STYLE.as_bytes().to_vec()),
        "/mailbox.js" => Response::new(
            200,
            "text/javascript; charset=utf-8",
            SCRIPT.as_bytes().to_vec(),
        ),
        _ => Response::text(404, "not found"),
    }
}

#[cfg(test)]
mod tests {
    /// The DOM client *is* the GUI. These are the keys §7 shares with the
    /// terminal, plus `g` for sync. A missing one here is a missing one in
    /// the window.
    #[test]
    fn the_client_has_the_terminals_keys() {
        let js = super::SCRIPT;
        for needle in [
            "j:", "k:", "u:", "f:", "s:", "d:", "z:", r#""/":"#, "m:", "r:", "R:", "F:", "g:",
        ] {
            assert!(
                js.contains(needle),
                "mailbox.js is missing the {needle} binding"
            );
        }
        assert!(
            js.contains("__MAILBOX_SHELL__"),
            "the desktop shell has no way to introduce itself"
        );
        assert!(
            js.contains("multi-account"),
            "§7's per-account colour has nowhere to land"
        );
    }
}
