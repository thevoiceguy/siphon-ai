# Local Homer stack

A minimal [Homer](https://sipcapture.io/) deployment for inspecting
the HEP3 traffic SiphonAI emits.

Three containers:

- **heplify-server** — the HEP3 collector. UDP on `9060`, writes to
  Postgres.
- **homer-db** — Postgres 16 with the Homer schema (created
  automatically on first start).
- **homer-webapp** — the Homer web UI on `http://127.0.0.1:9080`.

Default UI login: `admin` / `sipcapture` (change immediately if you
expose this beyond localhost).

## Bring it up

```sh
docker compose -f examples/homer-stack/compose.yaml up -d
```

First boot takes ~30 seconds while Postgres initializes and
heplify-server creates the partitioned schema. Watch with
`docker compose logs -f heplify-server` if you want to see it
happen.

## Wire SiphonAI to it

The daemon ships HEP packets only when `[hep]` is enabled in its
config. The simplest path:

```sh
# Use the bundled HEP-enabled config:
cp examples/homer-stack/siphon-ai-hep.toml docker/local-dev.toml

# (re)start the daemon stack:
docker compose -f docker/compose.yaml up -d
```

The config points at `host.docker.internal:9060`. Docker Desktop
resolves that automatically; on native Linux the daemon's compose
file already passes `--add-host=host.docker.internal:host-gateway`
so it works there too.

## Place a call, see it in Homer

```sh
# From your host, with SIPp:
sipp -sf test-harness/sipp-scenarios/basic_call_then_bye.xml \
     -m 1 -p 5080 -s 1000 127.0.0.1:5070

# Then open http://127.0.0.1:9080/, log in as admin / sipcapture,
# and click "Search". The call appears within seconds with the SIP
# flow, RTCP reports, and CDR/log chunks correlated by Call-ID.
```

## What lands in Homer

| Source                | HEP chunk | What Homer renders              |
|-----------------------|-----------|---------------------------------|
| `siphon-rs::sip-hep`  | 0x01 SIP  | every parsed/serialized SIP message |
| `forge-media::forge-hep` | 0x05 RTCP | every observed RTCP SR/RR/SDES/BYE |
| `forge-media::forge-hep` | 0x20 RTP-QoS | per-RR derived QoS summary (jitter, loss) |
| `siphon-ai-cdr::HepCdrSink` | 0x65 CDR | end-of-call JSON record |

All correlated by the SIP Call-ID (HEP chunk `0x0011`), so Homer's
call-search returns a single threaded view across protocols.

## Persistence + reset

DB state lives in the `homer-db-data` named volume — calls survive
`docker compose down` / `up`. Wipe with `down -v`:

```sh
docker compose -f examples/homer-stack/compose.yaml down -v
```

## What this stack is NOT

- Production-grade. Default passwords, no TLS, single Postgres
  replica, no backups. For real deployments follow the upstream
  [docker.sipcapture.io](https://github.com/sipcapture/docker)
  guide.
- Grafana / Prometheus / Loki integrated. Homer talks to those if
  you point it at them (see env vars in the upstream
  `sipcapture/webapp` image); out of scope for this example.
- Multi-node. One heplify-server per node is the standard pattern;
  scale by running more.
