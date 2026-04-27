---
name: github
description: "GitHub issues, PRs, CI, and code review via gh CLI"
requires:
  bins: ["gh"]
---

# GitHub Skill

Use the `gh` CLI to interact with GitHub repositories, issues, PRs, and CI.

## Setup

```bash
gh auth login    # one-time authentication
gh auth status   # verify
```

## Pull Requests

```bash
gh pr list --repo owner/repo
gh pr view 55 --repo owner/repo
gh pr checks 55 --repo owner/repo
gh pr create --title "feat: add feature" --body "Description"
gh pr merge 55 --squash --repo owner/repo
```

## Issues

```bash
gh issue list --repo owner/repo --state open
gh issue create --title "Bug: something broken" --body "Details..."
gh issue close 42 --repo owner/repo
```

## CI / Workflow Runs

```bash
gh run list --repo owner/repo --limit 10
gh run view <run-id> --repo owner/repo
gh run view <run-id> --repo owner/repo --log-failed
gh run rerun <run-id> --failed --repo owner/repo
```

## API Queries

```bash
gh api repos/owner/repo/pulls/55 --jq '.title, .state, .user.login'
gh api repos/owner/repo/labels --jq '.[].name'
```

## JSON Output

Most commands support `--json` with `--jq` filtering:

```bash
gh pr list --json number,title,state --jq '.[] | "\(.number): \(.title)"'
gh issue list --json number,title,labels --jq '.[] | "[\(.number)] \(.title)"'
```

## Notes

- Always specify `--repo owner/repo` when not in a git directory
- Use URLs directly: `gh pr view https://github.com/owner/repo/pull/55`
- Use `gh api --cache 1h` for repeated queries to avoid rate limits
- For local git operations (commit, push, pull), use `git` directly
