//! Manual browser end-to-end fixture for the hosted site.

use mailbox_l3::{ImapServer, Message};

fn main() -> std::io::Result<()> {
    let server = ImapServer::start(vec![Message::simple(
        1,
        "Hosted end-to-end",
        "hosted-e2e@example.com",
    )])?;
    println!("127.0.0.1:{}", server.port());
    std::thread::park();
    drop(server);
    Ok(())
}
