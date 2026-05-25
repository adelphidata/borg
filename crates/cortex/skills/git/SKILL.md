---
name: git
description: "Git operations: commit, branch, diff, log, rebase"
triggers:
  - "git commit"
  - "git branch"
  - "show me the diff"
  - "inspect git history"
requires:
  bins: ["git"]
---

# Git

## Status / diff

```bash
git status
git diff [--staged]
git diff HEAD~3..HEAD
git diff main..feature
```

## Commit

```bash
git add -p                                    # interactive
git add file1 file2
git commit -m "feat: add feature"
git commit --amend --no-edit
```

## Log

```bash
git log --oneline -20
git log --oneline --graph --all
git log --since="2 days ago" --author="name" --oneline
git show HEAD
git blame file.rs
```

## Branch

```bash
git branch -a
git switch -c new-branch                       # prefer over `checkout -b`
git switch main
git branch -d old-branch
git merge feature
```

## Stash

```bash
git stash
git stash list
git stash pop                                  # latest
git stash apply stash@{1}
```

## Rebase

```bash
git rebase main
git rebase --continue|--abort                  # after conflict resolution
```
