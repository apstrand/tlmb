// Loads the shared UI and hands it the token.
//
// This file is a loader, not an interface. §3 says the web UI is the same one
// the desktop renders, compiled to wasm — so there is deliberately no second
// implementation of the mail client here in JavaScript to drift from the first.

const fallback = (reason) => {
    document.getElementById("mailbox").hidden = true;
    document.getElementById("fallback-reason").textContent = reason;
    document.getElementById("fallback").hidden = false;
};

if (!window.MAILBOX.hasWasm) {
    fallback(
        "This build has no web UI compiled into it. Build one with `cargo xtask web`, " +
        "then start `mailbox serve` again. The API on /api is running either way."
    );
} else {
    try {
        const wasm = await import("/wasm/mailbox.js");
        await wasm.default();
        // The token never goes into a URL from here: every request carries it
        // as a header, which is what a cross-origin page cannot forge.
        wasm.start("mailbox", window.MAILBOX.token);
    } catch (e) {
        fallback("The web UI failed to start: " + e);
    }
}
