# Subscriptions

A **subscription** lets something outside a conversation wake Borg: a Sentry
issue, a Datadog alert, a GitHub pull request, a Slack message, an email, or
Borg's own goals and tasks. It is four parts:

- **Source**: where events come from (a preset such as Sentry, or a custom webhook).
- **Filter**: which events matter (kinds, severity, phrase, repository, …).
- **Instruction**: what Borg does with a matching event.
- **Delivery**: where the result goes. It always lands in the subscription's
  own conversation, and optionally also in a channel.

When a matching event arrives, Borg runs a normal tool-using turn in the
subscription's conversation (`subscription:<id>`) with the instruction and the
event, then reports. You can open that conversation and keep talking.

```
 webhook / email ─┐                         ┌─ dispatcher (borgd)
 channel tap ─────┼─▶ subscription_events ──┤   filter → digest → budget
 Borg events ─────┘    (durable queue)      └─▶ turn in subscription:<id> → delivery
```

The queue is a table, so events survive a restart. Producers store the event
and wake the dispatcher; the payload never crosses a lossy channel.

## Create one

- **App.** Connections → Subscriptions → *Add subscription*. Pick a source,
  narrow the filter, write the instruction, then *Create*. The card shows the
  endpoint URL, the key and the exact steps for the sender, and it changes to
  "Got it" when the first event arrives. Click *Enable* to start.
- **Chat.** Ask Borg: "wake up when Sentry reports a new error in checkout and
  find the commit that caused it". The `subscribe` tool creates it and gives
  you the URL, key and steps.
- **Terminal.** `borg subscriptions [list|pause|resume|events <id>|test <id>]`.

New subscriptions start **paused**, so nothing runs before you are ready.
*Test* sends the source's sample event through the real path, also while
paused, and the result lands in the subscription's conversation.

## Endpoint

Webhook and email sources receive events at:

```
POST <base>/v1/events/<subscription id>
Authorization: Bearer <key>
```

The base is `gateway.public_url` when set, else the running tunnel's address
(`borg tunnel start`), else the local gateway address. A local address is
reachable only from this computer; the app says so and offers to start a
tunnel. A quick tunnel gets a new address each time it starts, so set
`gateway.public_url` or a named tunnel for a source you keep.

| Response | Meaning |
|---|---|
| `202 {"ok":true,"event_id":…}` | Stored; a run follows. |
| `200 {"duplicate":true}` | Already received (provider retry). No second run. |
| `200 {"filtered":true}` | Did not match the filter. Costs nothing. |
| `200 {"accepted":false,…}` | Paused, paused itself, or the queue is full. 200 so the sender does not retry. |
| `200 {"handshake":true}` | Setup check such as GitHub's `ping`. Answered while paused. |
| `401` / `400` | Missing or wrong key or signature / malformed signature header. |
| `404` | No such subscription. |
| `413` | Body over 1 MiB. |
| `429` + `Retry-After: 60` | Over the per-minute burst cap. |
| `503` | Subscriptions are off, or Borg could not read or store. Safe to retry. |

**Authentication.** Borg's key as `Authorization: Bearer <key>` (or the
password of HTTP Basic) works for every source. When the provider's own
signature header is present it must verify; a bad signature is never rescued
by a good key. Providers that issue their own signing secret (Stripe, Sentry,
PagerDuty, Linear, AgentMail) show it only after the endpoint is added there,
so the subscription waits paused until you paste the secret into Borg; from
then on their signature is required. GitHub signs with Borg's key when you
paste the key as the webhook secret.

## Sources

| Preset | Source | Auth | Dedup key | Filter fields |
|---|---|---|---|---|
| Datadog | webhook | bearer (custom header) | `id` | kinds, severity_min |
| Sentry | webhook | `Sentry-Hook-Signature` | `data.event.event_id` | kinds, severity_min |
| PagerDuty | webhook | `X-PagerDuty-Signature` v1 | `event.id` | kinds, severity_min |
| GitHub | webhook | `X-Hub-Signature-256` | body hash | kinds, repos, branch, labels, author, exclude_drafts |
| Linear | webhook | `Linear-Signature` + 60 s age | `webhookId` | kinds, severity_min |
| Stripe | webhook | `Stripe-Signature`, 300 s tolerance | `id` | kinds |
| Postmark | email | bearer / Basic | `MessageID` | phrase |
| AgentMail | email | Svix signature | `event_id` | phrase |
| Slack, Discord, Telegram | channel | the channel's own checks | message id | channel_id, phrase, mention_only, reaction, sender |
| Borg | internal | none | event id | kinds (`goal.completed`, `goal.blocked`, `goal.budget_limited`, `task.failed`, `task.succeeded`, `heartbeat.failed`) |
| Custom webhook | webhook | bearer / Basic | `Idempotency-Key`, `id`, `event_id`, else body hash | kinds, severity_min |

