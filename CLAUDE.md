# Claude Code in this repository

@AGENTS.md

## Specific to Claude Code

- **Shared state lives in the repository, not in memory.** Your
  auto-memory is private to one user's sessions; other sessions and
  people don't see it. What another session needs to know (what is in
  flight, a decision, a host's state, a known flake) goes in
  [spec/progress.md](spec/progress.md) or the area's spec, in a PR.
  Memory is for how *you* work with this user.
- **At the start of a session**: read [spec/progress.md](spec/progress.md),
  then `git fetch origin main`, `gh pr list` and `git worktree list`, to
  see what has moved and who is working where. Create your worktree
  before editing anything.
- **Long runs in the background.** Workspace tests, clippy, release
  builds, installs and the UI e2e take minutes to an hour: start them
  with `run_in_background` and wait for the notification. Stop one with
  its task id (TaskStop), never by process name.
- **Delegate broad reading.** To survey many files (every spec's
  milestones, every caller of a function), give it to a subagent and
  keep its conclusion; read the few files you will edit yourself.
- **Ask, don't guess, on decisions that are the user's**: a spec's open
  questions, anything on a host, merging. One question with
  recommended options (AskUserQuestion) beats a paragraph of choices.
- **Commits and PRs**: end them with the attribution lines your harness
  gives you; don't name the model in a commit, a PR or a code comment.
