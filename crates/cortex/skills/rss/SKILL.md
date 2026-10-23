---
name: rss
description: "Read RSS, Atom, and JSON feeds, find the feed behind an ordinary site URL, and build digests of what is new since the last check. Use for blogs, release notes, podcasts, YouTube channels, news, and changelogs, or to follow sites on a schedule."
category: utilities
triggers:
  - "what's new on this blog"
  - "read this rss feed"
  - "does this site have a feed"
  - "latest releases of"
  - "send me a daily digest of"
requires:
  bins: ["python3"]
install:
  python3:
    brew: python
    apt: python3
    url: "https://www.python.org/downloads/"
---

# RSS and Atom feeds

A feed gives a clean, dated list of entries, which is cheaper and steadier
than scraping a front page. Summaries are often cut short: pass an entry's
link to `web_fetch` for the full article. Cite the entry link, not the feed.

## Read a feed (or the site behind it)

Arguments: URL, max entries, and an optional ISO date to drop older entries.
Give it a page URL and it finds the advertised feed, then tries the usual
paths. Standard library only.

```bash
python3 - "https://simonwillison.net/" 10 "2026-10-01" <<'EOF'
import html, json, re, sys, urllib.parse, urllib.request, xml.etree.ElementTree as ET
from datetime import datetime, timezone
from email.utils import parsedate_to_datetime
url, limit, since = sys.argv[1], int(sys.argv[2]), sys.argv[3] if len(sys.argv) > 3 else ""

def get(u):
    with urllib.request.urlopen(urllib.request.Request(u, headers={"User-Agent": "Borg feed reader"}), timeout=20) as r:
        return r.read()

def clean(s):
    return re.sub(r"\s+", " ", html.unescape(re.sub(r"<[^>]+>", " ", s or ""))).strip()

def iso(s):
    if not s: return ""
    try: d = parsedate_to_datetime(s)
    except (TypeError, ValueError):
        try: d = datetime.fromisoformat(s.strip().replace("Z", "+00:00"))
        except ValueError: return ""
    return (d if d.tzinfo else d.replace(tzinfo=timezone.utc)).astimezone(timezone.utc).isoformat()

def is_feed(b):
    h = b.lstrip()[:500].lower()
    return any(t in h for t in (b"<rss", b"<feed", b"<rdf")) or (h[:1] == b"{" and b'"items"' in b)

data = get(url)
if not is_feed(data):
    page = data.decode("utf-8", "replace")
    found = [urllib.parse.urljoin(url, html.unescape(h)) for tag in re.findall(r"<link\b[^>]*>", page, re.I)
             if re.search(r"application/(rss|atom|feed)\+(xml|json)", tag, re.I)
             for h in re.findall(r"href=[\"']([^\"']+)", tag)]
    base = "{0.scheme}://{0.netloc}".format(urllib.parse.urlsplit(url))
    for cand in found + [base + p for p in ("/feed", "/rss.xml", "/atom.xml", "/feed.xml", "/index.xml", "/rss")]:
        try: data = get(cand)
        except Exception: continue
        if is_feed(data): url = cand; break
    else: sys.exit(f"no feed found at {url}")

items = []
if data.lstrip()[:1] == b"{":
    for i in json.loads(data).get("items", []):
        items.append((iso(i.get("date_published")), clean(i.get("title")), i.get("url") or "",
                      clean(i.get("summary") or i.get("content_text") or i.get("content_html"))))
else:
    for e in ET.fromstring(data).iter():
        if e.tag.rsplit("}", 1)[-1] not in ("item", "entry"): continue
        kids = {}
        for c in e: kids.setdefault(c.tag.rsplit("}", 1)[-1], c)
        text = lambda *names: next((kids[n].text for n in names if n in kids and kids[n].text), "")
        links = [c for c in e if c.tag.rsplit("}", 1)[-1] == "link"]
        link = next((l.get("href") for l in links if l.get("href") and l.get("rel", "alternate") == "alternate"), "") \
            or next((l.text.strip() for l in links if l.text), "")
        items.append((iso(text("published", "pubDate", "updated", "date")), clean(text("title")), link,
                      clean(text("summary", "description", "encoded", "content"))))
items = sorted((i for i in items if not since or i[0] >= since), reverse=True)[:limit]
print(f"feed: {url}  ({len(items)} entries)")
for when, title, link, summary in items:
    print(f"- {when[:16]}  {title}\n  {link}\n  {summary[:280]}")
EOF
```

## Known feed URLs

| Source | Feed |
|---|---|
| GitHub releases, tags, commits | `github.com/OWNER/REPO/releases.atom`, `.../tags.atom`, `.../commits/BRANCH.atom` |
| YouTube channel | `youtube.com/feeds/videos.xml?channel_id=UC...` |
| Hacker News | `hnrss.org/frontpage`, `hnrss.org/newest?q=TERM` |
| Subreddit | `reddit.com/r/NAME/.rss` (about one request a minute; see the `reddit` skill) |
| arXiv category | `rss.arxiv.org/rss/cs.CL` |
| Substack, WordPress, Ghost | `SITE/feed`; Medium: `medium.com/feed/@user` |

## Digests on a schedule

Keep followed feeds in `~/.borg/rss/feeds.json`:

```json
{"feeds": [{"name": "Rust blog", "url": "https://blog.rust-lang.org/feed.xml", "last_seen": "2026-10-20T00:00:00+00:00"}]}
```

Each run reads every feed with `last_seen` as the since date, reports only new
entries (grouped by feed, newest first, one line each), then sets `last_seen`
to the newest date it saw. Stay silent when nothing is new. Create the job
with `schedule`, for example daily at 8am:

```json
{"action": "create", "type": "prompt", "name": "Morning feed digest",
 "prompt": "Use the rss skill to send a digest of new entries from ~/.borg/rss/feeds.json.",
 "schedule_type": "cron", "schedule_expr": "0 8 * * *", "delivery_channel": "origin"}
```

## Pitfalls

- An HTML page with no `<link rel="alternate">` and none of the usual paths
  gives "no feed found". Check the site footer or `/sitemap.xml` before you
  decide it has no feed.
- Entries without a date sort last and are dropped by a since date.
- Some sites return 403 to scripts. Try `web_fetch`, or the `browser` tool.
