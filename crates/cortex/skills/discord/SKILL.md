---
name: discord
description: "Send messages and manage Discord channels"
requires:
  bins: ["curl"]
  env: ["DISCORD_BOT_TOKEN"]
install:
  curl:
    brew: curl
    apt: curl
---

# Discord

Discord REST API v10 via `curl`. Bot needs Send Messages, Add Reactions,
Manage Messages, Read Message History. IDs are snowflakes; emoji must be
URL-encoded (`%E2%9C%85` = ✅); mention users as `<@USER_ID>`; respect
`X-RateLimit-*` headers. Avoid markdown tables in messages.

## Pattern

```bash
curl -s -X <METHOD> "https://discord.com/api/v10/<path>" \
  -H "Authorization: Bot $DISCORD_BOT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '<json>'        # omit -d/-H Content-Type on GET/DELETE
```

## Examples

Send:

```bash
curl -s -X POST "https://discord.com/api/v10/channels/CHANNEL_ID/messages" \
  -H "Authorization: Bot $DISCORD_BOT_TOKEN" -H "Content-Type: application/json" \
  -d '{"content":"Hello"}'
```

React (PUT, empty body, URL-encoded emoji):

```bash
curl -s -X PUT "https://discord.com/api/v10/channels/CHANNEL_ID/messages/MSG_ID/reactions/%E2%9C%85/@me" \
  -H "Authorization: Bot $DISCORD_BOT_TOKEN"
```

## Endpoint index

Base: `https://discord.com/api/v10`

| Action | Method | Path | Body |
|---|---|---|---|
| List messages | GET | `/channels/{c}/messages?limit=20` | — |
| Get one | GET | `/channels/{c}/messages/{m}` | — |
| Edit | PATCH | `/channels/{c}/messages/{m}` | `{"content":"…"}` |
| Delete | DELETE | `/channels/{c}/messages/{m}` | — |
| React / unreact | PUT / DELETE | `/channels/{c}/messages/{m}/reactions/{emoji}/@me` | — |
| List reactions | GET | `/channels/{c}/messages/{m}/reactions/{emoji}` | — |
| Pin / unpin | PUT / DELETE | `/channels/{c}/pins/{m}` | — |
| List pins | GET | `/channels/{c}/pins` | — |
| Channel info | GET | `/channels/{c}` | — |
| Create thread | POST | `/channels/{c}/messages/{m}/threads` | `{"name":"…","auto_archive_duration":1440}` |
| Active threads | GET | `/guilds/{g}/threads/active` | — |
| Member | GET | `/guilds/{g}/members/{u}` | — |
