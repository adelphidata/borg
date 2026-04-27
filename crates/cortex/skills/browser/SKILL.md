---
name: browser
description: "Browse the web, interact with pages, take screenshots"
requires:
  bins: ["agent-browser"]
---

# Browser Automation

Browse the web using `run_shell` with `agent-browser` commands. Uses a snapshot/ref model — take a snapshot to get element refs (@e1, @e2), then interact using those refs.

## Core Workflow

1. Navigate: `agent-browser open <url>`
2. Snapshot: `agent-browser snapshot -i` (returns elements with refs like `@e1`, `@e2`)
3. Interact using refs from the snapshot
4. Re-snapshot after navigation or significant DOM changes

## Commands

### Navigation

```bash
agent-browser open <url>      # Navigate to URL
agent-browser back            # Go back
agent-browser forward         # Go forward
agent-browser reload          # Reload page
agent-browser close           # Close browser
```

### Snapshot (page analysis)

```bash
agent-browser snapshot            # Full accessibility tree
agent-browser snapshot -i         # Interactive elements only (recommended)
agent-browser snapshot -c         # Compact output
agent-browser snapshot -d 3       # Limit depth to 3
agent-browser snapshot -s "#main" # Scope to CSS selector
```

### Interactions (use @refs from snapshot)

```bash
agent-browser click @e1           # Click
agent-browser dblclick @e1        # Double-click
agent-browser fill @e2 "text"     # Clear and type
agent-browser type @e2 "text"     # Type without clearing
agent-browser press Enter         # Press key
agent-browser hover @e1           # Hover
agent-browser check @e1           # Check checkbox
agent-browser uncheck @e1         # Uncheck checkbox
agent-browser select @e1 "value"  # Select dropdown option
agent-browser scroll down 500     # Scroll page
agent-browser upload @e1 file.pdf # Upload files
```

### Get information

```bash
agent-browser get text @e1        # Get element text
agent-browser get html @e1        # Get innerHTML
agent-browser get value @e1       # Get input value
agent-browser get attr @e1 href   # Get attribute
agent-browser get title           # Get page title
agent-browser get url             # Get current URL
agent-browser get count ".item"   # Count matching elements
```

### Screenshots & PDF

```bash
agent-browser screenshot          # Save to temp directory
agent-browser screenshot path.png # Save to specific path
agent-browser screenshot --full   # Full page
agent-browser pdf output.pdf      # Save as PDF
```

### Wait

```bash
agent-browser wait @e1                     # Wait for element
agent-browser wait 2000                    # Wait milliseconds
agent-browser wait --text "Success"        # Wait for text
agent-browser wait --url "**/dashboard"    # Wait for URL pattern
agent-browser wait --load networkidle      # Wait for network idle
```

### Semantic locators (alternative to refs)

```bash
agent-browser find role button click --name "Submit"
agent-browser find text "Sign In" click
agent-browser find label "Email" fill "user@test.com"
agent-browser find placeholder "Search" type "query"
```

### Authentication with saved state

```bash
# Login once
agent-browser open https://app.example.com/login
agent-browser snapshot -i
agent-browser fill @e1 "username"
agent-browser fill @e2 "password"
agent-browser click @e3
agent-browser wait --url "**/dashboard"
agent-browser state save auth.json

# Later: load saved state
agent-browser state load auth.json
agent-browser open https://app.example.com/dashboard
```

### Cookies & Storage

```bash
agent-browser cookies                     # Get all cookies
agent-browser cookies set name value      # Set cookie
agent-browser cookies clear               # Clear cookies
agent-browser storage local               # Get localStorage
agent-browser storage local set k v       # Set value
```

### JavaScript

```bash
agent-browser eval "document.title"   # Run JavaScript
```

## Example: Form submission

```bash
agent-browser open https://example.com/form
agent-browser snapshot -i
# Output shows: textbox "Email" [ref=e1], textbox "Password" [ref=e2], button "Submit" [ref=e3]

agent-browser fill @e1 "user@example.com"
agent-browser fill @e2 "password123"
agent-browser click @e3
agent-browser wait --load networkidle
agent-browser snapshot -i  # Check result
```

## Configuration

The `[browser]` section in `~/.borg/config.toml` controls browser automation behavior:

```toml
[browser]
enabled = true              # Enable/disable browser automation
headless = true             # Run headless (no visible window)
executable = "/path/to/chrome"  # Optional: override auto-detected Chrome path
cdp_port = 9222             # Chrome DevTools Protocol port
no_sandbox = false          # Disable Chrome sandboxing (use with caution)
timeout_ms = 30000          # Default command timeout
startup_timeout_ms = 15000  # Browser launch timeout
```

Run `borg doctor` to verify Chrome detection and `agent-browser` installation status.

## Error Recovery

**Stale element refs:** After navigation or DOM changes, refs (e.g. `@e1`) become stale. Always re-snapshot after clicking links, submitting forms, or waiting for dynamic content.

**Timeouts:** If a command times out, try:
1. `agent-browser wait --load networkidle` before interacting
2. Increase timeout via config (`browser.timeout_ms`)
3. Break complex pages into scoped snapshots: `agent-browser snapshot -s "#main-content"`

**Page loading issues:** Some SPAs need extra time. Use `agent-browser wait --url "**/expected-path"` or `agent-browser wait --text "Expected Content"` to confirm navigation completed.

**Sandbox errors:** If Chrome fails to launch with sandbox errors (common in containers), set `browser.no_sandbox = true` in config.

## Best Practices

- **Snapshot before interact:** Always take a fresh snapshot before interacting with elements. Refs are only valid for the current page state.
- **Use `-i` flag:** Prefer `agent-browser snapshot -i` (interactive elements only) over full snapshots — it's faster and produces less noise.
- **Wait for navigation:** After clicking links or submitting forms, wait for the page to settle before re-snapshotting: `agent-browser wait --load networkidle`.
- **Save auth state:** For authenticated workflows, save browser state with `agent-browser state save auth.json` and reload it in future sessions to avoid repeated logins.
- **Prefer `fill` over `type`:** Use `fill` for form fields — it clears existing content first. Use `type` only when you need to append to existing text.
- **Scope large pages:** For complex pages, use `agent-browser snapshot -s "CSS_SELECTOR"` to focus on the relevant section.

## Example: Data extraction

```bash
agent-browser open https://example.com/products
agent-browser snapshot -i
agent-browser get text @e1  # Get product title
agent-browser get attr @e2 href  # Get link URL
agent-browser screenshot products.png
```
