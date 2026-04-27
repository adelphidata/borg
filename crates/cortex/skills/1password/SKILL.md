---
name: 1password
description: "Retrieve secrets and credentials via op CLI"
requires:
  bins: ["op"]
---

# 1Password Skill

Use the `op` CLI to access secrets stored in 1Password.

## Setup & Authentication

```bash
# Sign in (interactive, opens browser for biometric/SSO)
eval $(op signin)

# Verify authentication
op whoami
```

## Retrieve Secrets

```bash
# Get a specific field from an item
op item get "Database" --fields password
op item get "AWS Credentials" --fields "access key id"

# Get full item as JSON
op item get "Database" --format json

# Get OTP code
op item get "GitHub" --otp
```

## Secret References

```bash
# Inject secrets into commands without exposing them
op run --env-file=.env.tpl -- ./start-server.sh

# Use secret references inline
DATABASE_URL=$(op read "op://Vault/Database/connection-string")
export API_KEY=$(op read "op://Private/Service/api-key")
```

## Search & List

```bash
# List items
op item list
op item list --vault "Private"
op item list --categories Login --tags dev

# Search items
op item list | grep -i "aws"
```

## Vaults

```bash
op vault list
op item list --vault "Development"
```

## Create & Edit

```bash
# Create a secure note
op item create --category "Secure Note" --title "Deploy Key" --notes "key-content"

# Edit a field
op item edit "Database" password="new-password-here"
```

## Notes

- Always use `op read "op://vault/item/field"` for scripting; it avoids exposing secrets in process lists
- The CLI uses biometric unlock when available (Touch ID on macOS)
- Never pipe raw secrets to stdout unnecessarily; use `op run` to inject into env
- Session tokens expire after 30 minutes of inactivity
- Use `--format json` and pipe through `jq` for structured data
