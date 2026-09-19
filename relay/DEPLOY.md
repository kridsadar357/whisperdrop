# Deploying the relay to riki-api.online

The relay is a small WebSocket broker with a persistent group registry
(`GROUPS_FILE`, JSON): it issues group numbers and per-device member tokens,
relays join requests to the group's head, and forwards transfer frames only
between registered members of the same group. The frame protocol is
documented at the top of `relay/src/main.rs`.

## Current deployment (since 2026-09-18)

`riki-api.online` is proxied by Cloudflare; the origin is a small VPS
(referred to as `orca-prod` below — the SSH alias on the maintainer's
machine). On that host:

| piece | where |
|---|---|
| binary (static musl build) | `/opt/whisperdrop-relay/whisperdrop-relay` |
| service | `systemctl status whisperdrop-relay` — `PORT=8765`, `GROUPS_FILE=/var/lib/whisperdrop-relay/groups.json` (`StateDirectory`), `Restart=always`, `DynamicUser` |
| registry | `/var/lib/whisperdrop-relay/groups.json` — back it up; losing it means every device must re-create/re-join its group |
| nginx site | `/etc/nginx/sites-available/riki-api.online` — `/ws` → `127.0.0.1:8765` (websocket upgrade, 1h read timeout), `/health` → `relay-ok`, `/whisperdrop/` static (update manifest) |
| TLS | Let's Encrypt via `certbot --nginx -d riki-api.online` (auto-renew) |

Port 8080 on that host belongs to another service — keep the relay on 8765.

Build the Linux binary from the Mac (no Rust on the server; Docker on both):

    cd relay
    docker run --rm --platform linux/amd64 -v "$PWD":/src -w /src rust:1-bookworm bash -c \
      'rustup target add x86_64-unknown-linux-musl && cargo build --release --target x86_64-unknown-linux-musl --target-dir /src/target/linux'
    scp target/linux/x86_64-unknown-linux-musl/release/whisperdrop-relay orca-prod:/tmp/
    ssh orca-prod 'install -m 755 /tmp/whisperdrop-relay /opt/whisperdrop-relay/whisperdrop-relay && systemctl restart whisperdrop-relay'

Smoke test from any machine (needs a receiver online in the same group):

    ./scripts/test-matrix.sh 127.0.0.1 wss://riki-api.online/ws <group-id>
    # or just reachability:
    whisperdrop status        # or: whisperdrop devices

## Generic setup

## Build for Linux server (from the project root)
    cd relay
    cargo build --release        # build natively on the server, or
    # cross-compile: rustup target add x86_64-unknown-linux-gnu && cargo build --release --target x86_64-unknown-linux-gnu

## Run (behind TLS)

Run **one long-lived instance** (a systemd service is ideal):

    PORT=8080 ./whisperdrop-relay

Then point your reverse proxy so that  wss://riki-api.online/ws
forwards to 127.0.0.1:8080  (nginx/caddy websocket pass-through).

The broker stores connected device sockets in memory. Do not route `/ws` to a
Cloudflare Worker, a serverless function, or multiple unshared instances:
two devices can then register successfully but land on different processes,
so discovery and file frames disappear. Keep the WebSocket path pinned to the
same relay process, or replace the in-memory peer map with a shared pub/sub
backend before scaling out.

For Caddy, the relevant route is:

    riki-api.online {
        reverse_proxy /ws 127.0.0.1:8080
    }

For nginx, ensure upgrade headers are retained:

    location /ws {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_read_timeout 3600;
    }

## Verify
Open two terminals and use distinct ids. In terminal A:

    wscat -c wss://riki-api.online/ws
    > {"type":"ping"}                     -> {"type":"pong",...}
    > {"type":"create_group","device_id":"a","device_name":"A"}   -> group + token
    # reconnect, then:
    > {"type":"register","group":"<group>","id":"a","token":"<token>"} -> registered

Registering without a valid token is refused with an `error` frame. A successful WebSocket
upgrade alone is not sufficient: a `devices` reply and routed frame must both
work before WhisperDrop tunnel transfers can work.
