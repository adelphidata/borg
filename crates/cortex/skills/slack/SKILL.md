---
name: slack
description: "Send messages and manage Slack channels"
requires:
  bins: ["curl"]
  env: ["SLACK_BOT_TOKEN"]
---

# Slack

Slack Web API via `curl`. Bot scopes: `chat:write`, `reactions:write`,
`pins:write`, `channels:history`, `users:read`. Channel IDs `C…`, user IDs
`U…`, timestamps double as message IDs (`1712023032.1234`). All responses are
JSON — check `ok`.

## Pattern

```bash
# POST (mutations)
curl -s -X POST https://slack.com/api/<method> \
  -H "Authorization: Bearer $SLACK_BOT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '<json body>'

# GET (reads) — params in querystring
curl -s "https://slack.com/api/<method>?<params>" \
  -H "Authorization: Bearer $SLACK_BOT_TOKEN"
```

## Examples

Send:

```bash
curl -s -X POST https://slack.com/api/chat.postMessage \
  -H "Authorization: Bearer $SLACK_BOT_TOKEN" -H "Content-Type: application/json" \
  -d '{"channel":"C123","text":"Hello"}'
```

Read history:

```bash
curl -s "https://slack.com/api/conversations.history?channel=C123&limit=20" \
  -H "Authorization: Bearer $SLACK_BOT_TOKEN"
```

## Method index

| Action | Method | Body / params |
|---|---|---|
| Send | `chat.postMessage` | `channel`, `text` (+ `thread_ts` to reply) |
| Ephemeral | `chat.postEphemeral` | `channel`, `user`, `text` |
| Edit | `chat.update` | `channel`, `ts`, `text` |
| Delete | `chat.delete` | `channel`, `ts` |
| React / unreact | `reactions.add` / `reactions.remove` | `channel`, `timestamp`, `name` |
| List reactions | `reactions.get` | `channel`, `timestamp`, `full=true` |
| Pin / unpin | `pins.add` / `pins.remove` | `channel`, `timestamp` |
| List pins | `pins.list` | `channel` |
| Thread replies | `conversations.replies` | `channel`, `ts`, `limit` |
| Channel info / list | `conversations.info` / `conversations.list` | `channel` / `types=public_channel,private_channel` |
| User info | `users.info` | `user` |
