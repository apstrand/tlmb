//! Manual browser end-to-end fixture for the hosted site.

use mailbox_l3::{ImapServer, Message};

fn main() -> std::io::Result<()> {
    // An optional count exercises newest-first browser backfill in batches.
    let count: u32 = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "1".into())
        .parse()
        .expect("expected a message count");
    let server = ImapServer::start(
        (1..=count)
            .map(|uid| {
                Message::simple(
                    uid,
                    &format!("Hosted end-to-end {uid}"),
                    &format!("hosted-e2e-{uid}@example.com"),
                )
            })
            .collect(),
    )?;
    println!("127.0.0.1:{}", server.port());
    std::thread::park();
    drop(server);
    Ok(())
}
