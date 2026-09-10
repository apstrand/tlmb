//! Finding a domain's mail servers from its address alone (§9).
//!
//! > **Setup is one line**: `mailbox accounts add peter@example.com`. Server
//! > settings are discovered via `autoconfig`/`autodiscover`, DNS SRV, the
//! > Mozilla ISPDB, and a small built-in table for the big providers. Manual
//! > overrides exist but are never required for a compliant host.
//!
//! Five sources, tried in the order below, first answer wins:
//!
//! 1. **The built-in table** ([`crate::providers`]). No network, and for the
//!    six domains it covers it is more accurate than what those domains
//!    publish about themselves.
//! 2. **The domain's own autoconfig** — `autoconfig.<domain>` and
//!    `<domain>/.well-known/autoconfig/`, Thunderbird's format, which is what
//!    a self-hosted server most often publishes.
//! 3. **The Mozilla ISPDB**, the same format, hosted centrally for providers
//!    that publish nothing themselves.
//! 4. **Autodiscover**, Microsoft's POST-an-XML-document flow, for Exchange.
//! 5. **DNS `SRV`** (RFC 6186), then the **`MX` host and the usual names** —
//!    `imap.<domain>`, `mail.<domain>`, the domain itself.
//!
//! ## Why the last step verifies and the others do not
//!
//! Sources 1–4 are assertions by someone entitled to make them: the domain
//! itself, a curated database, the mail host. Source 5 is inference. `SRV` is
//! unsigned, an `MX` record names an *inbound* server that may or may not also
//! speak IMAP, and `imap.<domain>` is a guess about a naming convention.
//!
//! So the last step does not write a config it has not proved: it opens the
//! connection, completes the TLS handshake against the system roots, and reads
//! the greeting. A host that fails any of that is not offered as an answer.
//! This is also what keeps an unsigned `SRV` record from being an attack — it
//! can point discovery at a host, but that host still has to present a
//! certificate valid for its own name over a connection §9's rules accept, and
//! the credential is never sent during discovery in any case.
//!
//! Nothing here runs unless a person asked for it. `accounts add` is the
//! caller, discovery happens once, and [`Options::network`] turns it off.

use std::net::SocketAddr;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::providers::ServerHints;

/// How long any one connection in the verify step may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(6);

/// Where a set of settings came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The built-in table (§9's "small built-in table").
    BuiltIn,
    /// `https://autoconfig.<domain>/mail/config-v1.1.xml`.
    Autoconfig,
    /// `https://<domain>/.well-known/autoconfig/mail/config-v1.1.xml`.
    WellKnown,
    /// `https://autoconfig.thunderbird.net/v1.1/<domain>`.
    Ispdb,
    /// Microsoft's `autodiscover.xml`.
    Autodiscover,
    /// `_imaps._tcp` / `_submission._tcp` SRV records.
    Srv,
    /// The `MX` host, or a conventional name, confirmed by connecting.
    Probe,
}

impl Source {
    /// How the result reads to a person: where the settings came from.
    pub fn describe(self) -> &'static str {
        match self {
            Source::BuiltIn => "the built-in table",
            Source::Autoconfig => "the domain's autoconfig",
            Source::WellKnown => "the domain's .well-known autoconfig",
            Source::Ispdb => "the Mozilla ISPDB",
            Source::Autodiscover => "autodiscover",
            Source::Srv => "DNS SRV records",
            Source::Probe => "connecting to the usual hosts",
        }
    }

    /// Whether this source asserts settings rather than infers them (see the
    /// module docs). An inferred answer is verified before it is offered.
    pub fn is_authoritative(self) -> bool {
        !matches!(self, Source::Srv | Source::Probe)
    }
}

/// One thing discovery tried, and what came of it. Reported so a domain that
/// yields nothing says *what* it was asked, rather than only that it failed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Step {
    pub source: Source,
    /// The URL or name that was asked.
    pub target: String,
    pub outcome: String,
}

/// Settings, and how much they are worth trusting.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Found {
    #[serde(flatten)]
    pub hints: HintsJson,
    pub source: Source,
    /// True when a connection to the IMAP host actually succeeded.
    pub verified: bool,
}

/// [`ServerHints`] in the shape the JSON contract shows it (§6.4).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HintsJson {
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imap_user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smtp_user: Option<String>,
}

impl From<ServerHints> for HintsJson {
    fn from(h: ServerHints) -> HintsJson {
        HintsJson {
            imap_host: h.imap_host,
            imap_port: h.imap_port,
            imap_security: h.imap_security,
            smtp_host: h.smtp_host,
            smtp_port: h.smtp_port,
            smtp_security: h.smtp_security,
            imap_user: h.imap_user,
            smtp_user: h.smtp_user,
        }
    }
}

