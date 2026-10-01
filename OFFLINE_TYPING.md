# Continuous typing across temporary disconnects

The prototype implements a **client-local reconnect draft**, without UDP, a new
SSH bridge, or a modified remote server. Enable both settings:

```toml
[remote]
predict_input = true
buffer_reconnect_input = true
```

## Current behavior

After a detected disconnect, a two-row **Reconnect draft** panel remains editable
inside the pane while remote interaction is frozen. Capture continues through
presentation synchronization, including the interval when the badge says online
but pane input is still fenced. Local text, single-line paste, Unicode-aware
Left/Right/Home/End, Backspace/Delete and supported word editing use Herdr's
existing text editor. Movement and deletion apply only to the new, never-sent
suffix. Enter, interrupts and unsupported controls are consumed with a notice;
they are never queued. Client prefix and direct shortcuts retain priority.

Once activation commits, best-effort recovery requires a recognized Claude/Codex
input row with no unconfirmed online input, the same endpoint, pane, server boot,
agent, geometry, terminal modes, full original cells and cursor. Delivery
confidence survives client-only prefix, focus and prediction resets; cold
prediction alone does not block an otherwise clean editor. An unchanged picture
is a heuristic, not an application-supported prompt epoch.

A new control-free Unicode suffix is inserted once at that cursor, including
wide/combining text or text that will wrap. A locally moved middle cursor is
restored with ordered Left events. The existing editor baseline must still have
safe, recognized single-row bounds; an already wrapped or ambiguous editor stays
manual. This canonical handoff does not imply inline prediction of wide text or
application-specific wrapping.

Each suffix is marked attempted **before** transport enqueue and attempted only
once. Successful enqueue immediately hides the draft panel and restores normal
agent keyboard routing, without a click or waiting for echo. A failed enqueue
keeps recovery visible. Exact projected text/cursor echo in the same connection
generation retires the retained receipt and may restore prediction confidence.
Later online input cannot be acknowledged or overwritten by that older echo.

A second drop before confirmation, a changed generation, or three seconds
without exact confirmation preserves the receipt as uncertain and prevents
automatic resend. Unconfirmed receipts remain hidden while online and reappear
on a later drop. Wide/wrapped handoffs have no exact inline echo projection, so
that later recovery remains manual until Copy/Discard; continuing online is
unblocked. Copy recovery composes later offline edits at the attempted cursor,
including middle insertion. A hidden caret painted over text needs fresh
prediction learning.

Changed/unknown context, unresolved pre-drop input and unsupported editor
baselines stay in the panel with **Copy** and **Discard**. While fully online,
this recovery panel does not capture ordinary typing from the agent editor. A replaced pane exposes its
original draft on the same machine for manual recovery; it never retargets input.
Switching machines keeps drafts with their original endpoint. Drafts are bounded
to 16 targets and 64 KiB of retained text per target, with visible limit notices.
Delivery confidence also tracks at most 16 unresolved targets; overflow keeps
automatic recovery conservative until client restart. Drafts live only in
client memory and disappear when that client exits. They are
never written to shared learning profiles or diagnostics by the feature.

## Limits and maintenance contract

This supports ordinary detected outages with best-effort automatic continuation.
It cannot preserve input sent between the actual drop and its detection, prove
that identical screen pixels mean the same application prompt, or determine
whether an uncertain attempt reached the remote editor. Copying uncertain text
may include text already present remotely. Keeping the online predictor alive
would not resolve these delivery ambiguities.

Keep remote pane-input leases and presentation gates intact. The draft is local
UI, not an exception to offline transport fencing. Frozen generation-one codecs
and stable remote binaries remain unchanged.

`reconnect_draft.rs` owns the pure bounded editor/attempt model;
`reconnect_delivery.rs` retains bounded delivery confidence independently of
prediction resets; `reconnect_input.rs` routes local input, renders recovery, and creates guarded
one-shot requests. `tests/reconnect_draft.rs` protects partial activation,
changed/replaced panes, uncertain online input, local controls and retained
rendering. Pure model tests cover second drops before echo, limits, full-cell
context and composed recovery. Run `scripts/remote_reconnect_draft_smoke.py
--help` for disposable native Claude/Codex outage comparisons. Keep its raw
artifacts private and never submit model prompts.

## Future resumable delivery capability

Keep a separate local draft across reconnects, and add an optional resumable input capability to the remote Herdr server. Route supported text through this capability from the start of the connection:

- Each immutable input batch carries a client resume token, server boot identity, terminal incarnation/input-lane epoch and increasing sequence number.
- The server accepts each sequence once into a retained ordered PTY input queue and acknowledges its contiguous accepted watermark. Reject gaps and same-sequence/different-payload duplicates; a full or closed queue does not advance acceptance. ACK means queue admission, not editor application.
- A reconnect atomically revokes the previous connection's input lease, reports the watermark and validates the target context. The client sends only later batches. Missing resume state after restart is rejected rather than treated as watermark zero.
- Prediction still renders tentative text locally; visual echo remains separate from delivery acknowledgement. Disconnected drafts use their own lifetime instead of the predictor's 750 ms timeout.
- Begin with appended text and Backspace only within the never-transmitted, unsequenced draft suffix. Transmitted batches remain immutable even before acknowledgement. Keep Enter and other controls online-only and fenced until earlier draft batches are accepted in the same ordered input lane. If the editor/pane/context changes, preserve the draft for recovery instead of injecting it into a different prompt.

This would cover the usual case of continuing to type into the same Claude/Codex textbox while SSH reconnects in the background. It needs a modified remote Herdr server in addition to the local client. Existing generation-one `EndpointControl`/advertised JSON method envelopes can carry an optional new capability without changing frozen core codecs; older servers retain current behavior.

The delivery promise is once into the same live terminal's input lane during network replacement. Server/terminal restart or an application changing its input context need explicit rejection/recovery rules. Input from another client/API must revoke the exclusive lane or advance its epoch. Terminal `ECHO` is not sufficient for coding-agent TUIs, which draw their own editors. Terminal identity and matching screen pixels cannot prove that the same textbox is active: reliable automatic replay needs an application-supported input epoch or an explicit draft-recovery fallback. Arbitrary application-level prompt identity and exactly-once model submission cannot be inferred from screen pixels.
