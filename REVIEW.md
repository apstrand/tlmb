# Security review brief — §8 pairing

What needs an outside look before the short-code path is trusted with real
credentials, in priority order. §16 required this review *before* the PAKE
shipped; the gate was lifted deliberately, so the code exists and the review
does not.

**What is at stake.** The payload is every credential the user owns — IMAP
passwords, OAuth refresh tokens, and the §5.4 state key. A break here is a
mailbox takeover, not a degraded feature.

**Scope.** Commits `f08ceed` (PAKE) and `5508cd1` (relay), plus `c409ced` for
the state-folder AEAD reuse. Roughly 1,300 lines, of which ~250 are load-bearing.

```sh
git log --oneline e57bf9a..HEAD          # the six commits under review
git diff e57bf9a..HEAD -- crates/mailbox-core/src/pair crates/mailbox-cli/src/relay.rs
```

---

## 1. Rate limiting collapses behind a reverse proxy — **defect**

`crates/mailbox-cli/src/relay.rs:166` takes the client address from
`peer_addr()` and nothing else; there is no `X-Forwarded-For` handling
(`grep -c forwarded` → 0). Every deployment README.md recommends — nginx,
Caddy — terminates TLS in front, so **every request arrives from `127.0.0.1`**
and the 120/min per-address bucket becomes a single global bucket.

Two consequences: an attacker gets no per-source limit at all, and legitimate
users throttle each other. §8.2 asks for rate limiting specifically to bound
online guessing, so this weakens the one control standing between a 45-bit code
and a determined guesser.

Needs a decision on trusted-proxy handling (which header, whose word to take)
rather than a quick patch — a naively trusted `X-Forwarded-For` is worse than
none.

```sh
sed -n '155,175p' crates/mailbox-cli/src/relay.rs   # where peer comes from
sed -n '105,120p' crates/mailbox-cli/src/relay.rs   # the bucket it feeds
```

## 2. `MAILBOX_SEED` makes pairing secrets deterministic

`Rng::from_env()` (`crates/mailbox-core/src/clock.rs:85`) returns a BLAKE3
counter-mode PRNG seeded from `MAILBOX_SEED` when that variable is set, and
from the OS otherwise. It is the RNG behind both the short code and the
256-bit LAN secret:

- `crates/mailbox-cli/src/pairing.rs:91` — `pake::generate_code`
- `crates/mailbox-cli/src/pairing.rs:232` — `crypto::generate_secret`

The variable exists for reproducible tests. Nothing refuses it at runtime, so a
process that inherits it pairs with predictable secrets. Question for review:
should key generation ignore `MAILBOX_SEED`, or refuse to run under it?

```sh
grep -rn "Rng::from_env()" crates/mailbox-cli/src/pairing.rs
sed -n '80,100p' crates/mailbox-core/src/clock.rs
```

## 3. The CPace implementation tracks an early draft

`pake-cpace 0.1.7` (`Cargo.lock`, checksum `bd8ddcfd…`) is a port of jedisct1's
libsodium CPace and follows **`draft-haase-cpace-01`**, an individual draft —
not `draft-irtf-cfrg-cpace`, which is what §8.1 cites when it says "CPace". The
divergences between the two need assessing, and the group is Ristretto255
rather than §8.1's stated X25519 (deliberate; cofactor handling).

The crate is 197 lines and worth reading in full:

```sh
cargo tree -p mailbox-core -i pake-cpace
less ~/.cargo/registry/src/*/pake-cpace-0.1.7/src/lib.rs
```

Points of interest: `CPace::new` (generator derivation, zero-padding of the
password block), `finalize` (identity check, transcript hash), and whether
`step2`'s `finalize(ya, ya, ctx.p)` argument order is correct.

## 4. The composition around it

Ours, not the crate's, and the part most likely to be wrong.

| What | Where |
|---|---|
| Identity + AD binding | `pair/pake.rs:71-79` |
| Two directional keys folded into one secret | `pair/pake.rs:242` (`mix`) |
| SAS transcript | `pair/pake.rs:254` |
| AEAD shared with the QR/LAN path | `pair/crypto.rs:87,120` |

