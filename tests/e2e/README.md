# UI end-to-end, in a VM

TUI, web, and GUI driven inside a KVM guest on libvirt. Mail never leaves
that guest: IMAP and SMTP bind loopback, nftables rejects those ports off
loopback, and after `isolate` the nic sits on a network with no WAN.

Inspired by `~/Work/omarchy-vm`: snapshots, a viewer, `shot` / `record`,
keys through the hypervisor, SSH for everything else. Nothing here needs
`sudo` on the host.

This is **evidence**, not the PR gate. §15 still covers UI behaviour through
the CLI/JSON contract; a VM is too slow and too machine-specific to fail a
build. `MAILBOX_E2E=1 cargo test -p mailbox-cli --test e2e_vm` runs the
suite when a guest is already provisioned.

## One command

With a guest already provisioned, this is the whole loop — it starts the VM if
it is off, syncs the checkout and the binary, seeds, records, runs, and leaves
video, screenshots and logs in one directory:

```sh
tests/e2e/mailbox-e2e check folders        # or tui | web | gui | guide | compose | …
tests/e2e/mailbox-e2e check tui large
```

`check` records a video; `run` does not. Either takes `--video` / `--no-video`
to say so outright:

```sh
tests/e2e/mailbox-e2e run --video gui inbox    # just the run, with an mp4
tests/e2e/mailbox-e2e check --no-video folders # everything else, no recording
```

The video lands **inside that run's directory** — `out/<stamp>/gui-inbox.mp4`,
not `out/videos/`, which is where the manual `record stop` puts one.

It prints where the evidence went and exits non-zero if a check failed:

```
● folders / inbox: passed
  evidence  tests/e2e/out/20260916T211354Z
  video     tests/e2e/out/20260916T211354Z/folders-inbox.mp4
  shots     9 png
  logs      6 files in tests/e2e/out/20260916T211354Z/logs
  summary   tests/e2e/out/20260916T211354Z/summary.txt
```

`run` is the same thing without the VM lifecycle or the summary — use it when
the guest is already up and you want the fastest loop.

## Isolation

Three belts:

1. **Config.** The guest account points at `127.0.0.1` only. `require_tls =
   false` is allowed solely because the host is loopback (§9).
2. **nftables.** TCP 25/465/587/143/993/995/110/2525 are rejected unless the
   output interface is `lo`. The in-guest Dovecot and the SMTP sink stay
   reachable; nothing else on those ports is.
3. **Network.** `mailbox-e2e isolate` moves the nic onto libvirt network
   `mailbox-e2e`, which has no `<forward>` — DHCP and SSH from this host,
   no route to the internet.

`run isolation` also tcpdumps while HTML mail with remote images is opened,
and fails if a SYN leaves for a tracker host.

## First run

Host needs KVM, libvirt, and membership of the `libvirt` group.

```sh
cargo build -p mailbox-cli
tests/e2e/mailbox-e2e preflight
tests/e2e/mailbox-e2e create       # ~400 MB cloud image, then cloud-init
tests/e2e/mailbox-e2e provision    # SSH, Dovecot, nftables, rsync
tests/e2e/mailbox-e2e stop
tests/e2e/mailbox-e2e isolate      # unplug the WAN
tests/e2e/mailbox-e2e start
tests/e2e/mailbox-e2e stop
tests/e2e/mailbox-e2e snap ready   # cheap reset from here
tests/e2e/mailbox-e2e start
tests/e2e/mailbox-e2e run all
```

`create` uses libvirt's `default` NAT so pacman can install packages once.
After `isolate`, the guest cannot reach a mail server even if a test
misconfigures the account. Provision masks NTP wait: an isolated boot has
no WAN, and Arch's `systemd-time-wait-sync` would otherwise block sshd forever.

Evidence lands in `tests/e2e/out/<timestamp>/`: PNG screenshots, TUI pane
dumps, guest logs under `logs/`, and — from `check`, or `run --video` — an
`.mp4` of the compositor while the UI was driven.

## Commands

