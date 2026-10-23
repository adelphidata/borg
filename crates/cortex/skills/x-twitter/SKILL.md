---
name: x-twitter
description: "Read and post on X (Twitter) with the official xurl CLI: search posts, read threads, timeline and mentions, then post, reply, quote, like, repost, bookmark, follow, send DMs, and upload media. Use when the user wants real posts to act on, or wants to publish."
category: productivity
triggers:
  - "post this to x"
  - "tweet this"
  - "search twitter for"
  - "check my x mentions"
  - "reply to this tweet"
requires:
  bins: ["xurl"]
install:
  xurl:
    brew: "xdevplatform/tap/xurl"
    npm: "@xdevplatform/xurl"
    url: "https://github.com/xdevplatform/xurl"
---

# X (Twitter)

`xurl` is X's official API client. Every command prints JSON. A post ID or a
full post URL both work, and the `@` on handles is optional. X bills API use;
many errors are plan or credit problems, not bugs.

## Keep secrets out of the chat

- Never read, print, or upload `~/.xurl` (the tokens live there).
- Never pass `--bearer-token`, `--consumer-key`, `--consumer-secret`,
  `--access-token`, `--token-secret`, `--client-id`, or `--client-secret`, and
  never ask the user to paste keys.
- Never use `-v` / `--verbose`: it prints auth headers.
- The only auth check you run is `xurl auth status`.

## One-time setup (the user runs it in their own terminal)

1. Create an app at https://developer.x.com, set the redirect URI to
   `http://localhost:8080/callback`, and copy the Client ID and Secret.
2. `xurl auth apps add my-app --client-id ... --client-secret ...`
3. `xurl auth oauth2 --app my-app` (add `--headless` on a machine with no
   browser; add the handle at the end if X answers `UsernameNotFound`)
4. `xurl auth default my-app`
5. Check with `xurl auth status` and `xurl whoami`.

If `auth status` marks a default app (`▸`) that has no oauth2 token while
another app has one, tell the user to run `xurl auth default <that-app>`.
Leaving `--app` off step 3 is the usual cause.

## Read

```bash
xurl whoami
xurl search "rust async lang:en -is:retweet" -n 10    # 10 to 100 results
xurl read https://x.com/user/status/1234567890
xurl posts @handle -n 20
xurl user @handle
xurl timeline -n 20
xurl mentions -n 20
xurl bookmarks -n 20          # also: likes, followers, following, dms
```

Long-form X Articles: use raw mode with the `article` field, then read
`data.article.plain_text`:

```bash
xurl '/2/tweets/POST_ID?tweet.fields=created_at,public_metrics,article&expansions=author_id'
```

## Write (confirm the text and target with the user first)

```bash
xurl post "Shipped v2 today."
xurl reply POST_ID "Thanks, fixed in main."
xurl quote POST_ID "Worth reading."
xurl delete POST_ID
xurl like POST_ID              # also: unlike, repost, unrepost, bookmark, unbookmark
xurl follow @handle            # also: unfollow, block, unblock, mute, unmute
xurl dm @handle "Hi!"
```

Media: upload (it waits for processing), then attach the returned id.

```bash
xurl media upload photo.jpg
xurl post "Demo" --media-id MEDIA_ID     # repeat --media-id for several
```

Only xurl's own response proves a write happened. Report the post id or URL
from it.

## Anything else: the raw v2 API

```bash
xurl /2/users/me
xurl -X POST /2/tweets -d '{"text":"hello"}'
xurl --app other-app /2/users/me
```

## Errors

| Symptom | Fix |
|---|---|
| 401 on everything | `xurl auth status`; the default app has no token, so `xurl auth default <app>` |
| 403 on one action | The token lacks that scope; the user re-runs `xurl auth oauth2 --app <app>` |
| `unauthorized_client` during OAuth | The app type must be "Web app, automated app or bot" |
| `CreditsDepleted`, `client-not-enrolled` | Billing or plan, in the X developer console |
| 429 | Rate limit; wait. Writes have tighter limits than reads |
