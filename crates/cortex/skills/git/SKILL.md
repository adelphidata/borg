---
name: git
description: "Git operations: commit, branch, diff, log, rebase"
requires:
  bins: ["git"]
---

# Git Skill

Use `git` for local version control operations.

## Status & Diff

```bash
git status
git diff                          # unstaged changes
git diff --staged                 # staged changes
git diff HEAD~3..HEAD             # last 3 commits
git diff main..feature-branch     # branch comparison
```

## Commits

```bash
git add -p                        # interactive staging
git add file1.rs file2.rs
git commit -m "feat: add feature"
git commit --amend --no-edit      # amend last commit (keep message)
```

## Log & History

```bash
git log --oneline -20
git log --oneline --graph --all
git log --since="2 days ago" --oneline
git log --author="name" --oneline
git show HEAD                     # last commit details
git blame file.rs
```

## Branches

```bash
git branch -a                     # list all branches
git switch -c new-branch          # create and switch
git switch main
git branch -d old-branch          # delete merged branch
git merge feature-branch
```

## Stash

```bash
git stash                         # stash working changes
git stash list
git stash pop                     # apply and remove latest stash
git stash apply stash@{1}         # apply specific stash
```

## Rebase

```bash
git rebase main                   # rebase current branch onto main
git rebase --continue             # after resolving conflicts
git rebase --abort                # cancel rebase
```

## Notes

- Use `git status` before committing to verify what will be included
- Prefer `git switch` over `git checkout` for branch operations
- Use `git log --oneline` for compact history summaries
- When conflicts arise, use `git diff` to inspect and resolve them