Specific questions:

- `mix` runs HKDF over `k1 ‖ k2` to produce one 256-bit secret, discarding
  CPace's directionality. Does folding lose a property the two-key form was
  providing?
- The transcript covers the offer and answer, length-prefixed. **It does not
  cover the slot or the relay URL.** Should it? An attacker who can run two
  slots concurrently is the case to think about.
- `seal_with_key`/`open_with_key` were extracted so the state folder could
  share the AEAD under a different HKDF label. Confirm the labels give real
  domain separation and that no nonce reuse is possible across the two users.

## 5. The SAS can be skipped

`confirm_sas` returns `true` immediately under `--yes`
(`crates/mailbox-cli/src/pairing.rs:615`). §8.1 calls the short authentication
string the defence against an active MITM, so this is an escape hatch around
the primary control. It is deliberate — §6.4 forbids prompting when stdin is
not a terminal — but a reviewer should say whether it is acceptable on a
transport where the attacker is remote rather than on the LAN.

Mitigation already in place: a wrong code still fails at the AEAD, tested at
`crates/mailbox-cli/tests/relay.rs` →
`a_guessed_code_cannot_pair_even_with_the_word_check_skipped`.

## 6. Channel-id separation

The break this design was corrected for. The slot is public and relay-allocated;
the 45-bit code never reaches the relay. Deriving the slot from the code — as
§8.1 originally specified — would have allowed offline recovery of the code by
anyone who saw a channel.

Confirm the separation actually holds end to end:

```sh
cargo test -p mailbox-cli --test relay -- the_relay_never_sees_anything_readable
sed -n '11,34p' crates/mailbox-core/src/pair/relay.rs   # the reasoning
```

## 7. No known-answer vectors

§15 requires them. `pake-cpace` draws its scalars inside `step1`/`step2` with no
RNG injection, so a handshake cannot be replayed to a fixed answer. Pinned
instead: alphabet, identities, AD, and HKDF label
(`pair/pake.rs` → `the_derivation_label_is_pinned`, `the_alphabet_is_pinned`).

Real vectors need an upstream change accepting an RNG, or a cross-check against
the libsodium C implementation's vectors. A reviewer may consider this blocking.

## 8. State-folder crypto (§5.4)

Lower priority — not yet reachable, since no transport reads or writes
`Mailbox/.state`. Same AEAD, different label, key held by
`credentials::state_secret`.

```sh
sed -n '32,52p' crates/mailbox-core/src/state/wire.rs
grep -n "state_secret\|STATE_SERVICE" crates/mailbox-core/src/credentials.rs
```

---

## What is already covered

So a reviewer does not repeat it. All green as of `5508cd1`.

```sh
cargo test -p mailbox-core --lib -- pair::pake     # 15: handshake, codes, SAS
cargo test -p mailbox-core --test pair_crypto      # 17: AEAD + payload vectors
cargo test -p mailbox-cli  --test relay            #  6: four-process end to end
cargo test -p mailbox-cli  --test pairing          # 16: LAN, export file, MITM
cargo test                                         # 54 suites
```

Properties under test: matching codes agree and mismatched ones diverge; a
wrong code is caught by both the words and the AEAD; a replayed transcript
gives unrelated secrets; degenerate points are refused; a relay box is written
once and read once; the relay's stored bytes contain no address or hostname;
Crockford confusables normalise; secrets do not appear in `Debug` output.

## Known gaps, already written down

In `TODO.md`: the relay sender learns no acceptance (three one-way messages);
the LAN has no short-code path; the relay holds state in memory, so multiple
instances need sticky routing.

## Reviewing without a relay of your own

```sh
cargo build
./target/debug/mailbox relay --port 8788 &
./target/debug/mailbox --json --yes pair --relay http://127.0.0.1:8788   # prints a code
./target/debug/mailbox --yes pair --relay http://127.0.0.1:8788 send <code>
```

Loopback is the only address where the client accepts plaintext (§9), which is
what makes this inspectable with `tcpdump` without a certificate.
