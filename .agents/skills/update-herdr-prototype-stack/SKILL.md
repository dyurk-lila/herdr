---
name: update-herdr-prototype-stack
description: Refresh the two-layer Herdr prototype PR stack against live upstream, preserving prediction and disconnected scratchpads. Use for updates to the dyurk-lila/herdr fork.
---

# Update the Herdr prototype stack

Read [the maintenance contract](../../../docs/prototype-maintenance.md) before
resolving input, transport or rendering conflicts. Work only in the requested
fork; discover current PR numbers, branches and stack membership with `gh`.

The ordered branches are `main` -> `dyurk/predictive-typing` ->
`dyurk/reconnect-scratchpad`. The historical `dyurk/remote-input-proto` branch
is a backup, not the update target. The old stable-release updater is retired.

- Fetch live upstream and fork refs. Upstream's default branch is currently
  `master`; resolve its HEAD rather than assuming a local ref or latest tag.
  Fast-forward fork `main` to that exact commit. If it has divergent local
  changes, preserve them and resolve the intended base instead of forcing it.
- Preserve pre-update feature heads. Use a clean isolated checkout, adopt the
  existing stack with `gh stack checkout`, and rebase bottom to top using
  `gh stack rebase --remote origin`. Do not mix scratchpad modules/config into
  the prediction layer. Preserve the default-on, client-local Settings → Typing
  switch, shared predictor profiles and frozen Gen1 codecs.
- Validate each layer independently with the repository's checks and relevant
  smoke harnesses. Record missing SDKs or unavailable CI honestly. Use disposable
  remote sessions; never submit model prompts as test traffic.
- When publishing the requested update, keep both heads/PRs on `dyurk-lila/herdr`,
  preserve the base chain, and use `gh stack submit` or `gh stack link` to maintain
  native membership. Verify ordering, PR bases and `needsRebase` with
  `gh stack view --json`. Use lease protection for rebased feature branches;
  never force `main` or automatically merge the PRs.

Keep docs and PR descriptions aligned with the final behavior. Install/rebuild
`herdr-proto` only when the user asks for a local update; preserve running clients,
volatile drafts and remote sessions. Official compatible Herdr remains sufficient
on remotes; retain the prototype's official-stable bootstrap policy.
