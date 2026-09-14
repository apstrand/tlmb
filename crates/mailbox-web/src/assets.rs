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
pub(crate) const STYLE: &str = include_str!("../web/style.css");
pub(crate) const SCRIPT: &str = include_str!("../web/mailbox.js");

pub const UNAUTHORIZED: &str = include_str!("../web/unauthorized.html");

pub(crate) fn index(token: &str, setup_allowed: bool, backend: &str) -> String {
    INDEX
        .replace("{{TOKEN}}", token)
        .replace("{{SETUP}}", if setup_allowed { "1" } else { "" })
        .replace("{{BACKEND}}", backend)
}

pub fn serve(path: &str, token: Option<&str>, setup_allowed: bool) -> Response {
    match path {
        "/" | "/index.html" => {
            // The token is written into the page rather than left in the URL or
            // read from the cookie: the cookie is `HttpOnly` by design, and the
            // API wants the token in a header (see `server`).
            //
            // Whether the setup form is offered is decided here too, rather
            // than by the page trying a write to find out: a form that appears
            // and then refuses on submit is worse than one that was never
            // shown, and the answer depends on how this server was bound.
            let page = index(token.unwrap_or(""), setup_allowed, "http");
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

    /// The packaged desktop app updates itself (mailbox-ui's `update.rs`), and
    /// the only place it can say so is this page. The names are the shell's
    /// side of the contract: the event it dispatches and the commands it
    /// grants.
    #[test]
    fn the_page_can_show_a_desktop_update() {
        assert!(super::INDEX.contains(r#"id="update-action""#));
        for needle in [
            "mailbox-update",
            "update_status",
            "update_install",
            "update_restart",
        ] {
            assert!(
                super::SCRIPT.contains(needle),
                "mailbox.js is missing {needle}"
            );
        }
    }

    /// `serve` and `gui` start whether or not an account exists (§9), and this
    /// is the only page either of them has — so the way to add one is carried
    /// here or nowhere. A browser tab and the desktop window are exactly the
    /// two places where there may be no terminal.
    #[test]
    fn the_page_carries_the_first_launch_setup() {
        assert!(
            super::INDEX.contains(r#"id="setup-form""#),
            "the page has no way to add an account"
        );
        for field in ["setup-address", "setup-password", "setup-imap-host"] {
            assert!(super::INDEX.contains(field), "the form has no {field}");
        }
        assert!(
            super::INDEX.contains("mailbox accounts add"),
            "the off-loopback fallback does not say what to type instead"
        );
        assert!(
            super::SCRIPT.contains("/api/accounts"),
            "the client never asks whether an account exists, so it cannot \
             tell an empty mailbox from an unconfigured one"
        );
        assert!(
            super::INDEX.contains(r#"id="setup-test""#),
            "the form has no way to try the settings before committing to them"
        );
        for route in ["/api/providers", "/api/discover", "/api/accounts/test"] {
            assert!(
                super::SCRIPT.contains(route),
                "the form never asks {route}, so it cannot fill itself in"
            );
        }
    }

    /// The form is offered or withheld here, once, rather than by the page
    /// trying a write to find out.
    #[test]
    fn the_page_is_told_whether_it_may_add_an_account() {
        let allowed = super::serve("/", Some("t"), true);
        let refused = super::serve("/", Some("t"), false);

        let text = |r: &crate::http::Response| String::from_utf8_lossy(&r.body).into_owned();
        assert!(text(&allowed).contains(r#"data-setup="1""#));
        assert!(
            text(&refused).contains(r#"data-setup="""#),
            "a server that would refuse the write still offered the form"
        );
    }
}
