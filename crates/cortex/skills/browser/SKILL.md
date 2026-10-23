---
name: browser
description: "Browse the web, interact with pages, take screenshots"
triggers:
  - "browse this site"
  - "click through the page"
  - "take a screenshot"
  - "inspect the web page"
requires:
  bins: ["agent-browser"]
install:
  agent-browser:
    npm: agent-browser
    url: "https://github.com/vercel-labs/agent-browser"
---

# Browser Automation

Drive `agent-browser` via `run_shell`. Snapshot/ref model: `snapshot -i` returns
interactive elements with refs (`@e1`, `@e2`); pass refs to interaction
commands. Refs go stale after navigation or DOM change — re-snapshot.

## Navigation

```bash
agent-browser open <url>
agent-browser back | forward | reload | close
```

## Snapshot

```bash
agent-browser snapshot -i          # interactive elements (default choice)
agent-browser snapshot -s "#main"  # scope to selector
agent-browser snapshot -d 3        # depth cap
```

## Interact (refs from snapshot)

```bash
agent-browser click @e1
agent-browser fill @e2 "text"      # clears then types; prefer over `type`
agent-browser type @e2 "text"      # appends, no clear
agent-browser press Enter
agent-browser hover|check|uncheck @e1
agent-browser select @e1 "value"
agent-browser scroll down 500
agent-browser upload @e1 file.pdf
```

Semantic locators (skip snapshot for obvious targets):

```bash
agent-browser find role button click --name "Submit"
agent-browser find text "Sign In" click
agent-browser find label "Email" fill "user@test.com"
```

## Read

```bash
agent-browser get text|html|value @e1
agent-browser get attr @e1 href
agent-browser get title | url
agent-browser get count ".item"
```

## Capture

```bash
agent-browser screenshot [path.png] [--full]
agent-browser pdf output.pdf
```

## Wait

```bash
agent-browser wait @e1
agent-browser wait 2000
agent-browser wait --text "Success"
agent-browser wait --url "**/dashboard"
agent-browser wait --load networkidle    # use after click/submit before re-snapshot
```

## Auth state

```bash
# capture once
agent-browser state save auth.json
# reuse
agent-browser state load auth.json
```

## Cookies / storage / JS

```bash
agent-browser cookies [set name value | clear]
agent-browser storage local [set k v]
agent-browser eval "document.title"
```

## Example: form submit

```bash
agent-browser open https://example.com/form
agent-browser snapshot -i
agent-browser fill @e1 "user@example.com"
agent-browser fill @e2 "password123"
agent-browser click @e3
agent-browser wait --load networkidle
agent-browser snapshot -i
```

## Recovery

- Stale ref → re-snapshot.
- Timeout → `wait --load networkidle` first; or scope with `snapshot -s`.
- Chrome sandbox failure in containers → set `browser.no_sandbox = true`
  (`borg settings set browser.no_sandbox true`).

Settings live in the `browser.*` namespace (`borg settings get browser`).
`borg doctor` verifies Chrome + `agent-browser` install.
