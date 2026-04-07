---
name: email
description: "Manage emails via himalaya CLI. Read, send, reply, forward, search, and organize emails across Gmail, Outlook, Yahoo, iCloud, or any IMAP/SMTP provider."
category: email
requires:
  bins: ["himalaya"]
---

# Email

Manage email via Himalaya CLI (IMAP/SMTP). Works with Gmail, Outlook, iCloud, or any IMAP provider.

## Setup

```bash
brew install himalaya          # macOS
cargo install himalaya         # from source
himalaya account configure     # interactive setup wizard
```

Configuration file: `~/.config/himalaya/config.toml`
See `references/configuration.md` for provider-specific configs.

## List Folders

```bash
himalaya folder list
himalaya folder list --output json
```

## List Emails

```bash
himalaya envelope list                              # default inbox
himalaya envelope list --folder "Sent"              # specific folder
himalaya envelope list --page 1 --page-size 20      # pagination
himalaya envelope list --output json                # structured output
```

## Search Emails

```bash
himalaya envelope list from john@example.com subject meeting
himalaya envelope list "from:alice@company.com since:2026-01-01"
```

## Read Email

```bash
himalaya message read 42                            # plain text
himalaya message read 42 --output json              # structured
himalaya message export 42 --full                   # raw MIME
```

## Send Email

```bash
# Non-interactive with headers flag
himalaya message write -H "To:recipient@example.com" -H "Subject:Hello" "Message body here"

# From stdin using MML template
cat << 'EOF' | himalaya template send
From: me@example.com
To: recipient@example.com
Subject: Hello

Message body here.
EOF
```

See `references/message-composition.md` for MML syntax (attachments, HTML, multipart).

## Reply

```bash
himalaya message reply 42                           # reply to sender
himalaya message reply 42 --all                     # reply all
```

## Forward

```bash
himalaya message forward 42
```

## Move / Copy / Delete

```bash
himalaya message move 42 "Archive"
himalaya message copy 42 "Important"
himalaya message delete 42
```

## Flags

```bash
himalaya flag add 42 --flag seen
himalaya flag remove 42 --flag seen
himalaya flag add 42 --flag flagged
```

## Attachments

```bash
himalaya attachment download 42
himalaya attachment download 42 --dir ~/Downloads
```

## Multiple Accounts

```bash
himalaya account list
himalaya --account work envelope list               # use specific account
himalaya --account personal message read 42
```

## JSON Output

Always use `--output json` when you need to parse results programmatically:

```bash
himalaya envelope list --output json
himalaya folder list --output json
himalaya message read 42 --output json
```

## Notes

- First run may prompt for password or keyring access
- Gmail requires an App Password if 2FA is enabled (not your regular password)
- Outlook/Microsoft 365 supports both app passwords and OAuth2
- Use `RUST_LOG=debug himalaya envelope list` for troubleshooting
