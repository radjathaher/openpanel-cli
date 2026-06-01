# OpenPanel CLI

CLI for the OpenPanel API. Command tree is driven by `schemas/command_tree.json`.

## Install

### Install script

```bash
curl -fsSL https://raw.githubusercontent.com/radjathaher/openpanel-cli/main/install.sh | sh
```

### Homebrew

```bash
brew tap radjathaher/tap
brew install openpanel
```

### Download binary

Grab the latest `openpanel-cli-<version>-<os>-<arch>.tar.gz` asset from GitHub Releases, unpack, and place `openpanel` on your `PATH`.

## Auth

Create a client in the OpenPanel dashboard (Organization → API Clients). Client types:
- `write`: track/event/profile
- `read`: export/import/insights
- `root`: organization-wide access to all APIs, including manage endpoints

Set environment variables (recommended):

```bash
export OPENPANEL_CLIENT_ID=...
export OPENPANEL_CLIENT_SECRET=...
```

Optional:

```bash
export OPENPANEL_API_URL=https://api.openpanel.dev
```

For self-hosted OpenPanel, point the CLI at that API host:

```bash
export OPENPANEL_API_URL=https://op-api.example.com
```

Validate the configured client:

```bash
openpanel auth doctor --project-id my-project --pretty
```

`auth doctor` classifies the current credentials as:
- `root`: Manage and Insights probes work.
- `read`: Insights works, Manage does not.
- `write_or_invalid`: official analytics reads do not work; this is common when using a frontend tracking client.

## Usage

List available commands:

```bash
openpanel list
```

Describe a command:

```bash
openpanel describe insights metrics
openpanel describe manage projects create
```

Track event:

```bash
openpanel track event --name signup --properties '{"plan":"pro"}'
```

Identify user:

```bash
openpanel track identify --profile-id user_123 --email a@b.com --properties '{"tier":"pro"}'
```

Alias:

```bash
openpanel track alias --profile-id user_123 --alias legacy_456
```

Increment:

```bash
openpanel track increment --profile-id user_123 --property visits --value 1
```

Event ingest (deprecated):

```bash
openpanel event post --name screen_view --properties '{"path":"/"}'
```

Profile update:

```bash
openpanel profile update --profile-id user_123 --email a@b.com
```

Export events:

```bash
openpanel export events --project-id my-project --event screen_view --start 2024-04-15 --end 2024-04-18
```

Efficient event query:

```bash
openpanel events query \
  --project-id my-project \
  --day 2026-05-12 \
  --tz-offset +07:00 \
  --events screen_view,quiz_started,payment_completed \
  --where 'country=US and properties.utm_source=meta' \
  --select createdAt,name,sessionId,country,properties.utm_content \
  --format jsonl \
  --output /tmp/openpanel-events.jsonl
```

Funnel summary:

```bash
openpanel analytics funnel \
  --project-id my-project \
  --day 2026-05-12 \
  --tz-offset +07:00 \
  --where 'country=US and properties.utm_source=meta' \
  --steps screen_view,quiz_started,paywall_viewed,checkout_clicked,payment_completed \
  --unit sessionId
```

Debug export pagination:

```bash
openpanel export events \
  --project-id my-project \
  --event screen_view \
  --start 2026-05-11T17:00:00Z \
  --end 2026-05-12T16:59:59Z \
  --limit 100 \
  --debug-pagination
```

Export charts:

```bash
openpanel export charts \
  --project-id my-project \
  --events '[{"name":"screen_view","segment":"user"}]' \
  --breakdowns '[{"name":"country"}]' \
  --interval day \
  --range 30d
```

Import events (JSON array):

```bash
openpanel import events --events '[{"name":"screen_view","profileId":"u1","created_at":"2024-01-01T00:00:00Z","properties":{}}]'
```

Insights metrics:

```bash
openpanel insights metrics --project-id my-project --range 30d
```

Manage projects:

```bash
openpanel manage projects list
openpanel manage projects create --name "My Project" --domain https://example.com --types website
```

Misc:

```bash
openpanel misc favicon --url https://example.com
openpanel misc geo
```

Live websocket (requires session cookies in most setups):

```bash
openpanel live events --project-id my-project --header "cookie=YOUR_SESSION_COOKIE"
```

## Output

- `--pretty` for formatted JSON
- `--raw` for status/headers/body
- `--dry-run` to print the request without sending
- `--body` to pass full JSON for POST/PATCH/PUT
- `--header` to add custom headers (repeatable)

## Notes

- Arrays can be repeated flags or JSON arrays (e.g. `--event a --event b` or `--event '["a","b"]'`).
- JSON flags must be valid JSON strings.
- `events query` and `analytics funnel` are workflow commands. They paginate `/export/events`, retry transient failures, exact-filter `createdAt` client-side, and stream compact output.
- OpenPanel `/export/events` currently filters dates at day granularity server-side, so timestamp-exact analysis should use `events query` or `analytics funnel`.
- Use `--day YYYY-MM-DD --tz-offset +07:00` for WIB calendar days.
