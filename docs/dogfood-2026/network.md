# Cuenca 2026 dogfood receipts — network part (BRO-3257, BRO-3259)

Observed 2026-10-27 00:00–00:30Z. The authority runs on the NUC (WSL2 Ubuntu 24.04, mirrored
networking) and two agents on two machines use it at once over the tailnet. This receipt was
itself written by the NUC agent in its own fork and landed through Cuenca.

## Host

- Authority: `cuenca serve` from `deploy/install.sh` with `--tailnet-addr 100.112.149.32
  --tailnet-addr fd7a:115c:a1e0::9237:9520` (cuenca#34), binary built on the NUC at main
  `262ef7a` (release, sha256 `da9ca7c0d035d0a7bbc06cc33a8d7effdba0162e178f3d4badb1d7888f5d7846`).
- `sudo bash deploy/check.sh` (full mode, systemd and listeners): `check.sh: all passed`.
  `:445 listens on the tailnet addresses only: 100.112.149.32:445 [fd7a:115c:a1e0::9237:9520]:445`;
  agent API on `100.112.149.32:7419` only; 401 without a key.
- Node identity: a whois-only Tailscale node `nuc-cuenca` inside WSL (userspace networking, shields
  up, no DNS or routes; owner decision 2026-10-26). Requests still arrive through Windows'
  Tailscale with the peer's own source address (a WSL listener saw `100.82.213.74` from the Mac).

## Two agents, two machines, one location

| Step | Mac (`mac-agent@nwPbDUaNzo11CNTRL`) | NUC (`nuc-agent@n3RXH4cuyj11CNTRL`) |
|---|---|---|
| login | `logged in as mac-agent (agent) on node nwPbDUaNzo11CNTRL` | `logged in as nuc-agent (agent) on node n3RXH4cuyj11CNTRL` |
| fork `cuenca` (within 1 s of each other) | `mac-bro3257-deploy`, epoch 1 | `nuc-bro3257-receipt`, epoch 1 |
| checkout | 184 files | 184 files |
| edit | deploy/ change of cuenca#34 (7 files) | this file |
| snapshot | changeset `6c858310da65…`, tree `1ac1fb2c…` = git tree of the hand commit `fdc7b6b` | see the PR |
| volume | `forks/mac-bro3257-deploy/` updated on the NUC **1.9 s** after the snapshot (spec: ≤ 15 s), `2750 cuenca:cuenca` | — |
| land | cuenca#34, head tree `1ac1fb2c…` | this PR |

## Boundaries, over the real network

- The NUC agent asking for the Mac's fork: `checkout` and `log` both give
  `no such thing, or not yours to read (not_found, HTTP 404)`.
- The Mac's key presented from a third tailnet node (the VPS, `srv1692698-agent`): `HTTP 401`;
  the same key from the Mac: `HTTP 200`. Keys are bound to their node by `tailscale whois`.
- Samba from the VPS (not an enrolled device), `smbclient -N -L //100.112.149.32`:
  `Protocol negotiation … failed: NT_STATUS_INVALID_NETWORK_RESPONSE` (dropped by `hosts allow`).
  The Mac, an enrolled device, reaches `:445` and `:7419`.

## Not shown here

- Finder over Samba as `cuenca-owner`: the password is generated on the host and handed to the
  owner out of band; the Finder check is the owner's (README step 4 and 6).
- A reboot of the NUC with the authority installed (the boot chain itself was proved under
  BRO-3461 on 2026-10-24).
- Probing the NUC's public address from off the LAN after Samba came up.
