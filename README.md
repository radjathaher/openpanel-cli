# OpenPanel CLI

CLI for the OpenPanel API (track, export, insights, manage). Command tree is driven by `schemas/command_tree.json`.

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

Grab the latest `openpanel-macos-arm64` asset from GitHub Releases and place it on your `PATH`.

## Auth

Set environment variables (recommended):

```bash
export OPENPANEL_CLIENT_ID=...
export OPENPANEL_CLIENT_SECRET=...
```

Optional:

```bash
export OPENPANEL_API_URL=https://api.openpanel.dev
```

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

Increment:

```bash
openpanel track increment --profile-id user_123 --property visits --value 1
```

Export events:

```bash
openpanel export events --project-id my-project --event screen_view --start 2024-04-15 --end 2024-04-18
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

Insights metrics:

```bash
openpanel insights metrics --project-id my-project --range 30d
```

Manage projects:

```bash
openpanel manage projects list
openpanel manage projects create --name "My Project" --domain https://example.com --types website
```

## Output

- `--pretty` for formatted JSON
- `--raw` for status/headers/body
- `--dry-run` to print the request without sending
- `--body` to pass full JSON for POST/PATCH

## Notes

- Arrays can be repeated flags or JSON arrays (e.g. `--event a --event b` or `--event '["a","b"]'`).
- JSON flags must be valid JSON strings.