impl From<HintsJson> for ServerHints {
    fn from(h: HintsJson) -> ServerHints {
        ServerHints {
            imap_host: h.imap_host,
            imap_port: h.imap_port,
            imap_security: h.imap_security,
            smtp_host: h.smtp_host,
            smtp_port: h.smtp_port,
            smtp_security: h.smtp_security,
            imap_user: h.imap_user,
            smtp_user: h.smtp_user,
        }
    }
}

/// What one discovery run learned.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Discovery {
    pub address: String,
    pub domain: String,
    /// `None` when every source came up empty — which is not an error, it is
    /// a domain that needs `--imap-host`.
    pub found: Option<Found>,
    pub steps: Vec<Step>,
}

impl Discovery {
    pub fn hints(&self) -> Option<ServerHints> {
        self.found.clone().map(|f| ServerHints::from(f.hints))
    }
}

/// How much of the chain to run.
#[derive(Debug, Clone)]
pub struct Options {
    /// Off: the built-in table only, and no packet leaves the machine.
    pub network: bool,
    /// Off: an inferred answer is returned unverified rather than connected to.
    /// Only the test suite and `--dry-run` want this.
    pub verify: bool,
    /// Empty means [`crate::dns::system_resolvers`].
    pub resolvers: Vec<SocketAddr>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            network: true,
            verify: true,
            resolvers: Vec::new(),
        }
    }
}

impl Options {
    /// The table and nothing else.
    pub fn offline() -> Options {
        Options {
            network: false,
            verify: false,
            resolvers: Vec::new(),
        }
    }

    /// The default, unless `MAILBOX_DISCOVERY` says otherwise.
    ///
    /// `off` (or `none`) is [`Options::offline`]; `no-verify` runs the network
    /// sources but connects to nothing. It exists for the same reason
    /// `MAILBOX_NOTIFIER=none` and `MAILBOX_OPENER` do: `cargo test` runs
    /// `accounts add`, and a test suite that quietly resolves and connects to
    /// whatever domain a fixture happened to name is a suite that fails on an
    /// aeroplane and leaks on a corporate network.
    pub fn from_env() -> Options {
        match std::env::var("MAILBOX_DISCOVERY").as_deref() {
            Ok("off") | Ok("none") => Options::offline(),
            Ok("no-verify") => Options {
                verify: false,
                ..Options::default()
            },
            _ => Options::default(),
        }
    }
}

/// Runs the chain for `address`.
///
/// Network failures are outcomes, not errors: a domain with no autoconfig host
/// refuses the connection, and that is a normal step of a successful
/// discovery. The only `Err` is an address that is not one.
pub fn discover(address: &str, opts: &Options) -> Result<Discovery> {
    let address = address.trim().to_string();
    let domain = address
        .rsplit_once('@')
        .map(|(_, d)| d.trim().to_ascii_lowercase())
        .filter(|d| !d.is_empty() && d.contains('.'))
        .ok_or_else(|| Error::usage(format!("`{address}` is not an email address")))?;

    let mut run = Run {
        address: address.clone(),
        domain: domain.clone(),
        opts,
        steps: Vec::new(),
    };
    let found = run.chain();
    Ok(Discovery {
        address,
        domain,
        found,
        steps: run.steps,
    })
}

struct Run<'a> {
    address: String,
    domain: String,
    opts: &'a Options,
    steps: Vec<Step>,
}

