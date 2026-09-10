//! `mailbox pair` (§8): moving accounts and credentials to another device.
//!
//! The shape of the flow is §8.3's:
//!
//! ```text
//! new device:   $ mailbox pair
//!               → scan: ▓▓▒▒…   (or paste this link)
//! old device:   $ mailbox pair send mailbox://pair/…#…
//!               → 3 accounts, 2 identities → confirm words: amber-tiger-04-mint
//! new device:   → confirm words: amber-tiger-04-mint  [y/N]
//!               → paired.
//! ```
//!
//! The confirmation is not a formality. §8.1 makes the short authentication
//! string the thing that defends against an active attacker, so the receiving
//! device asks before it writes anything, and a "no" leaves the config exactly
//! as it was.

use std::io::{IsTerminal, Write};

use mailbox_core::config::Config;
use mailbox_core::error::{Error, Result};
use mailbox_core::pair::{crypto, file, transport, ApplyMode, PairingUri, Payload};

use crate::cli::{PairAction, PairArgs};
use crate::commands::Ctx;
use crate::output::{GREEN, YELLOW};

pub fn pair(ctx: &Ctx<'_>, args: &PairArgs) -> Result<()> {
    // clap cannot express a conflict against a subcommand, so the combinations
    // are checked here — and named, rather than reported as a generic usage
    // error the caller has to work out.
    let modes = [
        args.export_file.is_some().then_some("--export-file"),
        args.import_file.is_some().then_some("--import-file"),
        args.action.is_some().then_some("send"),
    ];
    let chosen: Vec<&str> = modes.into_iter().flatten().collect();
    if chosen.len() > 1 {
        return Err(Error::usage(format!(
            "{} do different things; pick one",
            chosen.join(" and ")
        )));
    }

    if let Some(path) = &args.export_file {
        return export(ctx, path, args);
    }
    if let Some(path) = &args.import_file {
        return import(ctx, path, args);
    }
    match &args.action {
        Some(PairAction::Send { link, yes }) => match relay_of(ctx, args) {
            Some(relay) => relay_send(ctx, args, &relay, link, *yes),
            None => send(ctx, link, *yes),
        },
        None => match relay_of(ctx, args) {
            Some(relay) => relay_receive(ctx, args, &relay),
            None => receive(ctx, args),
        },
    }
}

/// The relay to use, if this run wants one: `--relay`, else `[pair] relay`
/// from the config (§12).
fn relay_of(ctx: &Ctx<'_>, args: &PairArgs) -> Option<mailbox_core::pair::relay::Relay> {
    args.relay
        .clone()
        .or_else(|| load_config(ctx).ok().and_then(|c| c.pair.relay.clone()))
        .filter(|url| !url.is_empty())
        .map(|url| mailbox_core::pair::relay::Relay::new(&url))
}

/// The receiving device, over a relay (§8.2).
///
/// The same shape as [`receive`] — show something, wait, confirm the words,
/// apply — with a rendezvous instead of a socket. The only real difference is
/// where the secret comes from: not generated here and carried in a link, but
/// *agreed* with the other device through the PAKE, so the thing the user
/// reads out can be short enough to say aloud.
fn relay_receive(
    ctx: &Ctx<'_>,
    args: &PairArgs,
    relay: &mailbox_core::pair::relay::Relay,
) -> Result<()> {
    use mailbox_core::pair::pake;
    use mailbox_core::pair::relay::{Box_, Code};

    let mode = apply_mode(ctx, args)?;
    let slot = relay.allocate()?;
    let secret_code = pake::generate_code(&mailbox_core::Rng::from_env());
    let code = Code {
        slot: slot.clone(),
        secret: pake::normalize_code(&secret_code)?,
    };
    let rendered = code.render();

    if ctx.out.is_structured() {
        ctx.out.reply(
            "pair",
            &serde_json::json!({
                "code": rendered,
                "relay": relay.base(),
                "expires_in_seconds": mailbox_core::pair::relay::TTL_SECONDS,
            }),
        )?;
    } else {
        ctx.out.print(format!(
            "On the other device run:\n\n  {} {}\n",
            ctx.out.bold("mailbox pair send"),
            rendered
        ));
        ctx.out.note("waiting — Ctrl-C to stop");
    }

    // The other device speaks first: it has the accounts, so it starts the
    // handshake and later fills the payload box.
    let offer_bytes = relay.take(&slot, Box_::Offer, &ctx.clock)?;
    let offer: [u8; pake::OFFER_BYTES] = offer_bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error::protocol("the other device sent a malformed handshake"))?;

    let (established, answer) = pake::respond(&code.secret, &offer)?;
    relay.put(&slot, Box_::Answer, &answer)?;

    let script = pake::transcript(&offer, &answer);
    let sas = crypto::short_authentication_string(established.secret(), &script);

    let sealed = relay.take(&slot, Box_::Payload, &ctx.clock)?;

    // §8.1: the words first, then anything is written. A wrong code reaches
    // exactly here and no further — the words differ, and the payload will not
    // open either.
    if !confirm_sas(ctx, &sas, "the sending device")? {
        return Err(Error::usage(
            "pairing refused — the words did not match, which means something is between \
             the two devices",
        ));
    }

    let payload = Payload::open(established.secret(), &sealed, ctx.clock.now())?;
    apply(ctx, &payload, mode)
}

