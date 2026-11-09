# Architecture decision records

An ADR records one architectural decision: the problem, the options we weighed, what we chose and what it costs us. It is written before the work, so the reasons survive after the code changes.

Format (Nygard / MADR, kept short):

- **Status:** Proposed, Accepted, Deferred, Superseded by NNNN, or Rejected, with a date.
- **Context:** the problem and the forces at play.
- **Decision drivers:** what a good answer must satisfy.
- **Considered options:** each with its pros and cons.
- **Decision outcome:** the choice and why.
- **Consequences:** the good, the bad, and the risks.
- **Open questions** and **References**.

Files are numbered `NNNN-short-title.md`. An accepted ADR is not edited; a later ADR supersedes it. An implementation plan, if any, lives in `docs/plans/` and links back.

| ADR | Title | Status |
|---|---|---|
| [0001](0001-map-projections.md) | Map projections beyond Mercator and Globe | Proposed, deferred (2026-10-18) |
