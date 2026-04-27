# Himalaya Configuration

Configuration file: `~/.config/himalaya/config.toml`

## Gmail

```toml
[accounts.gmail]
email = "you@gmail.com"
display-name = "Your Name"
default = true

backend.type = "imap"
backend.host = "imap.gmail.com"
backend.port = 993
backend.encryption.type = "tls"
backend.login = "you@gmail.com"
backend.auth.type = "password"
backend.auth.cmd = "pass show google/app-password"

message.send.backend.type = "smtp"
message.send.backend.host = "smtp.gmail.com"
message.send.backend.port = 587
message.send.backend.encryption.type = "start-tls"
message.send.backend.login = "you@gmail.com"
message.send.backend.auth.type = "password"
message.send.backend.auth.cmd = "pass show google/app-password"
```

Gmail requires an App Password if 2FA is enabled. Generate at: https://myaccount.google.com/apppasswords

## Outlook / Microsoft 365

```toml
[accounts.outlook]
email = "you@outlook.com"
display-name = "Your Name"

backend.type = "imap"
backend.host = "outlook.office365.com"
backend.port = 993
backend.encryption.type = "tls"
backend.login = "you@outlook.com"
backend.auth.type = "password"
backend.auth.cmd = "pass show outlook/app-password"

message.send.backend.type = "smtp"
message.send.backend.host = "smtp.office365.com"
message.send.backend.port = 587
message.send.backend.encryption.type = "start-tls"
message.send.backend.login = "you@outlook.com"
message.send.backend.auth.type = "password"
message.send.backend.auth.cmd = "pass show outlook/app-password"
```

For personal Outlook.com accounts, use `imap.outlook.com` / `smtp.outlook.com` instead.

## iCloud

```toml
[accounts.icloud]
email = "you@icloud.com"
display-name = "Your Name"

backend.type = "imap"
backend.host = "imap.mail.me.com"
backend.port = 993
backend.encryption.type = "tls"
backend.login = "you@icloud.com"
backend.auth.type = "password"
backend.auth.cmd = "pass show icloud/app-password"

message.send.backend.type = "smtp"
message.send.backend.host = "smtp.mail.me.com"
message.send.backend.port = 587
message.send.backend.encryption.type = "start-tls"
message.send.backend.login = "you@icloud.com"
message.send.backend.auth.type = "password"
message.send.backend.auth.cmd = "pass show icloud/app-password"
```

Generate app-specific password at: https://appleid.apple.com

## Password Options

```toml
# Raw password (testing only, not recommended)
backend.auth.raw = "your-password"

# From command (recommended)
backend.auth.cmd = "pass show email/imap"

# System keyring
backend.auth.keyring = "imap-example"
```

## OAuth2

```toml
backend.auth.type = "oauth2"
backend.auth.client-id = "your-client-id"
backend.auth.client-secret.cmd = "pass show oauth/client-secret"
backend.auth.access-token.cmd = "pass show oauth/access-token"
backend.auth.refresh-token.cmd = "pass show oauth/refresh-token"
backend.auth.auth-url = "https://provider.com/oauth/authorize"
backend.auth.token-url = "https://provider.com/oauth/token"
```

## Multiple Accounts

```toml
[accounts.personal]
email = "personal@gmail.com"
default = true
# ... backend config ...

[accounts.work]
email = "work@company.com"
# ... backend config ...
```

Use `himalaya --account work envelope list` to switch.

## Folder Aliases

```toml
[accounts.default.folder.alias]
inbox = "INBOX"
sent = "Sent"
drafts = "Drafts"
trash = "Trash"
```

## Signature

```toml
[accounts.default]
signature = "Best regards,\nYour Name"
signature-delim = "-- \n"
```

## Downloads Directory

```toml
[accounts.default]
downloads-dir = "~/Downloads/himalaya"
```
