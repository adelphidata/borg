---
name: email
description: "Manage emails via himalaya CLI. Read, send, reply, forward, search, and organize emails across Gmail, Outlook, Yahoo, iCloud, or any IMAP/SMTP provider."
category: core
triggers:
  - "check my email"
  - "send an email"
  - "draft a reply"
  - "email inbox"
requires:
  bins: ["himalaya"]
install:
  himalaya:
    brew: himalaya
    url: "https://github.com/pimalaya/himalaya"
---

# Email

Himalaya CLI (IMAP/SMTP). Config: `~/.config/himalaya/config.toml`
(`himalaya account configure` for interactive setup; see
`references/configuration.md` for provider specifics). Gmail with 2FA needs
an App Password.

Add `--output json` to any read command for parseable output.

## Folders & messages

```bash
himalaya folder list
himalaya envelope list                              # default inbox
himalaya envelope list --folder "Sent" --page 1 --page-size 20
himalaya envelope list from john@example.com subject meeting
himalaya envelope list "from:alice@co.com since:2026-01-01"
himalaya message read 42
himalaya message export 42 --full                   # raw MIME
```

## Send

```bash
himalaya message write -H "To:r@example.com" -H "Subject:Hi" "Body"

cat << 'EOF' | himalaya template send
From: me@example.com
To: r@example.com
Subject: Hi

Body.
EOF
```

MML for attachments/HTML/multipart: see `references/message-composition.md`.

## Reply / forward / organize

```bash
himalaya message reply 42 [--all]
himalaya message forward 42
himalaya message move|copy 42 "Archive"
himalaya message delete 42
himalaya flag add|remove 42 --flag seen|flagged
himalaya attachment download 42 [--dir ~/Downloads]
```

## Multiple accounts

```bash
himalaya account list
himalaya --account work envelope list
```

Debug: `RUST_LOG=debug himalaya envelope list`.