/// The sending device, over a relay (§8.2).
fn relay_send(
    ctx: &Ctx<'_>,
    _args: &PairArgs,
    relay: &mailbox_core::pair::relay::Relay,
    typed: &str,
    skip_confirmation: bool,
) -> Result<()> {
    use mailbox_core::pair::pake;
    use mailbox_core::pair::relay::{Box_, Code};

    let code = Code::parse(typed)?;
    let config = load_config(ctx)?;
    if config.accounts.is_empty() {
        return Err(Error::usage("this device has no accounts to send"));
    }

    let (pending, offer) = pake::begin(&code.secret)?;
    relay.put(&code.slot, Box_::Offer, &offer)?;

    let answer_bytes = relay.take(&code.slot, Box_::Answer, &ctx.clock)?;
    let answer: [u8; pake::ANSWER_BYTES] = answer_bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error::protocol("the other device sent a malformed handshake"))?;
    let established = pending.finish(&answer)?;

    let script = pake::transcript(&offer, &answer);
    let sas = crypto::short_authentication_string(established.secret(), &script);

    let rng = mailbox_core::Rng::from_env();
    let payload = Payload::build(
        &config,
        &mailbox_core::paths::device_name(),
        &ctx.clock,
        &rng,
    );
    let summary = payload.describe();
    let missing: Vec<&str> = payload
        .accounts
        .iter()
        .filter(|a| a.credential.is_none())
        .map(|a| a.id.as_str())
        .collect();
    if !missing.is_empty() && !ctx.out.is_structured() {
        ctx.out.warn(format!(
            "no credential could be read for: {}. Those accounts will need a password on \
             the new device.",
            missing.join(", ")
        ));
    }

    let sealed = payload.seal(established.secret(), &rng)?;
    relay.put(&code.slot, Box_::Payload, &sealed)?;

    if ctx.out.is_structured() {
        ctx.out.reply(
            "pair",
            &serde_json::json!({
                "sent": payload.accounts.len(),
                "relay": relay.base(),
                "words": sas,
            }),
        )?;
    } else {
        ctx.out.print(format!("  {summary}"));
        ctx.out
            .print(format!("  confirm words: {}", ctx.out.paint(&sas, GREEN)));
        if !skip_confirmation {
            ctx.out
                .note("check these match on the other device before accepting there");
        }
        // Unlike the LAN transport, there is no answer coming back: the relay
        // carries three messages one way each and has no channel for an
        // acknowledgement. Saying so is better than implying a success this
        // device cannot observe.
        ctx.out.note(
            "sent — the other device decides whether to accept, and this one will not hear which",
        );
    }
    Ok(())
}

/// The receiving device: generate a secret, show it, wait (§8.3).
fn receive(ctx: &Ctx<'_>, args: &PairArgs) -> Result<()> {
    let mode = apply_mode(ctx, args)?;
    let secret = crypto::generate_secret(&mailbox_core::Rng::from_env());

    let listener = transport::Listener::bind()?;
    let uri = PairingUri {
        secret,
        address: Some(listener.advertised_address()),
    };
    let link = uri.render();

    if ctx.out.is_structured() {
        // An agent driving this needs the link before the blocking wait, so it
        // goes out first and on its own line.
        ctx.out.reply(
            "pair",
            &serde_json::json!({
                "link": link,
                "waiting_on": listener.advertised_address(),
                "expires_in_seconds": crypto::FRESHNESS_SECONDS,
            }),
        )?;
    } else {
        if !args.no_qr {
            ctx.out.print(crate::qr::render(&link));
        }
        ctx.out.print(format!(
            "On the other device run:\n\n  {} {}\n",
            ctx.out.bold("mailbox pair send"),
            link
        ));
        ctx.out.note("waiting — Ctrl-C to stop");
    }

    let incoming = listener.accept(&secret)?;

    // §8.1: confirm before anything is written.
    if !confirm_sas(ctx, &incoming.sas, "the sending device")? {
        incoming.respond(false, "the words did not match")?;
        return Err(Error::usage(
            "pairing refused — the words did not match, which means something is between the two devices",
        ));
    }

    let payload = match Payload::open(&secret, &incoming.sealed, ctx.clock.now()) {
        Ok(payload) => payload,
        Err(e) => {
            incoming.respond(false, &e.to_string())?;
            return Err(e);
        }
    };
    incoming.respond(true, "")?;

    apply(ctx, &payload, mode)
}

