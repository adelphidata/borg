---
name: reddit
description: "Read Reddit without a login or browser: subreddit listings, search across Reddit or one subreddit, a thread with its comments, and a user's recent posts. Use for 'what is r/X saying about Y' or any reddit.com link. Read-only: no posting or voting."
category: utilities
triggers:
  - "what is reddit saying about"
  - "search reddit for"
  - "summarize this reddit thread"
  - "top posts in r/"
  - "find reddit threads on"
requires:
  bins: ["python3"]
install:
  python3:
    brew: python
    apt: python3
    url: "https://www.python.org/downloads/"
---

# Reddit

Reddit answers scripts on its public `.rss` feeds only. The `.json` pages,
`old.reddit.com`, plain `web_fetch`, and the `browser` tool usually get a 403
or a "prove you're human" page, so do not fall back to them or fake a browser
user agent.

The feeds allow about **one request per minute** per IP, and Reddit returns 5
to 25 entries whatever `limit` asks for. Plan calls before you make them: one
`search` inside a subreddit beats several listings, and one thread beats a
whole listing.

## Read

The argument is a reddit.com URL or a path. Paths: `r/NAME/hot` (also `new`,
`rising`, `top?t=week`), `r/NAME/search?q=TERM&restrict_sr=1&sort=new`,
`search?q=TERM&sort=top&t=month`, `user/NAME`, or a full thread URL.

```bash
python3 - "r/LocalLLaMA/search?q=quantization&restrict_sr=1&sort=new" 15 <<'EOF'
import html, re, sys, time, urllib.error, urllib.parse, urllib.request, xml.etree.ElementTree as ET
UA = {"User-Agent": "borg-reader/1.0 (personal assistant)"}
A = "{http://www.w3.org/2005/Atom}"
target, limit = sys.argv[1], int(sys.argv[2]) if len(sys.argv) > 2 else 25
p = urllib.parse.urlsplit(target if "://" in target else "https://www.reddit.com/" + target.lstrip("/"))
path = p.path.rstrip("/")
url = f"https://www.reddit.com{path}{'/.rss' if '/comments/' in path else '.rss'}?" + \
      urllib.parse.urlencode([*urllib.parse.parse_qsl(p.query), ("limit", limit)])

def get(u, retry=True):
    try:
        with urllib.request.urlopen(urllib.request.Request(u, headers=UA), timeout=30) as r:
            return r.read()
    except urllib.error.HTTPError as e:
        if e.code == 429 and retry:      # wait out the one-minute window once
            wait = min(int(float(e.headers.get("x-ratelimit-reset") or 61)) + 1, 90)
            print(f"(rate limited, waiting {wait}s)", file=sys.stderr); time.sleep(wait)
            return get(u, retry=False)
        raise

def clean(s):
    return re.sub(r"\s+", " ", html.unescape(re.sub(r"<[^>]+>", " ", s or ""))).strip()

for i, e in enumerate(ET.fromstring(get(url)).iter(A + "entry")):
    who = (e.findtext(f"{A}author/{A}name") or "").removeprefix("/u/")
    link = e.find(A + "link")
    body = clean(e.findtext(A + "content"))
    if "/comments/" in path and i > 0:
        print(f"  > u/{who}: {body[:1200]}")
    else:
        print(f"- {e.findtext(A + 'updated', '')[:10]}  {clean(e.findtext(A + 'title'))}  (u/{who})\n"
              f"  {link.get('href') if link is not None else ''}\n  {body[:400]}")
EOF
```

A listing shows only the first part of each post. For "what are people
saying", read the threads themselves. A thread feed holds the post plus
top-level comments, with no scores or reply nesting. Cite the thread or comment
link, not the listing.

## More than a few calls: app credentials

For monitoring, or more than about ten calls, ask the user to register a free
"script" app at https://www.reddit.com/prefs/apps. This registers an app; it
does not log Borg into their account. Never ask for a Reddit password or
browser cookies, and never take the secret in chat. The user saves the two
values in their own terminal:

```bash
mkdir -p ~/.borg/reddit && (umask 077; cat > ~/.borg/reddit/app.json)
# paste {"client_id": "...", "client_secret": "..."} then press Ctrl-D
```

Never print that file. With it, Reddit allows about 100 requests a minute and
returns scores and nested comments. Same paths and URLs as above:

```bash
python3 - "r/rust/comments/POST_ID" <<'EOF'
import base64, json, os, sys, urllib.parse, urllib.request
UA = "borg-reader/1.0 (personal assistant)"
app = json.load(open(os.path.expanduser("~/.borg/reddit/app.json")))
basic = base64.b64encode(f"{app['client_id']}:{app['client_secret']}".encode()).decode()
req = urllib.request.Request("https://www.reddit.com/api/v1/access_token", data=b"grant_type=client_credentials",
                             headers={"User-Agent": UA, "Authorization": "Basic " + basic})
bearer = json.load(urllib.request.urlopen(req, timeout=30))["access_token"]
p = urllib.parse.urlsplit(sys.argv[1] if "://" in sys.argv[1] else "https://oauth.reddit.com/" + sys.argv[1].lstrip("/"))
extra = {"depth": 4, "sort": "top"} if "/comments/" in p.path else {}
query = urllib.parse.urlencode({"raw_json": 1, "limit": 50, **extra, **dict(urllib.parse.parse_qsl(p.query))})
req = urllib.request.Request(f"https://oauth.reddit.com{p.path.rstrip('/')}?{query}",
                             headers={"User-Agent": UA, "Authorization": "Bearer " + bearer})
data = json.load(urllib.request.urlopen(req, timeout=30))

def walk(children, depth=0):
    for c in children:
        d = c.get("data", {})
        if c.get("kind") == "t3":
            print(f"- [{d['score']}] {d['title']} ({d['num_comments']} comments)\n"
                  f"  https://www.reddit.com{d['permalink']}\n  {d.get('selftext', '')[:400]}")
        elif c.get("kind") == "t1":
            print(f"{'  ' * depth}> [{d['score']}] u/{d['author']}: {d['body'][:800]}")
            if isinstance(d.get("replies"), dict): walk(d["replies"]["data"]["children"], depth + 1)

for listing in data if isinstance(data, list) else [data]:
    walk(listing["data"]["children"])
EOF
```

## Pitfalls

- A second 429 in a row means the task needs the app credentials. Do not loop
  or use a proxy.
- Archived old threads may still be on the Wayback Machine; fresh ones are not.
