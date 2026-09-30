# Maintaining the remote-input prototype

The custom fork is `dyurk-lila/herdr`; its prototype branch is
`dyurk/remote-input-proto`. Stable upstream releases come from `herdrdev/herdr`.
The fork's default branch should be the prototype branch.

The [feature guide](remote-input-prototype.md) describes setup and measured
behavior; the [editing design](remote-editing-design.md) explains reconciliation,
learning and prior art. This document is the acceptance contract for upstream
updates, including updates that merge cleanly.

## Features and constraints to preserve

| Area | Required behavior after an update |
|---|---|
| Composed edits | Append/middle insertion, Backspace/Delete, Left/Right/End, learned word deletion and eligible clicks compose in input order while earlier echoes are pending. Preserve both hardware and software caret rendering and exact original UTF-8 input. |
| Delivery and reconciliation | Forward every original event once, independently of prediction. Keep authoritative frames unchanged; retire only the earliest exact row-and-cursor matching queue prefix. An unchanged screen cannot acknowledge cancelling edits. No retries or replay inferred from screen equality. |
| Context and expiry | Preserve machine, connection generation, server boot, pane, geometry and live-input-bound fencing. Keep the 256-edit limit, 750 ms visible expiry and bounded three-second invisible late-echo recovery. New input/context discards unresolved expired history; late echoes must never resurrect visible predictions. |
| Conservative scope | Require agreement between Ghostty and Unicode width plus surrounding grapheme boundaries for single-cell insertion. Wide/combining/ZWJ text, wrapping, Home, selection, paste and unsupported operations retain authoritative rendering. Learn Ctrl-W and Alt-Backspace separately; predict only when surviving word-boundary hypotheses agree. |
| Mouse routing | Learn only forwarded, same-row single left clicks with application mouse reporting and an observable hardware cursor. Preserve local selection/drag behavior and modifier, pixel and multiple-click fallbacks. The first unknown click is authoritative. |
| Message/session warmth | Reuse exact learned prompt fingerprints across messages, named sessions and client restarts for the same SSH destination and canonical agent. Derive bounds from the current frame; never restore saved draft/cursor state. Different aliases and agents remain separate identities. |
| Agent changes | Activity/readiness labels, including unknown, are advisory. Do not depend on pinned CLI versions, internal editor APIs or daemon readiness RPCs. New layouts require evidence; contradictory echoes invalidate shared learning, including contradictions arriving after visible expiry, then permit relearning. |
| Shared persistence | Store only destination/prompt hashes, agent labels and bounded behavior rules. Preserve private permissions, atomic replacement, cross-client locking and bounded background work. Epochs fence stale observations; concurrent invalidations must both survive. Overflow pauses reuse until a newer reset is observed. Corrupt/unknown schemas remain untrained; an incompatible schema needs an explicit migration or reset. |
| Performance | Keep prediction state pure and the cached rendering path free of filesystem I/O, waits and per-render hashing. Preserve hidden-pane/retained-render exits. Changes to pane-scaled work require fixed-geometry measurements with one and at least 15 populated panes. |
| Compatibility and isolation | Keep opt-in prediction and separate `herdr-proto` config/state/catalog/socket paths, including direct and saved-machine routing. Keep stable remote interoperability and connection recovery. Do not change frozen generation-one codecs, enum variants, fixtures or method meanings to make new features pass. |

Reliable offline buffering is **not** part of this contract. Disconnected input
is currently discarded. [OFFLINE_TYPING.md](../OFFLINE_TYPING.md) describes the
required negotiated input IDs, acceptance ACKs, deduplication and context fences.
SSH/QUIC transport delivery or keeping the predictor alive cannot substitute for
those server guarantees. Do not silently enable replay against an older server.

## Implementation and regression map

- `src/client/shell/prediction.rs`: line projection, ordered reconciliation,
  expiry, prompt reuse and context fencing.
- `prediction_input.rs`, `prediction_words.rs` and shell input routing: event
  classification, separate learned deletion rules and conservative fallbacks.
- `prediction_profiles.rs`: pure shared profile/epoch model;
  `src/client/prediction_store.rs`: bounded persistence worker and private storage.
- `src/config/io.rs` and `src/remote/args.rs`: prototype namespace and stable
  destination identity for direct and saved connections.
- `src/client/shell/tests/prediction.rs`, `prediction_sequence_tests.rs` and
  `tests/prediction_mouse.rs`: exact-prefix/cancelling sequences, software carets,
  Unicode, mixed clicks/edits, first-character warmth, late contradictions and
  changed word semantics followed by relearning. Profile/store tests protect
  concurrent invalidation, stale refresh, overflow and corruption behavior.
- Existing frozen endpoint codec/digest/method fixtures and client reconnect tests
  remain compatibility tests; never rewrite their expectations to bless drift.

