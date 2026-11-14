# antigravity-status Telemetry Worker

## 1. Deploy the Cloudflare Worker & D1 (`worker/`)

```bash
cd worker
bun install
bunx wrangler login

# Create the D1 database and paste the returned `database_id` into `worker/wrangler.toml`
bunx wrangler d1 create antigravity-status-telemetry

# Apply initial schema migration to remote D1
bun run db:migrate:remote

# Deploy Worker first on a fresh setup (registers the Worker + hourly :10 UTC rollup cron;
# `/api/telemetry/ingest` and `/api/telemetry/stats` fail closed with 503 until secrets are added)
bun run deploy

# Configure required runtime secrets
bunx wrangler secret put ANONYMIZATION_SALT
bunx wrangler secret put TELEMETRY_API_TOKEN
```

Verify the live deployment:

```bash
curl -fsS https://antigravity-status-telemetry.<your-subdomain>.workers.dev/api/telemetry/health
curl -fsS -H "Authorization: Bearer <TELEMETRY_API_TOKEN>" \
  "https://antigravity-status-telemetry.<your-subdomain>.workers.dev/api/telemetry/stats?granularity=hourly"
```

When wiring [`united-magic-association/antigravity-status-telemetry-data`](https://github.com/united-magic-association/antigravity-status-telemetry-data), configure `TELEMETRY_BASE_URL` as an Actions **Repository variable** (base Worker URL without trailing `/api/telemetry/stats`) and `TELEMETRY_API_TOKEN` as a **Repository secret**.

## 2. Enable Opt-In Telemetry Locally

In `~/.config/antigravity-status/config.toml` (using the public Worker or your own deployment):

```toml
[telemetry]
enabled = true
endpoint = "https://antigravity-status-telemetry.umahq.workers.dev"
auto_upload = true
upload_interval_secs = 300
```

Audit exact payloads before enabling with:

```bash
antigravity-status telemetry preview
```
