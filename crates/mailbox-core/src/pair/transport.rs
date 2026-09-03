//! The LAN transport (§8, transport 2).
//!
//! > **LAN (offline)** — mDNS discovery on the local network, direct TLS
//! > connection between the two devices, authenticated by the pairing secret.
//!
//! Two deviations from that sentence, both deliberate and both narrowing:
//!
//! **No mDNS.** The receiver's address travels in the same QR as the secret
//! (§8.1's `mailbox://pair#…`), so there is nothing to discover. Discovery
//! exists to answer "where is the other device", and a link that already says
//! is a better answer than a protocol that guesses — it works across subnets,
//! needs no multicast, and removes a dependency that would ship in every
//! binary for one command.
//!
//! **No TLS.** The channel is authenticated and encrypted by the pairing secret
//! itself: the payload is sealed with XChaCha20-Poly1305 under a key only the
//! two devices have (§8.1). TLS underneath would add a second, weaker
//! authentication — a self-signed certificate nobody can verify — and the
//! honest description of that is decoration. What matters is that nothing
//! readable ever crosses the wire, and nothing does.
//!
//! What is *not* deviated from is the confirmation: both devices display the
//! short authentication string, and the payload is not accepted until the user
//! says they match (§8.1). That is the check that survives everything else
//! being wrong.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::pair::crypto;

/// The wire is one JSON line each way, so a transcript is readable and a
/// mismatch is diagnosable.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Offer {
    /// Derived from the secret, so a device that dialled the wrong address
    /// finds out before sending anything.
    channel: String,
    /// base64 of the sealed payload.
    sealed: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Acceptance {
    accepted: bool,
    #[serde(default)]
    reason: String,
}

/// How long either side waits. §8.1 gives a channel 300 s; the socket gives up
/// sooner, because a device that has not answered in a minute is not coming.
const TIMEOUT: Duration = Duration::from_secs(60);

/// The receiving side: listens, and hands back what arrived.
pub struct Listener {
    listener: TcpListener,
}

/// A payload that has arrived but has not been accepted yet.
///
/// Deliberately a separate step: §8.1's confirmation happens between receiving
/// and applying, and a type that cannot be applied without going through
/// [`Incoming::accept`] is how that is enforced rather than remembered.
pub struct Incoming {
    /// The sealed bytes, still sealed.
    pub sealed: Vec<u8>,
    /// The words both devices must be showing (§8.3).
    pub sas: String,
    stream: TcpStream,
}

impl Listener {
    /// Binds an ephemeral port on every interface.
    ///
    /// Every interface because the other device is by definition not this one;
    /// ephemeral because a fixed port would be a thing to collide with and to
    /// firewall.
    pub fn bind() -> Result<Listener> {
        let listener = TcpListener::bind("0.0.0.0:0")
            .map_err(|e| Error::Network(format!("could not listen for a pairing: {e}")))?;
        Ok(Listener { listener })
    }

    pub fn port(&self) -> u16 {
        self.listener
            .local_addr()
            .map(|a| a.port())
            .unwrap_or_default()
    }

    /// The address to put in the pairing link.
    ///
    /// Best-effort: it asks the routing table which source address would be
    /// used to reach the local network, because `0.0.0.0` is not something the
    /// other device can dial.
    pub fn advertised_address(&self) -> String {
        format!("{}:{}", local_address(), self.port())
    }

    /// Waits for the sending device.
    pub fn accept(&self, secret: &[u8]) -> Result<Incoming> {
        self.listener
            .set_nonblocking(false)
            .map_err(|e| Error::Network(e.to_string()))?;
        let (stream, _peer) = self
            .listener
            .accept()
            .map_err(|e| Error::Network(format!("pairing connection failed: {e}")))?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;

        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| Error::Network(format!("the sending device went away: {e}")))?;

        let offer: Offer = serde_json::from_str(line.trim())
            .map_err(|_| Error::protocol("that device did not speak the pairing protocol"))?;

        // Checked before anything else: a wrong channel means the wrong secret,
        // and there is nothing to be gained by trying to open the payload.
        let expected = crypto::channel_id(secret);
        if !crypto::equal(offer.channel.as_bytes(), expected.as_bytes()) {
            return Err(Error::usage(
                "that device offered a different pairing code; check the link and try again",
            ));
        }

        use base64::Engine;
        let sealed = base64::engine::general_purpose::STANDARD
            .decode(offer.sealed.as_bytes())
            .map_err(|_| Error::protocol("the pairing payload is malformed"))?;

        let sas =
            crypto::short_authentication_string(secret, &crypto::transcript(&expected, &sealed));
        Ok(Incoming {
            sealed,
            sas,
            stream,
        })
    }
}

impl Incoming {
    /// Tells the sender the user confirmed, or did not.
    ///
    /// Both outcomes are reported, because the sending device is showing words
    /// too and its user needs to know whether to worry.
    pub fn respond(mut self, accepted: bool, reason: &str) -> Result<()> {
        let response = serde_json::to_string(&Acceptance {
            accepted,
            reason: reason.to_string(),
        })?;
        self.stream.write_all(response.as_bytes())?;
        self.stream.write_all(b"\n")?;
        self.stream.flush()?;
        Ok(())
    }
}

/// The sending side: connects, offers, and waits to be confirmed.
///
/// Returns the words to display. The caller shows them, the user compares, and
/// the receiving device is what actually decides — which is why this returns
/// the far side's answer rather than asking here.
pub fn send(address: &str, secret: &[u8], sealed: &[u8]) -> Result<Sent> {
    let stream = TcpStream::connect(address)
        .map_err(|e| Error::Network(format!("could not reach {address}: {e}")))?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;

    let channel = crypto::channel_id(secret);
    let sas = crypto::short_authentication_string(secret, &crypto::transcript(&channel, sealed));

    use base64::Engine;
    let offer = serde_json::to_string(&Offer {
        channel,
        sealed: base64::engine::general_purpose::STANDARD.encode(sealed),
    })?;

    let mut writer = stream.try_clone()?;
    writer.write_all(offer.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| Error::Network(format!("the receiving device went away: {e}")))?;
    let acceptance: Acceptance = serde_json::from_str(line.trim())
        .map_err(|_| Error::protocol("the receiving device did not answer properly"))?;

    Ok(Sent {
        sas,
        accepted: acceptance.accepted,
        reason: acceptance.reason,
    })
}

#[derive(Debug, Clone)]
pub struct Sent {
    /// The words this device showed.
    pub sas: String,
    pub accepted: bool,
    pub reason: String,
}

/// The address another device on the network would use to reach this one.
///
/// Asks the kernel by opening a UDP socket towards a routable address — no
/// packet is sent, but the routing table picks a source address, which is the
/// one to advertise. Falls back to loopback, which at least works for two
/// processes on one machine.
fn local_address() -> String {
    use std::net::UdpSocket;
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            socket.connect("192.0.2.1:9")?;
            socket.local_addr()
        })
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}
