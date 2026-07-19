# CLAUDE.md

Guidance for AI assistants working in this repository.

## Comment discipline — describe the contract, not the narrative

Applies to in-source comments and doc comments, and to standalone docs in this repo.

**Describe what the code currently does and why it must be that way** — the invariants it upholds, the ordering it relies on, and the inputs, outputs, and error modes callers must respect. The reader is someone understanding the code as it is now, not reconstructing how it got here.

**Never** bake transient development context into long-lived comments or docs:

- No change narration — "previously did X, now does Y", "the old behaviour was…", "this used to…", "no longer gated".
- No mention of a bug or behaviour that has since been fixed. Once a fix lands, drop the mention and describe the correct behaviour.
- No references to a specific PR, review comment, branch, ticket, or commit — the git log already records that; readers don't have it loaded.
- No one-off empirical numbers from a single run (e.g. "37/42 entries were stale") — those belong in the commit message that introduced the change.
- No in-flight refactoring scaffolding ("for now…", "until the X migration lands…"). If it's the current behaviour, document that; if it's a genuine temporary, leave a TODO with a tracking link and document the contract the temporary upholds.

The right home for change/PR/incident narrative is the **commit message** or **PR description**. A comment should read the same six months and ten unrelated PRs later as it does today. When you touch a comment that already drifts into narrative, rewrite it into contract form rather than appending another layer.