impl Run<'_> {
    fn chain(&mut self) -> Option<Found> {
        if let Some(hints) = crate::providers::hints_for(&self.address) {
            self.note(Source::BuiltIn, &self.domain.clone(), "known");
            return Some(self.accept(hints, Source::BuiltIn));
        }
        self.note(Source::BuiltIn, &self.domain.clone(), "not in the table");
        if !self.opts.network {
            return None;
        }

        for (source, url) in self.autoconfig_urls() {
            if let Some(hints) = self.fetch_autoconfig(source, &url) {
                return Some(self.accept(hints, source));
            }
        }
        if let Some(hints) = self.autodiscover() {
            return Some(self.accept(hints, Source::Autodiscover));
        }
        if let Some(hints) = self.srv() {
            return Some(self.accept(hints, Source::Srv));
        }
        self.probe()
    }

    /// Verifies an inferred answer, and records what verification found.
    fn accept(&mut self, hints: ServerHints, source: Source) -> Found {
        let verified = if source.is_authoritative() || !self.opts.verify {
            false
        } else {
            // `probe` verified as it went; anything else inferred is checked
            // here so no unverified guess is ever written to a config.
            true
        };
        Found {
            hints: hints.into(),
            source,
            verified,
        }
    }

    fn note(&mut self, source: Source, target: &str, outcome: impl Into<String>) {
        self.steps.push(Step {
            source,
            target: target.to_string(),
            outcome: outcome.into(),
        });
    }

    fn autoconfig_urls(&self) -> Vec<(Source, String)> {
        let d = &self.domain;
        let email = crate::https::percent_encode(&self.address);
        vec![
            (
                Source::Autoconfig,
                format!("https://autoconfig.{d}/mail/config-v1.1.xml?emailaddress={email}"),
            ),
            (
                Source::WellKnown,
                format!(
                    "https://{d}/.well-known/autoconfig/mail/config-v1.1.xml?emailaddress={email}"
                ),
            ),
            (
                Source::Ispdb,
                format!("https://autoconfig.thunderbird.net/v1.1/{d}"),
            ),
        ]
    }

    fn fetch_autoconfig(&mut self, source: Source, url: &str) -> Option<ServerHints> {
        match crate::https::get(url) {
            Ok(response) if response.status == 200 => {
                match parse_autoconfig(&response.body, &self.address) {
                    Some(hints) => {
                        self.note(source, url, "answered");
                        Some(hints)
                    }
                    None => {
                        self.note(source, url, "answered, but with no IMAP server in it");
                        None
                    }
                }
            }
            Ok(response) => {
                self.note(source, url, format!("HTTP {}", response.status));
                None
            }
            Err(e) => {
                self.note(source, url, short(&e));
                None
            }
        }
    }

    /// Microsoft's flow: POST a request document, read a `Protocol` list back.
    fn autodiscover(&mut self) -> Option<ServerHints> {
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
             <Autodiscover xmlns=\"http://schemas.microsoft.com/exchange/autodiscover/outlook/requestschema/2006\">\
             <Request><EMailAddress>{}</EMailAddress>\
             <AcceptableResponseSchema>http://schemas.microsoft.com/exchange/autodiscover/outlook/responseschema/2006a</AcceptableResponseSchema>\
             </Request></Autodiscover>",
            escape_xml(&self.address)
        );
        let urls = [
            format!(
                "https://autodiscover.{}/autodiscover/autodiscover.xml",
                self.domain
            ),
            format!("https://{}/autodiscover/autodiscover.xml", self.domain),
        ];
        for url in urls {
            match crate::https::post_xml(&url, &body) {
                Ok(response) if response.status == 200 => {
                    match parse_autodiscover(&response.body) {
                        Some(hints) => {
                            self.note(Source::Autodiscover, &url, "answered");
                            return Some(hints);
                        }
                        None => self.note(
                            Source::Autodiscover,
                            &url,
                            "answered, but with no IMAP protocol in it",
                        ),
                    }
                }
                Ok(response) => self.note(
                    Source::Autodiscover,
                    &url,
                    format!("HTTP {}", response.status),
                ),
                Err(e) => self.note(Source::Autodiscover, &url, short(&e)),
            }
        }
        None
    }

    fn resolvers(&self) -> Vec<SocketAddr> {
        if self.opts.resolvers.is_empty() {
            crate::dns::system_resolvers()
        } else {
            self.opts.resolvers.clone()
        }
    }

    /// RFC 6186: `_imaps._tcp` and `_submission._tcp`.
    ///
    /// The IMAP record decides the answer — an account with no way to receive
    /// is not an account — and submission is filled in from its own record or
    /// left to the probe step.
    fn srv(&mut self) -> Option<ServerHints> {
        let resolvers = self.resolvers();
        if resolvers.is_empty() {
            self.note(
                Source::Srv,
                &self.domain.clone(),
                "no nameserver configured",
            );
            return None;
        }

        let (imap_host, imap_port, imap_security) = self.srv_pick(
            &resolvers,
            &[
                (format!("_imaps._tcp.{}", self.domain), "implicit"),
                (format!("_imap._tcp.{}", self.domain), "starttls"),
            ],
        )?;

        let submission = self.srv_pick(
            &resolvers,
            &[
                (format!("_submission._tcp.{}", self.domain), "starttls"),
                (format!("_submissions._tcp.{}", self.domain), "implicit"),
            ],
        );
        let (smtp_host, smtp_port, smtp_security) = match submission {
            Some(found) => found,
            None => (imap_host.clone(), 587, "starttls".to_string()),
        };

        let candidate = ServerHints {
            imap_host,
            imap_port,
            imap_security,
            smtp_host,
            smtp_port,
            smtp_security,
            imap_user: None,
            smtp_user: None,
        };
        if !self.opts.verify {
            return Some(candidate);
        }
        match verify_imap(
            &candidate.imap_host,
            candidate.imap_port,
            &candidate.imap_security,
        ) {
            Ok(()) => Some(candidate),
            Err(e) => {
                // A record that names a host which does not answer is worse
                // than no record: writing it would produce an account that
                // fails on every command afterwards.
                self.note(
                    Source::Srv,
                    &candidate.imap_host,
                    format!("SRV named it, but {}", short(&e)),
                );
                None
            }
        }
    }

    /// Asks for each name in turn and takes the best record of the first that
    /// answers. `.` targets are already dropped by [`crate::dns::srv`].
    fn srv_pick(
        &mut self,
        resolvers: &[SocketAddr],
        names: &[(String, &str)],
    ) -> Option<(String, u16, String)> {
        for (name, security) in names {
            match crate::dns::srv(name, resolvers) {
                Ok(records) if records.is_empty() => self.note(Source::Srv, name, "no records"),
                Ok(records) => {
                    let best = &records[0];
                    self.note(Source::Srv, name, format!("{}:{}", best.target, best.port));
                    return Some((best.target.clone(), best.port, security.to_string()));
                }
                Err(e) => self.note(Source::Srv, name, short(&e)),
            }
        }
        None
    }

    /// The last resort: the `MX` host and the conventional names, each tried
    /// by actually connecting.
    fn probe(&mut self) -> Option<Found> {
        if !self.opts.verify {
            self.note(
                Source::Probe,
                &self.domain.clone(),
                "skipped: verification is off, and an unverified guess is not an answer",
            );
            return None;
        }

        let (imap_host, imap_port, imap_security) = self.probe_imap()?;
        // SMTP is chosen without connecting. A submission server answers a
        // greeting to anyone, so a probe would prove only that something is
        // listening; the mail path that matters is exercised by the first
        // send, which reports honestly (§6.6). Receiving is what discovery
        // must get right, and that is what it verified.
        let smtp_host = self
            .smtp_candidates(&imap_host)
            .into_iter()
            .next()
            .unwrap_or_else(|| imap_host.clone());

        Some(Found {
            hints: ServerHints {
                imap_host,
                imap_port,
                imap_security,
                smtp_host,
                smtp_port: 587,
                smtp_security: "starttls".into(),
                imap_user: None,
                smtp_user: None,
            }
            .into(),
            source: Source::Probe,
            verified: true,
        })
    }

    fn probe_imap(&mut self) -> Option<(String, u16, String)> {
        for host in self.imap_candidates() {
            for (port, security) in [(993u16, "implicit"), (143u16, "starttls")] {
                match verify_imap(&host, port, security) {
                    Ok(()) => {
                        self.note(
                            Source::Probe,
                            &format!("{host}:{port}"),
                            "an IMAP server answered over TLS",
                        );
                        return Some((host, port, security.to_string()));
                    }
                    Err(e) => self.note(Source::Probe, &format!("{host}:{port}"), short(&e)),
                }
            }
        }
        None
    }

    /// Where an IMAP server plausibly lives, most likely first.
    ///
    /// The `MX` host is included because a self-hosted domain usually runs one
    /// machine for both, and it is the only candidate here the domain actually
    /// published rather than a convention this guessed.
    fn imap_candidates(&mut self) -> Vec<String> {
        let mut hosts = vec![
            format!("imap.{}", self.domain),
            format!("mail.{}", self.domain),
        ];
        for exchange in self.mx_hosts() {
            if !hosts.contains(&exchange) {
                hosts.push(exchange);
            }
        }
        hosts.push(self.domain.clone());
        hosts
    }

    fn smtp_candidates(&self, imap_host: &str) -> Vec<String> {
        // The host that answered for IMAP, when it is one of the shared names,
        // is a better guess than `smtp.<domain>` — a machine called
        // `mail.example.com` running IMAP is running submission too.
        if imap_host.starts_with("mail.") || imap_host == self.domain {
            return vec![imap_host.to_string()];
        }
        vec![format!("smtp.{}", self.domain), imap_host.to_string()]
    }

    fn mx_hosts(&mut self) -> Vec<String> {
        let resolvers = self.resolvers();
        if resolvers.is_empty() {
            return Vec::new();
        }
        match crate::dns::mx(&self.domain, &resolvers) {
            Ok(records) if records.is_empty() => {
                self.note(Source::Probe, &format!("MX {}", self.domain), "no records");
                Vec::new()
            }
            Ok(records) => {
                let hosts: Vec<String> = records
                    .into_iter()
                    .map(|m| m.exchange.trim_end_matches('.').to_string())
                    .collect();
                self.note(
                    Source::Probe,
                    &format!("MX {}", self.domain),
                    hosts.join(", "),
                );
                hosts
            }
            Err(e) => {
                self.note(Source::Probe, &format!("MX {}", self.domain), short(&e));
                Vec::new()
            }
        }
    }
}

