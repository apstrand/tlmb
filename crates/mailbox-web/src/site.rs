//! Static hosted-site generation.

use std::path::{Path, PathBuf};

use mailbox_core::error::{Error, Result};

const HOSTED: &str = include_str!("../web/hosted.js");
const WORKER: &str = include_str!("../web/hosted-worker.js");
const BRIDGE: &str = include_str!("../../mailbox-wasm/web/sab-bridge.js");
const STORAGE: &str = include_str!("../../mailbox-wasm/web/storage.js");

#[derive(Debug, Clone)]
pub struct Options {
    pub out: PathBuf,
    pub relay: Option<String>,
    /// Override used by packaging and tests. Normal source builds find the
    /// output of `mailbox-wasm/build-web.sh` beside this crate.
    pub wasm_pkg: Option<PathBuf>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Report {
    pub out: PathBuf,
    pub files: usize,
    pub relay: Option<String>,
}

pub fn generate(options: &Options) -> Result<Report> {
    if options.out.exists() && !options.out.is_dir() {
        return Err(Error::usage(format!(
            "{} exists and is not a directory",
            options.out.display()
        )));
    }
    let package = options
        .wasm_pkg
        .clone()
        .or_else(|| std::env::var_os("MAILBOX_WASM_PKG_DIR").map(PathBuf::from))
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../mailbox-wasm/web/pkg"));
    let binding = package.join("mailbox_wasm.js");
    let module = package.join("mailbox_wasm_bg.wasm");
    if !binding.is_file() || !module.is_file() {
        return Err(Error::usage(format!(
            "the wasm web package is missing at {}; run ./crates/mailbox-wasm/build-web.sh first",
            package.display()
        )));
    }

    std::fs::create_dir_all(options.out.join("pkg"))?;
    let config = serde_json::to_vec_pretty(&serde_json::json!({
        "backend": "wasm",
        "relay": options.relay,
    }))?;
    let headers = concat!(
        "/*\n",
        "  Cross-Origin-Opener-Policy: same-origin\n",
        "  Cross-Origin-Embedder-Policy: require-corp\n",
        "  Content-Security-Policy: default-src 'self'; connect-src 'self' ws: wss:; ",
        "worker-src 'self'; script-src 'self' 'wasm-unsafe-eval'; ",
        "style-src 'self'; img-src 'self' data: blob:; frame-src blob:; ",
        "base-uri 'none'; form-action 'none'\n",
    );
    let index = super::assets::index("", true, "hosted");
    let files: [(&str, &[u8]); 8] = [
        ("index.html", index.as_bytes()),
        ("style.css", super::assets::STYLE.as_bytes()),
        ("mailbox.js", super::assets::SCRIPT.as_bytes()),
        ("hosted.js", HOSTED.as_bytes()),
        ("hosted-worker.js", WORKER.as_bytes()),
        ("sab-bridge.js", BRIDGE.as_bytes()),
        ("storage.js", STORAGE.as_bytes()),
        ("config.json", &config),
    ];
    for (name, bytes) in files {
        std::fs::write(options.out.join(name), bytes)?;
    }
    std::fs::write(options.out.join("_headers"), headers)?;
    std::fs::copy(binding, options.out.join("pkg/mailbox_wasm.js"))?;
    std::fs::copy(module, options.out.join("pkg/mailbox_wasm_bg.wasm"))?;

    Ok(Report {
        out: options.out.clone(),
        files: 11,
        relay: options.relay.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_site_is_static_isolated_and_uses_the_shared_client() {
        let root = std::env::temp_dir().join(format!(
            "mailbox-site-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("site")
        ));
        let package = root.join("input");
        let out = root.join("dist");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("mailbox_wasm.js"), b"binding").unwrap();
        std::fs::write(package.join("mailbox_wasm_bg.wasm"), b"wasm").unwrap();

        let report = generate(&Options {
            out: out.clone(),
            relay: Some("wss://relay.example/".into()),
            wasm_pkg: Some(package),
        })
        .unwrap();
        assert_eq!(report.files, 11);
        let index = std::fs::read_to_string(out.join("index.html")).unwrap();
        assert!(index.contains(r#"data-backend="hosted""#));
        assert!(index.contains("mailbox.js"));
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(out.join("config.json")).unwrap()).unwrap();
        assert_eq!(config["relay"], "wss://relay.example/");
        let headers = std::fs::read_to_string(out.join("_headers")).unwrap();
        assert!(headers.contains("Cross-Origin-Opener-Policy: same-origin"));
        assert!(headers.contains("Cross-Origin-Embedder-Policy: require-corp"));
        assert_eq!(
            std::fs::read(out.join("pkg/mailbox_wasm_bg.wasm")).unwrap(),
            b"wasm"
        );

        std::fs::remove_dir_all(root).unwrap();
    }
}
