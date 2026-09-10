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
dumps, videos if you `record`.

## Commands

| Command | What it does |
|---|---|
| `create` | Arch cloud image overlay, cloud-init, first boot on NAT |
| `provision` | Wait for SSH, bootstrap Dovecot / nftables / Sway |
| `isolate` | Nic onto `mailbox-e2e` (no WAN). VM must be off |
| `sync` | rsync the checkout and copy `target/{release,debug}/mailbox` |
| `seed NAME` | `inbox`, `html-images`, `attachments`, `thread`, `large`, `imap` |
| `run UI [NAME]` | `tui`, `web`, `gui`, `compose`, `isolation`, or `all` |
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
