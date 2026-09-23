# Git

## Commits
- Never add co-author trailers, attribution lines, or "Generated with" footers to commit messages.
- Follow the git-scm commit message convention:
  - Subject line: imperative mood, capitalized, no trailing period, 50 characters or fewer.
  - One blank line between subject and body.
  - Body wrapped at 72 characters.
- The subject states what the change does.
- The body explains why the change was made: motivation, context, the problem being solved.
- The body does not restate what changed or describe how it was implemented. The diff shows that.
- Commit messages are not documentation. Implementation detail belongs in code comments or docs.

## Pushing
- Never run `git push` unless the user explicitly asks for a push in the current request.
- A request to commit is not a request to push.
