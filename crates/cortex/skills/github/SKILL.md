---
name: github
description: "GitHub issues, PRs, CI, and code review via gh CLI"
triggers:
  - "github issue"
  - "pull request"
  - "open a pr"
  - "review this github change"
requires:
  bins: ["gh"]
install:
  gh:
    brew: gh
    apt: gh
    url: "https://cli.github.com"
---

# GitHub

`gh` CLI. Add `--repo owner/repo` outside a clone, or pass a URL directly
(`gh pr view https://github.com/owner/repo/pull/55`). `--cache 1h` on
`gh api` to avoid rate limits. Use `git` for local commit/push/pull.

## PRs

```bash
gh pr list
gh pr view 55
gh pr checks 55
gh pr create --title "feat: add feature" --body "…"
gh pr merge 55 --squash
```

## Issues

```bash
gh issue list --state open
gh issue create --title "Bug: …" --body "…"
gh issue close 42
```

## CI runs

```bash
gh run list --limit 10
gh run view <id> [--log-failed]
gh run rerun <id> --failed
```

## API + jq

```bash
gh api repos/owner/repo/pulls/55 --jq '.title, .state, .user.login'
gh pr list --json number,title,state --jq '.[] | "\(.number): \(.title)"'
```
