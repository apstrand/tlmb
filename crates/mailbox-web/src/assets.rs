//! What the browser is served.
//!
//! §3 says the web interface is compiled into every build, so these are
//! compiled in rather than read from a directory beside the binary: a mail
//! client that stops working when a file moves is not self-contained, and a
//! path that is read at runtime is a path someone can substitute.

use crate::http::Response;

/// The shell page. The UI itself is a wasm module the page loads.
const INDEX: &str = include_str!("../web/index.html");
const STYLE: &str = include_str!("../web/style.css");
const SCRIPT: &str = include_str!("../web/mailbox.js");

pub const UNAUTHORIZED: &str = include_str!("../web/unauthorized.html");

/// The wasm bundle, when this build has one.
///
/// Built by `cargo xtask web` and committed as a build artefact under
/// `web/dist`. A build without it still serves and still works — the page says
/// so rather than showing a blank canvas, which is the failure mode that wastes
/// an afternoon.
pub fn wasm() -> Option<&'static [u8]> {
    const BUNDLE: &[u8] = include_bytes!("../web/dist/mailbox_bg.wasm");
    (!BUNDLE.is_empty()).then_some(BUNDLE)
}

pub fn glue() -> Option<&'static str> {
    const GLUE: &str = include_str!("../web/dist/mailbox.js");
    (!GLUE.is_empty()).then_some(GLUE)
}

pub fn serve(path: &str, token: Option<&str>) -> Response {
    match path {
        "/" | "/index.html" => {
            // The token is inlined into the page rather than left in the URL
            // or read from the cookie: the cookie is `HttpOnly` by design, and
            // the API wants it in a header (see `server`).
            let page = INDEX.replace("{{TOKEN}}", token.unwrap_or("")).replace(
                "{{HAS_WASM}}",
                if wasm().is_some() { "true" } else { "false" },
            );
            Response::new(200, "text/html; charset=utf-8", page.into_bytes())
        }
        "/style.css" => Response::new(200, "text/css; charset=utf-8", STYLE.as_bytes().to_vec()),
        "/mailbox.js" => Response::new(
            200,
            "text/javascript; charset=utf-8",
            SCRIPT.as_bytes().to_vec(),
        ),
        "/wasm/mailbox.js" => match glue() {
            Some(js) => Response::new(
                200,
                "text/javascript; charset=utf-8",
                js.as_bytes().to_vec(),
            ),
            None => Response::text(404, "this build has no wasm bundle"),
        },
        "/wasm/mailbox_bg.wasm" => match wasm() {
            Some(bytes) => Response::new(200, "application/wasm", bytes.to_vec()),
            None => Response::text(404, "this build has no wasm bundle"),
        },
        _ => Response::text(404, "not found"),
    }
}