Every preset carries the sender's setup steps, a sample payload for *Test*,
a default instruction, and a one-line summary for each event kind (GitHub:
`PR #42 opened: Fix login (octo/app)`). GitHub kinds are `{event}.{action}`,
and a filter entry matches the kinds under it (`pull_request` matches
`pull_request.opened`). Channel sources see only messages the channel already
admits (Slack allowlist, approved Telegram senders) and never Borg's own
messages.

## Bursts and loops

Designed against 9 000 Sentry alerts in four minutes, and against a run that
causes the events it listens to. In the order an event meets them:

| Guard | Default | Setting |
|---|---|---|
| Burst cap per subscription (429 past it, counted as dropped) | 120 events/min | `subscriptions.ingest_burst` |
| Dedup on the provider's event id or the body hash | on | — |
| Storm breaker: pause itself past this many events in 10 min | 300 | `subscriptions.storm_threshold` |
| Waiting events per subscription (later ones kept as "queue full", not run) | 200 | `subscriptions.max_pending` |
| One run at a time per subscription; waiting events join it as a digest | up to 50 per run | — |
| Runs per subscription per hour (events wait past it) | 60 | `subscriptions.default_rate_limit_per_hour`, per subscription |
| Runs per day, all subscriptions (events wait past it) | 500 | `subscriptions.max_runs_per_day` |
| Runs at once, separate from scheduled tasks | 2 | `subscriptions.max_concurrent` |
| Failure breaker: pause itself after this many failed runs in a row | 5 | `subscriptions.storm_failures` |
| Tool steps per run, sub-agents included | 40 | `subscriptions.max_steps` |
| Run timeout | 300 s | `subscriptions.default_timeout_secs`, per subscription |

Inside a run, Borg cannot create schedules, start goals, or create or change
subscriptions, and neither can its sub-agents; it can still read them. Borg
events born in a subscription's own conversation never wake a subscription.
A reply posted into a channel is Borg's own message, so the tap ignores it.
A paused-itself subscription stays paused until you click *Resume* or say
`subscribe resume`. A run is not retried; providers retry and dedup absorbs it.

The event payload goes into the prompt as untrusted data (cut to 16 KiB), with
an injection warning when the scan finds one. A reply of `[SILENT]` means
nothing needed attention; it is recorded and not delivered.

## Delivery

- **Only here** (default): the subscription's conversation.
- **A channel**: Slack, Telegram or Discord with a target, prefixed with the
  subscription's name.
- **Reply where it came from**: channel sources only. Borg answers in the
  Slack thread (or chat) the message came from. A test has no conversation to
  reply to, so its result stays in the subscription's conversation.

## The `subscribe` tool

Actions: `sources`, `create`, `list`, `get`, `update`, `pause`, `resume`,
`delete`, `events`, `test`. `create` returns the endpoint, the key and the
sender steps. Plan mode allows the read actions only. The tool is present only
while `subscriptions.enabled` is on ("Let subscriptions wake Borg").

## History

Each subscription keeps its newest 50 finished events whole
(`subscriptions.event_retention`). Older events lose their payload and go
after a day; the budgets and breakers still count them until then.

## Security

- The key and any pasted signing secret are stored in `~/.borg/borg.db`, which
  is not encrypted at rest; protection is the `~/.borg` 0700 permissions and
  the sandbox. Revealing or replacing a key is recorded in the activity log,
  and the key is not shown if that record fails.
- An unauthenticated request learns only whether the subscription exists.
- Keep filters narrow. Every match wakes Borg and uses model usage, and
  inbound email is written by strangers.

## Not yet supported

| Source or feature | Why | Until then |
|---|---|---|
| SendGrid Inbound Parse | Multipart body, no JSON | Postmark or AgentMail |
| Mailgun | Three-field signature scheme | Postmark, or custom webhook with a bearer key |
| IMAP polling | No inbound mail reader | Forward to Postmark or AgentMail |
| Microsoft Teams, Google Chat, WhatsApp, iMessage, Signal taps | Not wired into the channel tap | Custom webhook from the platform's own automation |
| Jira, Opsgenie, Grafana, Notion, SMS (Twilio) | No preset | Custom webhook with `Authorization: Bearer <key>` |
| Keychain wrapping of keys | Keys are stored in the database | Rotate a key you think leaked |
| Auto-resume after a storm | Deliberate: a human looks first | Resume in the app or chat |
| Replay of events after they are pruned | History keeps 50 events and drops older payloads | Re-send from the provider |
| Showing the daily cap in the app | Reaching it is only written to `daemon.log` | Check the log when events wait |
| Moving `/v1/webhook/{name}` onto subscriptions | The generic webhook stays as it is | Use a custom-webhook subscription for new senders |
| TUI `/subscriptions` popup | Out of scope for the first pass | `borg subscriptions` |
