# Maintaining the remote-input stack

The custom fork is `dyurk-lila/herdr`. Its review stack is:

1. `main` mirrors the live `herdrdev/herdr` default branch (`master`).
2. `dyurk/predictive-typing` adds prediction, shared learning and bootstrap support.
3. `dyurk/reconnect-scratchpad` adds the disconnected local editor.

`dyurk/remote-input-proto` preserves the original combined history. Its legacy
stable-release updater does not maintain this stack; do not use it to update these
branches. The split branches omit that workflow. Use the ordered update below or the repository-local
[stack maintenance skill](../.agents/skills/update-herdr-prototype-stack/SKILL.md).
The [feature guide](remote-input-prototype.md) and [editing design](remote-editing-design.md)
describe behavior; this document is the acceptance contract for later updates.

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
| Reconnect drafts | Keep opt-in client-local capture through committed activation, Unicode-aware never-sent suffix editing, 16-target/64-KiB bounds, visible limits and volatile storage. Once pane input is ready, transfer all scratchpad text through the canonical input queue independently of prediction training, previous unconfirmed input, changed editor contents or agent readiness. Successful queue admission removes the scratchpad immediately and restores normal agent input and cursor; failed admission retains unsent text. A server terminal popup defers pane input until closing it automatically flushes the scratchpad. No Copy/Discard, manual recovery or retained attempted receipts. Never replay accepted text on a later drop. Preserve endpoint isolation; prefer the original visible pane, otherwise the focused visible pane on that endpoint. Preserve ordered middle-cursor Left events up to 4,095; larger restoration transfers all text with the caret at end to respect the stable event-batch limit. Wide/wrapped text can transfer without inline projection. Enter/controls stay unqueued; prefix shortcuts and remote input/presentation leases remain intact. Queue admission is not authoritative delivery acknowledgement. |
| Compatibility and isolation | Keep default-on prediction and its client-local Settings → Typing switch and separate `herdr-proto` config/state/catalog/socket paths, including direct and saved-machine routing. Keep stable remote interoperability and connection recovery. Do not change frozen generation-one codecs, enum variants, fixtures or method meanings to make new features pass. |
| Remote bootstrap | Prototype builds reuse compatible official remote servers and install official stable assets for the Cargo base version when needed. Never seed a remote with the local prototype by default or look up custom prototype releases in upstream manifests. Preserve explicit binary overrides, checksums, capability checks and installation/restart confirmations; stable/preview clients keep upstream behavior. |

Reliable resumable delivery is **not** part of this contract. Client-local
reconnect drafts are best effort and opt-in. [OFFLINE_TYPING.md](../OFFLINE_TYPING.md) describes the
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
- `reconnect_draft.rs`, `reconnect_input.rs` and `tests/reconnect_draft.rs`:
  local capture through partial activation, bounded scratchpad editing,
  one-shot queue admission, cursor-aware handoff, failed enqueue and retained-patch
  occlusion. Test changed editors and previous unconfirmed input without holding
  the panel, plus a second outage after wide Unicode without accepted-text replay.
  Run `scripts/remote_reconnect_draft_smoke.py` against unchanged stable servers
  with both current agents, comparing disabled/enabled scratchpads.
- Existing frozen endpoint codec/digest/method fixtures and client reconnect tests
  remain compatibility tests; never rewrite their expectations to bless drift.

## Update the stack

Keep `main` free of prototype changes so it can fast-forward to upstream. Fetch
live refs before choosing a base, and rebase the prediction layer before the
scratchpad layer. Preserve the old stack heads locally before a rebase.

```sh
git fetch upstream
git fetch origin
git switch main
git merge --ff-only upstream/master
git push origin main
gh stack checkout dyurk/predictive-typing
gh stack rebase --remote origin
```

Resolve conflicts against the feature contracts, not by selecting one side
wholesale. Stable tags may have separate promotion ancestry; the fork's `main`
tracks upstream's development branch, not the latest release tag. Keep the shared
Gen1 codecs and fixtures unchanged. Validate both layers independently with
`just check` and `just docs-contract-test`, then publish the restacked PRs with
`gh stack submit --auto --open --remote origin`. Review branch rewrites and use
lease protection; never force-push `main` or merge the PRs automatically.

Run the synthetic latency/edge harnesses and real Claude/Codex editing comparisons
after input or transport integration changes. Use disposable sessions and matching
agent versions; never submit model prompts. Check exact authoritative drafts and
cursor positions, first-character warmth, shared profiles, changed word/layout
behavior, unsupported Unicode/click fallbacks, and offline-input nonreplay.
Profile one and fifteen populated panes after render-path changes. Record source
and binary IDs, versions and limitations; keep targets, credentials and raw
agent diagnostics out of the public fork. Probe invocations disable agent update
checks; `--claude-binary` can pin a matching native executable.

Linux/macOS/Windows CI and Windows packaging remain the platform checks. If the
local Windows SDK is absent or GitHub Actions cannot run, record the verification
limit instead of weakening checks or claiming a green result.

## Build the selected layer

The lower branch builds prediction alone; the upper branch builds both features.
Use Rust from `rust-toolchain.toml` and the Zig version required by vendored
Ghostty. Keep installed regular Herdr separate. The built-in `herdr-proto update`
uses upstream manifests and can replace the prototype with regular Herdr.

```sh
HERDR_BUILD_CHANNEL=proto HERDR_BUILD_ID="$(git rev-parse --short=12 HEAD)" \
  just build
install -m 755 target/release/herdr ~/.local/bin/herdr-proto
```

The `proto` release channel keeps separate config, state, machine catalog and
local sockets. Confirm that `herdr-proto machine list --json` and regular Herdr's
catalog remain separate. Remote sessions still share panes and geometry across
clients; keep test session names unique. `[remote].predict_input = true` enables
prediction; it defaults on and can be disabled in Settings → Typing. Official
compatible remote Herdr is sufficient.

The upper layer separately enables `[remote].buffer_reconnect_input = true`.
Retest the reconnect harness after activation, rendering or transport changes,
including repeated loss, changed editors and unconfirmed input. Scratchpad queue
admission is best effort, not server acknowledgement or reliable replay.

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