/// The sending device: seal this device's accounts and offer them (§8.3).
fn send(ctx: &Ctx<'_>, link: &str, skip_confirmation: bool) -> Result<()> {
    let uri = PairingUri::parse(link)?;
    let address = uri.address.clone().ok_or_else(|| {
        Error::usage(
            "that link carries no address, so there is nowhere to send to — \
             use --export-file for an air-gapped move",
        )
    })?;

    let config = load_config(ctx)?;
    if config.accounts.is_empty() {
        return Err(Error::usage("this device has no accounts to send"));
    }

    let rng = mailbox_core::Rng::from_env();
    let payload = Payload::build(
        &config,
        &mailbox_core::paths::device_name(),
        &ctx.clock,
        &rng,
    );
    let missing: Vec<&str> = payload
        .accounts
        .iter()
        .filter(|a| a.credential.is_none())
        .map(|a| a.id.as_str())
        .collect();
    if !missing.is_empty() && !ctx.out.is_structured() {
        // Said before sending, not after: the user can stop and fix it.
        ctx.out.warn(format!(
            "no credential could be read for: {}. Those accounts will need a password on the new device.",
            missing.join(", ")
        ));
    }

    let sealed = payload.seal(&uri.secret, &rng)?;
    let sent = transport::send(&address, &uri.secret, &sealed)?;

    if !ctx.out.is_structured() {
        ctx.out.print(format!("  {}", payload.describe()));
        ctx.out.print(format!(
            "  confirm words: {}",
            ctx.out.paint(&sent.sas, GREEN)
        ));
        if !skip_confirmation {
            ctx.out
                .note("check these match on the other device before accepting there");
        }
    }

    if !sent.accepted {
        return Err(Error::usage(format!(
            "the other device refused: {}",
            if sent.reason.is_empty() {
                "no reason given".to_string()
            } else {
                sent.reason
            }
        )));
    }

    if ctx.out.is_structured() {
        ctx.out.reply(
            "pair send",
            &serde_json::json!({
                "sas": sent.sas,
                "accepted": true,
                "accounts": payload.accounts.len(),
                "credentials_missing": missing,
            }),
        )?;
    } else {
        ctx.out
            .print(format!("{} paired", ctx.out.paint("✓", GREEN)));
    }
    Ok(())
}

/// Whether a `--export-file`/`--import-file` path means the standard streams.
///
/// `-` is the convention every other tool uses, and it is what makes the
/// desktop-to-desktop case a pipe rather than a file: two machines that can
/// already reach each other over SSH have an authenticated channel, and
/// writing every credential the user owns to a file on both ends — to move it
/// between two machines that were already talking — is a worse answer than
/// never letting it touch a disk at all.
fn is_stdio(path: &std::path::Path) -> bool {
    path.as_os_str() == "-"
}

fn export(ctx: &Ctx<'_>, path: &std::path::Path, args: &PairArgs) -> Result<()> {
    let config = load_config(ctx)?;
    if config.accounts.is_empty() {
        return Err(Error::usage("this device has no accounts to export"));
    }
    // The payload owns stdout when it is going there, so there is nowhere for
    // a JSON document to go that would not corrupt one or the other.
    if is_stdio(path) && ctx.out.is_structured() {
        return Err(Error::usage(
            "`--export-file -` writes the payload to stdout, so it cannot also carry --json; \
             export to a file, or drop --json",
        ));
    }
    // A sealed payload is binary. Spraying it across somebody's terminal is
    // never what they meant, and the mistake is easy to make.
    if is_stdio(path) && std::io::stdout().is_terminal() {
        return Err(Error::usage(
            "`--export-file -` writes binary to stdout; pipe it somewhere, such as \
             `mailbox pair --export-file - | ssh other-host mailbox pair --import-file -`",
        ));
    }
    let passphrase = passphrase(ctx, args, true)?;

    let rng = mailbox_core::Rng::from_env();
    let payload = Payload::build(
        &config,
        &mailbox_core::paths::device_name(),
        &ctx.clock,
        &rng,
    );
    let bytes = file::export(&payload, &passphrase, &rng)?;

    if is_stdio(path) {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&bytes)?;
        stdout.flush()?;
        // Everything else goes to stderr: stdout is the payload.
        ctx.out.note(format!(
            "{} → stdout ({} bytes)",
            payload.describe(),
            bytes.len()
        ));
        return Ok(());
    }
    mailbox_core::paths::write_private(path, &bytes)?;

    let payload_summary = payload.describe();
    if ctx.out.is_structured() {
        ctx.out.reply(
            "pair",
            &serde_json::json!({
                "exported_to": path.display().to_string(),
                "accounts": payload.accounts.len(),
                "bytes": bytes.len(),
            }),
        )?;
    } else {
        ctx.out.print(format!(
            "{} {} → {}",
            ctx.out.paint("✓", GREEN),
            payload_summary,
            path.display()
        ));
        ctx.out
            .note("this file contains every credential — treat it like a password");
    }
    Ok(())
}