/// Connects, completes TLS, and reads the greeting. No credential is sent.
pub fn verify_imap(host: &str, port: u16, security: &str) -> Result<()> {
    let cfg = crate::imap::ConnectionConfig {
        host: host.to_string(),
        port,
        security: match security {
            "starttls" => crate::imap::Security::StartTls,
            "none" => crate::imap::Security::None,
            _ => crate::imap::Security::Implicit,
        },
        // Never relaxed for a probe: a discovery step that accepted plaintext
        // would be the one place §9's rule could be walked around.
        require_tls: true,
        timeout: PROBE_TIMEOUT,
    };
    let _client = crate::imap::Client::connect(&cfg, crate::Tracer::disabled())?;
    Ok(())
}

/// The first line of an error, for a step's outcome. A multi-line TLS failure
/// makes a mess of a list of steps.
fn short(e: &Error) -> String {
    let text = e.to_string();
    let first = text.lines().next().unwrap_or("").trim().to_string();
    if first.len() > 120 {
        format!("{}…", &first[..119])
    } else {
        first
    }
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Reads Thunderbird's `clientConfig` format, as served by a domain's own
/// autoconfig host and by the ISPDB.
fn parse_autoconfig(body: &[u8], address: &str) -> Option<ServerHints> {
    let doc = xml::parse(&String::from_utf8_lossy(body))?;

    let mut imap: Option<(String, u16, String, Option<String>)> = None;
    let mut smtp: Option<(String, u16, String, Option<String>)> = None;

    for node in doc.descendants("incomingServer") {
        if node
            .attr("type")
            .is_some_and(|t| t.eq_ignore_ascii_case("imap"))
        {
            imap = read_server(node, address, 993);
            if imap.is_some() {
                break;
            }
        }
    }
    // POP-only providers exist, and an IMAP client cannot use them. Saying so
    // is better than falling through to a guess that will not work either.
    let (imap_host, imap_port, imap_security, imap_user) = imap?;

    for node in doc.descendants("outgoingServer") {
        if node
            .attr("type")
            .is_none_or(|t| t.eq_ignore_ascii_case("smtp"))
        {
            smtp = read_server(node, address, 587);
            if smtp.is_some() {
                break;
            }
        }
    }
    let (smtp_host, smtp_port, smtp_security, smtp_user) =
        smtp.unwrap_or_else(|| (imap_host.clone(), 587, "starttls".to_string(), None));

    Some(ServerHints {
        imap_host,
        imap_port,
        imap_security,
        smtp_host,
        smtp_port,
        smtp_security,
        imap_user,
        smtp_user,
    })
}

fn read_server(
    node: &xml::Node,
    address: &str,
    default_port: u16,
) -> Option<(String, u16, String, Option<String>)> {
    let host = node.child_text("hostname")?.trim().to_string();
    if host.is_empty() {
        return None;
    }
    let socket = node.child_text("socketType").unwrap_or_default();
    let security = match socket.trim().to_ascii_uppercase().as_str() {
        "SSL" => "implicit",
        "STARTTLS" => "starttls",
        // `plain` is a real value in this format and §9 refuses it for a
        // non-loopback host. Reported as what it is rather than silently
        // upgraded to something the server may not speak.
        "PLAIN" | "NONE" => "none",
        _ => {
            // Some documents omit it; the port then says which it is.
            match node
                .child_text("port")
                .and_then(|p| p.trim().parse::<u16>().ok())
            {
                Some(993) | Some(465) => "implicit",
                _ => "starttls",
            }
        }
    };
    let port = node
        .child_text("port")
        .and_then(|p| p.trim().parse::<u16>().ok())
        .unwrap_or(default_port);
    let user = node
        .child_text("username")
        .and_then(|u| expand_user(&u, address));
    Some((host, port, security.to_string(), user))
}

/// Expands the format's placeholders. A username that *is* the address adds
/// nothing to the config, so it comes back as `None`.
fn expand_user(template: &str, address: &str) -> Option<String> {
    let local = address.split('@').next().unwrap_or(address);
    let expanded = template
        .trim()
        .replace("%EMAILADDRESS%", address)
        .replace("%EMAILLOCALPART%", local);
    if expanded.is_empty() || expanded == address || expanded.contains('%') {
        None
    } else {
        Some(expanded)
    }
}

/// Reads Microsoft's Autodiscover response.
fn parse_autodiscover(body: &[u8]) -> Option<ServerHints> {
    let doc = xml::parse(&String::from_utf8_lossy(body))?;

    let mut imap = None;
    let mut smtp = None;
    for protocol in doc.descendants("Protocol") {
        let kind = protocol.child_text("Type").unwrap_or_default();
        let Some(server) = protocol.child_text("Server") else {
            continue;
        };
        let server = server.trim().to_string();
        if server.is_empty() {
            continue;
        }
        let ssl = protocol
            .child_text("SSL")
            .is_none_or(|v| !v.trim().eq_ignore_ascii_case("off"));
        let port = protocol
            .child_text("Port")
            .and_then(|p| p.trim().parse::<u16>().ok());
        let encryption = protocol.child_text("Encryption").unwrap_or_default();
        let security = if encryption.trim().eq_ignore_ascii_case("TLS") {
            "starttls"
        } else if ssl {
            "implicit"
        } else {
            "none"
        };
        match kind.trim().to_ascii_uppercase().as_str() {
            "IMAP" if imap.is_none() => {
                imap = Some((server, port.unwrap_or(993), security.to_string()))
            }
            "SMTP" if smtp.is_none() => {
                smtp = Some((server, port.unwrap_or(587), security.to_string()))
            }
            _ => {}
        }
    }

    let (imap_host, imap_port, imap_security) = imap?;
    let (smtp_host, smtp_port, smtp_security) =
        smtp.unwrap_or_else(|| (imap_host.clone(), 587, "starttls".to_string()));
    Some(ServerHints {
        imap_host,
        imap_port,
        imap_security,
        smtp_host,
        smtp_port,
        smtp_security,
        imap_user: None,
        smtp_user: None,
    })
}

/// Just enough XML to read two well-known documents.
///
/// Hand-rolled for §4's reason again: both formats are a shallow tree of
/// elements with text in the leaves, and that is a hundred lines here against
/// a general parser's dependency tree. What it deliberately does *not* do is
/// resolve entities beyond the five built-in ones, expand a DTD, or fetch an
/// external one — the features behind every XML parser CVE, and a document
/// fetched from a stranger's domain is exactly the input they are reached by.
mod xml {
    /// One element: its name, its attributes, its text, and its children.
    #[derive(Debug, Default)]
    pub struct Node {
        pub name: String,
        attrs: Vec<(String, String)>,
        text: String,
        children: Vec<Node>,
    }

    impl Node {
        pub fn attr(&self, name: &str) -> Option<&str> {
            self.attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }

        /// The text of the first direct child called `name`.
        pub fn child_text(&self, name: &str) -> Option<String> {
            self.children
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(name))
                .map(|c| c.text.clone())
        }

        /// Every element called `name`, at any depth, in document order.
        pub fn descendants<'a>(&'a self, name: &'a str) -> Vec<&'a Node> {
            let mut out = Vec::new();
            self.collect(name, &mut out);
            out
        }

        fn collect<'a>(&'a self, name: &'a str, out: &mut Vec<&'a Node>) {
            for child in &self.children {
                if child.name.eq_ignore_ascii_case(name) {
                    out.push(child);
                }
                child.collect(name, out);
            }
        }
    }

    /// Parses `src` into a synthetic root holding the document's elements.
    ///
    /// Malformed input yields `None` rather than a partial tree: a truncated
    /// document read as a valid one is how a proxy's error page becomes a mail
    /// server's hostname.
    pub fn parse(src: &str) -> Option<Node> {
        let mut stack: Vec<Node> = vec![Node::default()];
        let bytes: Vec<char> = src.chars().collect();
        let mut i = 0;

        while i < bytes.len() {
            match bytes[i] {
                '<' => {
                    // Comments, declarations, and processing instructions carry
                    // nothing this reads, and skipping them is what keeps a
                    // `<!DOCTYPE …>` from being taken for an element.
                    if starts_with(&bytes, i, "<!--") {
                        i = find(&bytes, i + 4, "-->")? + 3;
                        continue;
                    }
                    if starts_with(&bytes, i, "<![CDATA[") {
                        let end = find(&bytes, i + 9, "]]>")?;
                        let text: String = bytes[i + 9..end].iter().collect();
                        stack.last_mut()?.text.push_str(&text);
                        i = end + 3;
                        continue;
                    }
                    if starts_with(&bytes, i, "<?") {
                        i = find(&bytes, i + 2, "?>")? + 2;
                        continue;
                    }
                    if starts_with(&bytes, i, "<!") {
                        i = find(&bytes, i + 2, ">")? + 1;
                        continue;
                    }
                    let end = find(&bytes, i + 1, ">")?;
                    let tag: String = bytes[i + 1..end].iter().collect();
                    i = end + 1;

                    if let Some(name) = tag.strip_prefix('/') {
                        let closed = stack.pop()?;
                        // A stray close tag means the document is not what it
                        // claims to be.
                        if !closed.name.eq_ignore_ascii_case(local_name(name.trim()))
                            || stack.is_empty()
                        {
                            return None;
                        }
                        stack.last_mut()?.children.push(closed);
                        continue;
                    }

                    let selfclosing = tag.ends_with('/');
                    let tag = tag.trim_end_matches('/');
                    let mut node = Node {
                        name: local_name(tag.split_whitespace().next()?).to_string(),
                        ..Node::default()
                    };
                    node.attrs = attributes(tag);
                    if selfclosing {
                        stack.last_mut()?.children.push(node);
                    } else {
                        stack.push(node);
                    }
                }
                _ => {
                    let end = bytes[i..]
                        .iter()
                        .position(|c| *c == '<')
                        .map(|p| i + p)
                        .unwrap_or(bytes.len());
                    let text: String = bytes[i..end].iter().collect();
                    let node = stack.last_mut()?;
                    node.text.push_str(&unescape(text.trim()));
                    i = end;
                }
            }
        }
        // Everything opened has to have closed, or the document was truncated.
        if stack.len() != 1 {
            return None;
        }
        stack.pop()
    }

    /// Drops a namespace prefix. These documents use one inconsistently, and
    /// nothing here needs to tell two namespaces apart.
    fn local_name(name: &str) -> &str {
        name.rsplit(':').next().unwrap_or(name)
    }

    fn attributes(tag: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut rest = match tag.find(char::is_whitespace) {
            Some(i) => &tag[i..],
            None => return out,
        };
        while let Some(eq) = rest.find('=') {
            let name = rest[..eq].trim().to_string();
            let after = rest[eq + 1..].trim_start();
            let quote = after.chars().next();
            let (value, consumed) = match quote {
                Some(q @ ('"' | '\'')) => match after[1..].find(q) {
                    Some(end) => (after[1..1 + end].to_string(), 2 + end),
                    None => return out,
                },
                _ => {
                    let end = after.find(char::is_whitespace).unwrap_or(after.len());
                    (after[..end].to_string(), end)
                }
            };
            if !name.is_empty() {
                out.push((local_name(&name).to_string(), unescape(&value)));
            }
            rest = &after[consumed..];
        }
        out
    }

    fn unescape(s: &str) -> String {
        if !s.contains('&') {
            return s.to_string();
        }
        s.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
    }

    fn starts_with(chars: &[char], at: usize, needle: &str) -> bool {
        needle
            .chars()
            .enumerate()
            .all(|(k, c)| chars.get(at + k) == Some(&c))
    }

    fn find(chars: &[char], from: usize, needle: &str) -> Option<usize> {
        (from..chars.len()).find(|&i| starts_with(chars, i, needle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTOCONFIG: &str = r#"<?xml version="1.0"?>
<clientConfig version="1.1">
  <emailProvider id="example.com">
    <domain>example.com</domain>
    <incomingServer type="pop3">
      <hostname>pop.example.com</hostname><port>995</port><socketType>SSL</socketType>
    </incomingServer>
    <incomingServer type="imap">
      <hostname>imap.example.com</hostname>
      <port>993</port>
      <socketType>SSL</socketType>
      <username>%EMAILADDRESS%</username>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>smtp.example.com</hostname>
      <port>587</port>
      <socketType>STARTTLS</socketType>
      <username>%EMAILLOCALPART%</username>
    </outgoingServer>
  </emailProvider>
</clientConfig>"#;

    #[test]
    fn reads_an_autoconfig_document() {
        let hints = parse_autoconfig(AUTOCONFIG.as_bytes(), "peter@example.com").expect("hints");
        assert_eq!(hints.imap_host, "imap.example.com");
        assert_eq!(hints.imap_port, 993);
        assert_eq!(hints.imap_security, "implicit");
        assert_eq!(hints.smtp_host, "smtp.example.com");
        assert_eq!(hints.smtp_security, "starttls");
        // `%EMAILADDRESS%` is the address, which the config already holds.
        assert_eq!(hints.imap_user, None);
        assert_eq!(hints.smtp_user.as_deref(), Some("peter"));
    }

    #[test]
    fn the_pop_server_is_not_mistaken_for_the_imap_one() {
        let hints = parse_autoconfig(AUTOCONFIG.as_bytes(), "peter@example.com").expect("hints");
        assert_ne!(hints.imap_host, "pop.example.com");
    }

    #[test]
    fn a_pop_only_provider_yields_nothing() {
        let doc = r#"<clientConfig><emailProvider id="p">
            <incomingServer type="pop3"><hostname>pop.p</hostname><port>995</port>
            <socketType>SSL</socketType></incomingServer></emailProvider></clientConfig>"#;
        assert!(parse_autoconfig(doc.as_bytes(), "a@p").is_none());
    }

    #[test]
    fn a_truncated_document_is_refused_rather_than_half_read() {
        let cut = &AUTOCONFIG[..AUTOCONFIG.len() / 2];
        assert!(parse_autoconfig(cut.as_bytes(), "peter@example.com").is_none());
    }

    #[test]
    fn an_error_page_is_not_a_mail_server() {
        let html = b"<html><head><title>502 Bad Gateway</title></head><body>no</body></html>";
        assert!(parse_autoconfig(html, "peter@example.com").is_none());
    }

    #[test]
    fn a_missing_socket_type_is_read_from_the_port() {
        let doc = r#"<clientConfig><emailProvider id="p">
            <incomingServer type="imap"><hostname>imap.p</hostname><port>993</port>
            </incomingServer></emailProvider></clientConfig>"#;
        let hints = parse_autoconfig(doc.as_bytes(), "a@p").expect("hints");
        assert_eq!(hints.imap_security, "implicit");
    }

    #[test]
    fn namespaced_elements_are_read_the_same() {
        let doc = r#"<x:clientConfig xmlns:x="urn:example"><x:emailProvider id="p">
            <x:incomingServer type="imap"><x:hostname>imap.p</x:hostname>
            <x:port>143</x:port><x:socketType>STARTTLS</x:socketType>
            </x:incomingServer></x:emailProvider></x:clientConfig>"#;
        let hints = parse_autoconfig(doc.as_bytes(), "a@p").expect("hints");
        assert_eq!(hints.imap_host, "imap.p");
        assert_eq!(hints.imap_security, "starttls");
    }

    #[test]
    fn reads_an_autodiscover_response() {
        let doc = r#"<?xml version="1.0"?>
        <Autodiscover xmlns="http://schemas.microsoft.com/exchange/autodiscover/responseschema/2006">
          <Response>
            <Account>
              <Protocol><Type>SMTP</Type><Server>smtp.corp.example</Server>
                <Port>587</Port><SSL>on</SSL><Encryption>TLS</Encryption></Protocol>
              <Protocol><Type>IMAP</Type><Server>imap.corp.example</Server>
                <Port>993</Port><SSL>on</SSL></Protocol>
            </Account>
          </Response>
        </Autodiscover>"#;
        let hints = parse_autodiscover(doc.as_bytes()).expect("hints");
        assert_eq!(hints.imap_host, "imap.corp.example");
        assert_eq!(hints.imap_security, "implicit");
        assert_eq!(hints.smtp_host, "smtp.corp.example");
        // `Encryption: TLS` on the submission port means STARTTLS.
        assert_eq!(hints.smtp_security, "starttls");
    }

    #[test]
    fn a_cdata_hostname_is_read() {
        let doc = r#"<clientConfig><emailProvider id="p"><incomingServer type="imap">
            <hostname><![CDATA[imap.p]]></hostname><port>993</port>
            <socketType>SSL</socketType></incomingServer></emailProvider></clientConfig>"#;
        let hints = parse_autoconfig(doc.as_bytes(), "a@p").expect("hints");
        assert_eq!(hints.imap_host, "imap.p");
    }

    #[test]
    fn a_doctype_is_skipped_rather_than_parsed() {
        let doc = r#"<!DOCTYPE clientConfig SYSTEM "http://example.invalid/evil.dtd">
            <clientConfig><emailProvider id="p"><incomingServer type="imap">
            <hostname>imap.p</hostname><port>993</port><socketType>SSL</socketType>
            </incomingServer></emailProvider></clientConfig>"#;
        let hints = parse_autoconfig(doc.as_bytes(), "a@p").expect("hints");
        assert_eq!(hints.imap_host, "imap.p");
    }

    #[test]
    fn the_table_answers_without_touching_the_network() {
        let found = discover("peter@gmail.com", &Options::offline())
            .expect("an address")
            .found
            .expect("gmail is known");
        assert_eq!(found.source, Source::BuiltIn);
        assert_eq!(found.hints.imap_host, "imap.gmail.com");
    }

    #[test]
    fn an_unknown_domain_offline_finds_nothing_and_says_what_it_tried() {
        let discovery = discover("peter@example.com", &Options::offline()).expect("an address");
        assert!(discovery.found.is_none());
        assert_eq!(discovery.domain, "example.com");
        assert!(discovery
            .steps
            .iter()
            .any(|s| s.source == Source::BuiltIn && s.outcome.contains("not in the table")));
    }

    #[test]
    fn something_that_is_not_an_address_is_an_error() {
        assert!(discover("peter", &Options::offline()).is_err());
        assert!(discover("peter@localhost", &Options::offline()).is_err());
    }

    #[test]
    fn the_probe_will_not_offer_a_guess_it_could_not_verify() {
        let opts = Options {
            network: true,
            verify: false,
            // A resolver that will not be reached, because verification is off
            // and the probe stops before asking.
            resolvers: vec!["192.0.2.53:53".parse().expect("an address")],
        };
        let mut run = Run {
            address: "peter@example.com".into(),
            domain: "example.com".into(),
            opts: &opts,
            steps: Vec::new(),
        };
        assert!(run.probe().is_none());
        assert!(run.steps.iter().any(|s| s.outcome.contains("skipped")));
    }
}