## Enable the updater

Enable GitHub Actions on the fork. Under **Settings → Actions → General**, enable
**Allow GitHub Actions to create and approve pull requests**. Default token
permissions can remain read-only; the updater explicitly requests contents and pull-request write
permissions and authenticates with the repository's `GITHUB_TOKEN`.

The workflow file must exist on the default branch: GitHub runs scheduled
workflows there, and requires it there to register `workflow_dispatch`. It runs
weekly on Monday at 14:23 UTC. Public repository schedules may be disabled after
60 days without repository activity; re-enable the workflow if that happens. See
[GitHub's scheduled workflow rules](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#schedule).

To check immediately:

```sh
gh workflow run prototype-upstream.yml --repo dyurk-lila/herdr \
  --ref dyurk/remote-input-proto
```

## Review an update

The updater verifies the latest published stable release's tag object and exact
commit, then merges it into `prototype/update-<tag>` from the prototype branch.
Reruns reuse that branch and its open PR, preserving existing commits. Conflicts
stop the run with a summary before any push. Moving release/base refs also stop
the run. The workflow never force-pushes, updates the prototype branch directly,
or merges a PR.

Review the PR **into `dyurk/remote-input-proto`**. A clean Git merge is not runtime
validation. Approve pending CI runs, check the platform results, and rerun the
remote typing/reconnect smoke tests before merging. GitHub currently places CI
triggered by `GITHUB_TOKEN`-created/updated PRs into an approval-required state;
do not assume checks ran because the updater passed. See
[GitHub's token trigger rules](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow).

The fork's CI also runs on direct pushes to the prototype branch and supports
manual dispatch. It validates Linux, macOS and Windows; the updater itself only
prepares a merge and PR.

If a merge conflicts, resolve it locally on the update branch using the exact
release commit shown in the run summary, validate it, push that branch, and rerun
the updater. The updater is scoped to this fork; it opens no upstream PRs.

Before accepting an update:

1. Review the overlay against the contract above. Keep the regression cases even
   if upstream refactors move or replace their implementation. Update the feature
   guide's current stable base and the design's changed assumptions.
2. Run `just check` and `just docs-contract-test`; require Linux/macOS/Windows CI
   and Windows packaging. If local Windows SDK setup is absent, record that limit
   and obtain native Windows CI rather than weakening checks.
3. Build one identifiable candidate and compare prediction off/on over the same
   native SSH route with current Claude and Codex. Use
   `scripts/remote_agent_latency_smoke.py --help` and `--editing`; also run the
   synthetic latency/reconnect and edge-case harnesses. Verify exact remote
   drafts/cursors and rapid mixed edits, not just local timing. Use disposable
   sessions, never submit model prompts, and clean up test resources.
4. Check first-character warmth after a new message and client restart, shared
   learning across two clients, changed-layout/word-rule relearning, Unicode
   fallbacks, unsupported clicks and disconnect behavior. Profile pane scaling
   when render paths change. Record agent versions, binary/source IDs and limits;
   keep workspace targets, credentials and raw diagnostics out of the public fork.
5. Preserve the weekly/manual updater on the default prototype branch, its
   verified stable-tag input, no-force-push/no-auto-merge behavior and the fork CI
   branch trigger. Merge only after validation; then rebuild the local client.

## Rebuild after merging

Install the Rust version from `rust-toolchain.toml` and the Zig version required
by the merged vendored Ghostty source, then run `just check`. Build an identifiable
prototype separately from the stable executable:

```sh
HERDR_BUILD_CHANNEL=proto HERDR_BUILD_ID="$(git rev-parse --short=12 HEAD)" \
  just build
install -m 755 target/release/herdr ~/.local/bin/herdr-proto
```

The `proto` release channel selects separate `herdr-proto` config, state,
machine-catalog and local socket directories. Verify that `herdr-proto machine
list --json` and `herdr machine list --json` remain separate after each upstream
merge. Keep remote test sessions distinct too; local isolation does not split
a shared remote session. `[remote].predict_input = true` enables prediction;
it remains opt-in and does not add offline input replay.

## Remote Codex startup troubleshooting

A background-server startup dialog is separate from prediction readiness. Check
the CLI and managed daemon using the workspace's own Codex wrapper and scoped
`CODEX_HOME`; the desktop app's bundled executable may be a separate server.
With CLI versions supporting these commands, use `codex app-server daemon version`
and, when needed, `start` or `restart` through that same wrapper. Inspect current
`--help` after a CLI update. A matching-version daemon can still stall; verify a
fresh daemon-backed editor and its feature-list response after recovery. Preserve
credentials/history, and coordinate restart of attached clients. Do not make
foreground fallback or a daemon-version check a predictor dependency.
