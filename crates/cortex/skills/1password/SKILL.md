---
name: 1password
description: "Retrieve secrets and credentials via op CLI"
requires:
  bins: ["op"]
install:
  op:
    brew: 1password-cli
    url: "https://developer.1password.com/docs/cli/get-started/"
---

# 1Password

`op` CLI. Sessions expire after 30 min of inactivity; biometric unlock used
where available. Prefer `op read` / `op run` over piping raw secrets to
stdout.

```bash
eval $(op signin)          # interactive
op whoami
```

## Read

```bash
op item get "Database" --fields password
op item get "AWS Credentials" --fields "access key id"
op item get "Database" --format json | jq .
op item get "GitHub" --otp

# Scripting-safe (no exposure in process list):
DATABASE_URL=$(op read "op://Vault/Database/connection-string")
```

## Inject into a process

```bash
op run --env-file=.env.tpl -- ./start-server.sh
```

## Search / list

```bash
op vault list
op item list [--vault "Private"] [--categories Login --tags dev]
```

## Mutate

```bash
op item create --category "Secure Note" --title "Deploy Key" --notes "..."
op item edit "Database" password="new-password"
```
