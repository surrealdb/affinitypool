# CLAUDE.md

Guidance for AI assistants working in this repository.

## Comment discipline

Code comments must describe the code as it currently stands, not the development history that produced it. Do not narrate changes ("this now does X", "previously returned Y", "no longer gated") or reference bugs or behaviours that have since been fixed — once a fix has landed, drop the mention and describe the correct behaviour. Genuinely open limitations may be noted, but neutrally: no dates, no "found while…", no PR/issue references, no root-cause internals. The rationale for a change belongs in the commit message or PR description, not inline.