| Command | What it does |
|---|---|
| `create` | Arch cloud image overlay, cloud-init, first boot on NAT |
| `provision` | Wait for SSH, bootstrap Dovecot / nftables / Sway |
| `isolate` | Nic onto `mailbox-e2e` (no WAN). VM must be off |
| `sync` | rsync the checkout and copy `target/{release,debug}/mailbox` |
| `seed NAME` | `inbox`, `html-images`, `attachments`, `thread`, `large`, `imap` |
| `run UI [NAME]` | `tui`, `web`, `gui`, `folders`, `guide`, `compose`, `isolation`, or `all` |
| `check UI [NAME]` | The lot in one line: start, sync, record, run, collect |
| `--video` / `--no-video` | On `run` or `check`: record the compositor, or do not |
| `shot NAME` | Guest grim, else `virsh screenshot` |
| `record start\|stop` | wf-recorder in the guest (compositor) |
| `key KEYS` | `virsh send-key` — compositor shortcuts |
| `ssh [cmd]` | Shell in the guest |
| `snap NAME` / `revert NAME` | qcow2 snapshots; VM must be off |
| `view` | SPICE VGA — the login tty, not the test run |
| `console` | Serial (kernel, cloud-init, sshd). Ctrl+] detaches |
| `rescue` | VM off: attach serial, set console password `mailbox` |
| `start` / `stop` / `kill` / `status` / `ip` | Lifecycle |
| `destroy` | Undefine and delete the disk |

The graphical console is a getty. Tests run over SSH: TUI in tmux, GUI on
headless Sway. Nothing from `run all` appears on that login screen. After
`rescue` (or a new `create`), VGA login is `mailbox` / `mailbox`.

## Scenarios

| Name | What it is |
|---|---|
| `inbox` | Unread, read, flagged, a short thread |
| `html-images` | Remote `<img>` and mixed-text links (§9) |
| `attachments` | A PDF-shaped part and a JPEG-shaped part |
| `thread` | A four-message conversation |
| `large` | `debug generate` — default 3000 messages |
| `imap` | Same as inbox, but APPENDed to Dovecot and synced |
| `compose` | `mailbox send` into the loopback SMTP sink |
| `isolation` | Probe + tcpdump while HTML mail is on screen |

Each UI is its own script under `guest/tests/`, and each runs on its own in
the guest as well as through the runner:

```
guest/lib.sh          the session, the binary, the helpers, the recorder
guest/run.sh          a dispatcher: run.sh <ui> [scenario] → tests/<ui>.sh
guest/tests/tui.sh    one file per UI — web.sh, gui.sh, folders.sh, …
guest/tests/folders.py  the driver folders.sh runs
guest/cdp.py          the DevTools client the drivers import
```

```sh
mailbox-e2e ssh "bash mailbox/tests/e2e/guest/tests/folders.sh inbox"
```

`folders` is the layout-focused browser scenario. It drives the served page
through Chromium's debugger (`guest/cdp.py`, stdlib
only — the guest has no pip after `isolate`) and checks §7 behaviour a
screenshot cannot show:

- the folder list counts **unread**, not messages, with both numbers in the
  hover, and the count follows a message being read and marked unread again;
- on a phone-shaped viewport the panes are a stack — folders → list → reader —
  that the back arrow walks up, which is how the folder list is reached at all.

`guide` drives and photographs every state used by `docs/USER-GUIDE.md`:
first-run setup, reading, folders, compose, and shortcut help. It also executes
the documented read toggle, archive, trash, and spam actions and checks the
result through the HTTP API. After a passing run, refresh the checked-in images
with `tests/e2e/update-guide-screenshots PATH-TO-EVIDENCE`.

Every click is a real mouse event at the control's rendered position, because
the bug it was written for was an arrow that was visible and did nothing.
Checks land in `folders-checks.json` beside the screenshots.

## Settings

`tests/e2e/config`, sourced if present (`config.example` is the template):

| Variable | Default |
|---|---|
| `MAILBOX_E2E_NAME` | `mailbox-e2e` |
| `MAILBOX_E2E_USER` | `mailbox` |
| `MAILBOX_E2E_MEMORY` | `4096` |
| `MAILBOX_E2E_CPUS` | `2` |
| `MAILBOX_E2E_DISK_SIZE` | `20` |

QEMU runs as `libvirt-qemu`. A `700` home directory needs one traverse bit,
same as omarchy-vm:

```sh
setfacl -m u:libvirt-qemu:--x "$HOME"
```

## Notes

- **Graphics.** Software virtio, no 3D. `virsh screenshot` works; a GL
  display would not. The GUI and Chromium run under headless Sway in the
  guest (`WLR_BACKENDS=headless`), and `grim` photographs that.
- **The binary.** Built on this host and copied in. The guest is Arch, so
  glibc and `webkit2gtk-4.1` match Omarchy closely. Rebuild, then `sync`.
- **Reset.** `stop && revert ready && start` is a clean seeded-capable
  guest. Each `run` reseeds; it does not revert the disk.
- **No outside mail.** There is no OAuth, no real MX, no default route
  after `isolate`. The SMTP sink writes `/tmp/mailbox-e2e-sent/*.eml`.
