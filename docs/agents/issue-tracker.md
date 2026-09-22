# Issue tracker: GitHub

Issues and specs live as GitHub issues, via the `gh` CLI.

- **Create**: `gh issue create --title "..." --body "..."` (heredoc for multi-line).
- **Read**: `gh issue view <n> --comments`.
- **List**: `gh issue list --state open --json number,title,body,labels` plus `--label`/`--state`
  filters.
- **Comment**: `gh issue comment <n> --body "..."`.
- **Label**: `gh issue edit <n> --add-label "..."` / `--remove-label "..."`.
- **Close**: `gh issue close <n> --comment "..."`.

Repo inferred from `git remote -v`; `gh` does this inside a clone.

**PRs as a request surface: no.** _(Flip to `yes` if external PRs count as feature requests; the
triage flow reads this flag. Then use the `gh pr` equivalents.)_

## When a skill says "publish to the issue tracker"

Create a GitHub issue.

## When a skill says "fetch the relevant ticket"

Run `gh issue view <number> --comments`.

## Claiming a ticket

An agent that takes on a ticket assigns itself before any other write: `gh issue edit <n>
--add-assignee @me`. An assigned ticket is claimed; don't start work on one without taking it first.
