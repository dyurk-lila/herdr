# Continuous typing across temporary disconnects

The prototype implements a **client-local reconnect draft**, without UDP, a new
SSH bridge, or a modified remote server. Enable buffering; inline prediction is
an independent option:

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

Once activation commits and remote pane input is available, all scratchpad text
is handed to the remote pane's normal input queue. Prediction training, prior
unconfirmed input, changed editor contents, agent readiness labels and exact
screen echoes do not gate this transfer. There are no Copy/Discard controls or
manual recovery mode. The scratchpad disappears immediately after a successful
queue admission, and normal agent input and cursor rendering resume. An open
server terminal popup temporarily defers pane input; closing it automatically
flushes the waiting scratchpad.

Control-free Unicode text can transfer even when it is too wide or long for
inline prediction. A locally moved middle cursor is restored with ordered Left
events after the text. If restoring the cursor would require more than 4,095
Left events, all text still transfers but the caret stays at the end, avoiding
the stable server's event-batch limit. This does not imply prediction of
application-specific Unicode cursor movement or wrapping. Failed queue admission
leaves the unsent scratchpad available for the next ready connection. Accepted
scratchpad text is removed, never retained as a receipt, and never replayed on a
later disconnect; that outage starts a new scratchpad containing only newly typed
text.

Drafts remain associated with their original endpoint. The original visible pane
is preferred; if it is gone or no longer viewed, handoff uses the focused visible
pane on that same endpoint. Switching machines does not move scratchpad text
between destinations. They are bounded to 16 targets and 64 KiB per target, with
visible limit notices. Scratchpads live
only in client memory and disappear when the client exits. They are never
written to shared learning profiles or diagnostics by this feature.

## Limits and maintenance contract

This is best-effort automatic continuation during detected outages. Queue
admission means the local connection accepted the request, not that the remote
editor applied it. A later failure can lose accepted text; the client does not
retry it and risk duplicate insertion. Input sent between the actual drop and
its detection remains subject to the normal connection's delivery limits.

Text goes to the current editor in the target pane even when its contents,
application or prompt changed during the outage. Screen pixels cannot establish
application prompt identity. Enter and other controls remain unqueued, so the
scratchpad never submits a model prompt automatically.

Keep remote pane-input leases and presentation gates intact. The scratchpad is
local UI, not an exception to offline transport fencing. Frozen generation-one
codecs and stable remote binaries remain unchanged.

`reconnect_draft.rs` owns the pure bounded scratchpad editor;
`reconnect_input.rs` captures local input, renders the panel and creates the
one-shot request after committed readiness. `tests/reconnect_draft.rs` protects
partial activation, changed editors, unconfirmed earlier input, local controls,
failed enqueue and retained rendering. Pure model tests cover bounded editing,
cursor positioning and removal of accepted text. Run
`scripts/remote_reconnect_draft_smoke.py --help` for disposable native
Claude/Codex outage comparisons, including Unicode across a second loss. Keep
its raw artifacts private and never submit model prompts.

## Future resumable delivery capability

Keep a separate local draft across reconnects, and add an optional resumable input capability to the remote Herdr server. Route supported text through this capability from the start of the connection:

- Each immutable input batch carries a client resume token, server boot identity, terminal incarnation/input-lane epoch and increasing sequence number.
- The server accepts each sequence once into a retained ordered PTY input queue and acknowledges its contiguous accepted watermark. Reject gaps and same-sequence/different-payload duplicates; a full or closed queue does not advance acceptance. ACK means queue admission, not editor application.
- A reconnect atomically revokes the previous connection's input lease, reports the watermark and validates the target context. The client sends only later batches. Missing resume state after restart is rejected rather than treated as watermark zero.
- Prediction still renders tentative text locally; visual echo remains separate from delivery acknowledgement. Disconnected drafts use their own lifetime instead of the predictor's 750 ms timeout.
- Begin with appended text and Backspace only within the never-transmitted, unsequenced draft suffix. Transmitted batches remain immutable even before acknowledgement. Keep Enter and other controls online-only and fenced until earlier draft batches are accepted in the same ordered input lane. If the editor/pane/context changes, preserve the draft for recovery instead of injecting it into a different prompt.

This would cover the usual case of continuing to type into the same Claude/Codex textbox while SSH reconnects in the background. It needs a modified remote Herdr server in addition to the local client. Existing generation-one `EndpointControl`/advertised JSON method envelopes can carry an optional new capability without changing frozen core codecs; older servers retain current behavior.

The delivery promise is once into the same live terminal's input lane during network replacement. Server/terminal restart or an application changing its input context need explicit rejection/recovery rules. Input from another client/API must revoke the exclusive lane or advance its epoch. Terminal `ECHO` is not sufficient for coding-agent TUIs, which draw their own editors. Terminal identity and matching screen pixels cannot prove that the same textbox is active: reliable automatic replay needs an application-supported input epoch or an explicit draft-recovery fallback. Arbitrary application-level prompt identity and exactly-once model submission cannot be inferred from screen pixels.