fn import(ctx: &Ctx<'_>, path: &std::path::Path, args: &PairArgs) -> Result<()> {
    let mode = apply_mode(ctx, args)?;
    let bytes = if is_stdio(path) {
        use std::io::Read;
        let mut buf = Vec::new();
        std::io::stdin().lock().read_to_end(&mut buf)?;
        buf
    } else {
        std::fs::read(path).map_err(|e| Error::usage(format!("{}: {e}", path.display())))?
    };
    // Reading the payload from stdin means the passphrase cannot also come
    // from there; `passphrase` already refuses to prompt without a terminal
    // and names the two flags that work.
    let passphrase = passphrase(ctx, args, false)?;

    let payload = file::import(&bytes, &passphrase, ctx.clock.now())?;
    let age = file::age_days(&payload, ctx.clock.now());
    if age > 0 && !ctx.out.is_structured() {
        // An export is deliberately kept, so age is information rather than an
        // error — but a year-old file may hold credentials that have rotated.
        ctx.out.note(format!("this export is {age} days old"));
    }
    apply(ctx, &payload, mode)
}

/// Writes the config, and reports what changed (§8.3).
fn apply(ctx: &Ctx<'_>, payload: &Payload, mode: ApplyMode) -> Result<()> {
    let existing = load_config(ctx).unwrap_or_default();
    let (config, applied) = payload.apply(&existing, mode);

    let path = ctx
        .global
        .config
        .clone()
        .unwrap_or_else(mailbox_core::paths::config_file);

    if ctx.global.dry_run {
        if ctx.out.is_structured() {
            ctx.out.reply(
                "pair",
                &serde_json::json!({
                    "dry_run": true,
                    "would_add": applied.added,
                    "would_update": applied.updated,
                    "would_remove": applied.removed,
                }),
            )?;
        } else {
            ctx.out.print(format!(
                "would add {:?}, update {:?}, remove {:?} in {}",
                applied.added,
                applied.updated,
                applied.removed,
                path.display()
            ));
        }
        return Ok(());
    }

    let text = toml::to_string_pretty(&config)
        .map_err(|e| Error::usage(format!("could not write the config: {e}")))?;
    mailbox_core::paths::write_private(&path, text.as_bytes())?;

    // §9: credentials never land in the config file. They come out of the
    // payload and go to the keychain where one exists; where none does, the
    // user is told exactly how to supply them rather than left guessing.
    let mut stored = Vec::new();
    let mut manual = Vec::new();
    for account in &payload.accounts {
        match &account.credential {
            Some(mailbox_core::pair::PairedCredential::Password { secret }) => {
                // The core's writer, not a second one here: it knows what
                // §9's lookup will read back on each platform, and one of the
                // two copies of that knowledge was already wrong.
                let cfg = config
                    .accounts
                    .get(&account.id)
                    .cloned()
                    .unwrap_or_default();
                match mailbox_core::credentials::store_password(&account.id, &cfg, secret) {
                    mailbox_core::credentials::Stored::Keychain => stored.push(account.id.clone()),
                    mailbox_core::credentials::Stored::Process => manual.push(account.id.clone()),
                }
            }
            Some(mailbox_core::pair::PairedCredential::OAuth2 { refresh_token }) => {
                // §8: "no re-doing OAuth". The refresh token is what makes
                // that true, and the first command on this device trades it
                // for an access token.
                match mailbox_core::credentials::adopt_refresh_token(&account.id, refresh_token) {
                    Ok(_) => stored.push(account.id.clone()),
                    Err(_) => manual.push(account.id.clone()),
                }
            }
            None => manual.push(account.id.clone()),
        }
    }

    // §5.4's key, alongside the credentials and for the same reason: it is a
    // secret, so it goes where secrets go and not into the config. Without it
    // this device would sync the state folder, fail to open every record in
    // it, and quietly show none of the tags §8 promised would follow.
    let mut state_key = "not carried";
    if let Some(hex) = &payload.state_key {
        match mailbox_core::hex::decode(hex) {
            Some(secret) => {
                state_key = match mailbox_core::credentials::adopt_state_secret(&secret) {
                    Ok(mailbox_core::credentials::Stored::Keychain) => "the OS keychain",
                    Ok(_) => "a private file",
                    Err(_) => "could not be stored",
                };
            }
            None => state_key = "could not be read",
        }
    }

    if ctx.out.is_structured() {
        ctx.out.reply(
            "pair",
            &serde_json::json!({
                "config": path.display().to_string(),
                "added": applied.added,
                "updated": applied.updated,
                "removed": applied.removed,
                "credentials_stored": stored,
                "credentials_needing_attention": manual,
                "state_key": state_key,
            }),
        )?;
        return Ok(());
    }

    ctx.out.print(format!(
        "{} {} account(s) added, {} updated → {}",
        ctx.out.paint("✓", GREEN),
        applied.added.len(),
        applied.updated.len(),
        path.display()
    ));
    if !stored.is_empty() {
        ctx.out
            .print(format!("  credentials stored for: {}", stored.join(", ")));
    }
    for id in &manual {
        ctx.out.print(format!(
            "  {} {id}: set {}=… , or a credential_command",
            ctx.out.paint("!", YELLOW),
            Payload::credential_env_name(id),
        ));
    }
    ctx.out.note("run `mailbox sync` to fetch mail");
    Ok(())
}

