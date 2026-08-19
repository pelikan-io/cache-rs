# Engineering Journal Project Profile

Filled from observed cache-rs conventions. Sources cited per section.

## Repository Convention

- Project name: `cache-rs`
- Journal path and filename pattern: `docs/journal/YYYY-MM-DD-short-slug.md`
- Index path: `docs/journal/README.md`
- Index shape and required columns: table — date, entry link, title, status
- Repository guidance sources: `CLAUDE.md`, `docs/design.md`, `docs/diagrams/README.md`

## Entry Schema

- Required frontmatter: `status`, `opened`, `updated`
- Optional frontmatter: `beta_skills` (when the skill-feedback policy applies), `superseded_by` (with `status: superseded`)
- Required headings: Goal, Scope, Evidence, Design and Implementation, Outcome, Deferred or Reopen Items; Decision Criteria / Derived Documents / Skill Feedback / Appendix: Skills Invoked when applicable
- Fixed lifecycle states: `open`, `shipped`, `no-go`, `superseded`
- Lifecycle extensions: none
- Date and supersession rules: template defaults (opened <= updated; supersession requires an existing target entry)

## Operating Policy

- Operating-mode preference: decide per effort; default single-PR
- Intent-first landing requirement: land the open entry in its own PR before implementation when cross-repo coordination matters (e.g. work paired with pelikan)
- Trivial-change threshold: single-file mechanical changes, lint/format fixes, version bumps, and doc typo fixes are not journaled
- Evidence requirements: commit SHAs, PR links, and exact commands with results (`cargo test -p <crate>`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, fuzz run counts, `cargo kani` verdicts, criterion output when performance is claimed)
- Skill-feedback policy: records skills invoked and beta-skill friction/confirmation (pull model: skills-mcp surveys consumer-repo journals; nothing is pushed upstream)
- Skills this project treats as beta: per self-declaration (currently `review-guide`, `architecture-diagram`); none added locally

## Durable Derived Documents

- Backlog: none
- Roadmap: none
- Assumptions and limitations: none
- Canonical facts, datasets, bibliographies, or other records: `docs/ziplist.md` and `docs/diagrams/` (format reference and generated figures; synchronized whenever the frozen format or its fixtures change)
- Derived-document policy: update `docs/ziplist.md` and regenerate `docs/diagrams/ziplist-block-anatomy.svg` in the same change as any format-affecting outcome

## Validation and Reconciliation

- Validation commands: `cargo test --workspace`; `cargo run -p ziplist --example block_anatomy | diff - docs/diagrams/ziplist-block-anatomy.svg` for figure freshness
- Link or index checks: manual — entry links in `docs/journal/README.md` resolve
- Reconciliation boundaries: `docs/journal/**` only
- Judgment changes requiring review: status transitions, supersessions, and any edit outside `docs/journal/`
- Advisory brief reporting policy: report candidates only; brief creation happens in the knowledge-iop vault at the user's request, never from reconciliation

## Profile Evidence

- Filled by and date: Claude (session with Yao Yue), 2026-08-18
- Evidence inspected: `CLAUDE.md`, `docs/` tree, `.claude/commands/`, git log of `feature/ziplist-codec`, pelikan's `.agent/skills` convention as sibling precedent
- Unresolved conflicts or unknowns: none
