# Contributing to Borgbase

Thanks for your interest in contributing!

## Development Setup

1. Install [Bun](https://bun.sh) 1.x
2. Clone the repository and install dependencies:
   ```sh
   git clone <repository-url> borgbase
   cd borgbase
   bun install
   ```
   `bun install` also installs the git hooks (husky).
3. Run the app in dev mode:
   ```sh
   bun run dev
   ```

## Before Submitting

- `bun run typecheck` must pass
- `bun run format:check` must pass — or just rely on the pre-commit hook, which runs Prettier on staged files via lint-staged

## Pull Requests

- Keep PRs focused on a single change
- Use concise, prefixed commit messages: `fix:`, `feat:`, `update:`, `refactor:`, `chore:`, `docs:`
- Describe what the change does and why

## Questions

Open an issue for bugs and feature discussion. For the Borg agent itself (daemon, CLI, plugins), see the main [Borg repository](https://github.com/adelphidata/borg).