/// §8.3: `--replace` wipes the target's accounts and requires `--yes`.
fn apply_mode(ctx: &Ctx<'_>, args: &PairArgs) -> Result<ApplyMode> {
    if !args.replace {
        return Ok(ApplyMode::Merge);
    }
    if !ctx.global.yes {
        return Err(Error::usage(
            "--replace deletes this device's accounts; pass --yes to confirm",
        )
        .needs_confirmation("pass --yes"));
    }
    Ok(ApplyMode::Replace)
}

/// Shows the words and asks (§8.1, §8.3).
fn confirm_sas(ctx: &Ctx<'_>, sas: &str, other: &str) -> Result<bool> {
    if ctx.global.yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        // §6.4: no interactive prompts when stdin is not a TTY. This one really
        // would have prompted, so it fails rather than assuming yes — assuming
        // yes here is assuming there is no attacker.
        return Err(Error::usage(format!(
            "pairing needs the words `{sas}` confirmed against {other}"
        ))
        .needs_confirmation("pass --yes once you have compared them"));
    }

    eprintln!();
    eprintln!("  confirm words: {}", ctx.out.paint(sas, GREEN));
    eprint!("  do these match {other}? [y/N] ");
    let _ = std::io::stderr().flush();

    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

fn passphrase(ctx: &Ctx<'_>, args: &PairArgs, confirm: bool) -> Result<String> {
    if let Some(p) = &args.passphrase {
        return Ok(p.clone());
    }
    if let Ok(p) = std::env::var("MAILBOX_PAIR_PASSPHRASE") {
        if !p.is_empty() {
            return Ok(p);
        }
    }
    if !std::io::stdin().is_terminal() {
        return Err(Error::usage(
            "an encrypted export needs a passphrase; pass --passphrase or set MAILBOX_PAIR_PASSPHRASE",
        )
        .needs_confirmation("pass --passphrase"));
    }

    // No terminal echo control here, so the passphrase is visible as it is
    // typed. Said plainly rather than pretended otherwise.
    ctx.out
        .warn("the passphrase will be visible as you type it");
    eprint!("  passphrase: ");
    let _ = std::io::stderr().flush();
    let mut first = String::new();
    std::io::stdin().read_line(&mut first)?;
    let first = first.trim().to_string();

    if confirm {
        eprint!("  again: ");
        let _ = std::io::stderr().flush();
        let mut again = String::new();
        std::io::stdin().read_line(&mut again)?;
        if again.trim() != first {
            return Err(Error::usage("the passphrases did not match"));
        }
    }
    Ok(first)
}

fn load_config(ctx: &Ctx<'_>) -> Result<Config> {
    match &ctx.global.config {
        Some(path) => Config::load(path),
        None => Config::load_default(),
    }
}
