# Continuous typing across temporary disconnects

**Feasible, without UDP or Anyscale infrastructure changes.** The current prototype deliberately discards disconnected input. Keeping its prediction object alive alone would be unsafe: its pending characters may already have reached the remote editor even though their visual echoes have not returned.

## Recommended extension

Keep a separate local draft across reconnects, and add an optional resumable input capability to the remote Herdr server. Route supported text through this capability from the start of the connection:

- Each immutable input batch carries a client resume token, server boot identity, terminal incarnation/input-lane epoch and increasing sequence number.
- The server accepts each sequence once into a retained ordered PTY input queue and acknowledges its contiguous accepted watermark. Reject gaps and same-sequence/different-payload duplicates; a full or closed queue does not advance acceptance. ACK means queue admission, not editor application.
- A reconnect atomically revokes the previous connection's input lease, reports the watermark and validates the target context. The client sends only later batches. Missing resume state after restart is rejected rather than treated as watermark zero.
- Prediction still renders tentative text locally; visual echo remains separate from delivery acknowledgement. Disconnected drafts use their own lifetime instead of the predictor's 750 ms timeout.
- Begin with appended text and Backspace only within the never-transmitted, unsequenced draft suffix. Transmitted batches remain immutable even before acknowledgement. Keep Enter and other controls online-only and fenced until earlier draft batches are accepted in the same ordered input lane. If the editor/pane/context changes, preserve the draft for recovery instead of injecting it into a different prompt.

This would cover the usual case of continuing to type into the same Claude/Codex textbox while SSH reconnects in the background. It needs a modified remote Herdr server in addition to the local client. Existing generation-one `EndpointControl`/advertised JSON method envelopes can carry an optional new capability without changing frozen core codecs; older servers retain current behavior.

The delivery promise is once into the same live terminal's input lane during network replacement. Server/terminal restart or an application changing its input context need explicit rejection/recovery rules. Input from another client/API must revoke the exclusive lane or advance its epoch. Terminal `ECHO` is not sufficient for coding-agent TUIs, which draw their own editors. Terminal identity and matching screen pixels cannot prove that the same textbox is active: reliable automatic replay needs an application-supported input epoch or an explicit draft-recovery fallback. Arbitrary application-level prompt identity and exactly-once model submission cannot be inferred from screen pixels.

## Smaller client-only first step

Capture text only after transmission is fenced off, in a separate offline draft. A fresh coherent surface permits recovery but cannot itself prove unchanged application context; use explicit draft recovery when no reliable input epoch is available. Never replay uncertain pre-disconnect input or retry a flushed batch whose confirmation was lost. This helps detected outages but cannot guarantee preservation of characters typed between the actual drop and failure detection. It therefore does not fully meet the lossless continuous-typing goal.

### Concrete path around the reconnecting pane lockout

The lockout is client policy, not a requirement of SSH or the remote-machine
model. Stdin still reaches `ClientShellState::handle_raw_events` during reconnect.
`finish_client_shell_input` discards pane requests unless the active endpoint and
surface are ready and no activation is pending. Endpoint status changes clear
prediction; composition removes the pane cursor and hit targets. Conversely,
`ClientState::present_frozen_chrome` already permits local UI updates over the
frozen remote surface.

A bounded client-only draft can therefore be added without changing endpoint
activation, SSH bridges, the remote server or frozen codecs:

1. Capture printable input only after the disconnect gate is established, before
   it becomes a remote request. Keep one in-memory draft per endpoint/pane, with
   its originating boot, prompt/line evidence and cursor location. Do not copy
   unresolved online prediction into the never-sent draft.
2. Render the draft inside the frozen pane with a local cursor and a queued-text
   indication. Reuse `src/client/shell/text_editor.rs` for Unicode-aware editing;
   initially restrict deletion/movement to this new suffix. Enter, interrupts,
   pane commands and arbitrary terminal controls must not be queued.
3. Continue capture through reconnect synchronization. Return to remote input only
   after activation commits a coherent new surface, not when transport merely
   connects or the badge changes. Switching machines/panes leaves the draft with
   its original target rather than redirecting it.
4. The conservative mode offers explicit draft recovery. An optional best-effort
   mode can append automatically when a fresh recognized editor, original boot
   and pane, and exact input line/cursor agree. This check is a heuristic: identical
   pixels do not prove unchanged application context. Changed/unknown context must
   keep the draft recoverable rather than inject it.
5. On a delivery attempt, move the suffix out of the never-sent state before
   handing it to transport. Local writer acceptance is not server/editor
   acknowledgement. A second disconnect makes attempted text uncertain: retain a
   recovery copy, but never automatically resend it. Keep later never-sent text
   separate and preserve its order.

This is a small presentation/input feature, suitable for ordinary periodic
disconnects when best-effort automatic continuation is acceptable. It does not
provide a lossless promise for undetected drops or a failed delivery attempt;
those still require the resumable server capability above. The current build has
not implemented either mode.

Regression cases must cover capture while reconnecting, local edits with zero
transport writes, partial activation, target switches, changed boot/prompt,
uncertain online input, a second drop during drain, bounded draft size, and exactly
one attempt per delivered suffix. Preserve existing tests that freeze remote pane
interaction and reject offline transport input; the new draft is local UI, not a
reason to remove those gates.

## Implementation hooks and verification

Client draft routing/composition belongs around `finish_client_shell_input`, active-surface activation completion and a separate client presentation state. Runtime input acceptance/deduplication belongs at server dispatch and bounded PTY queue admission, preserving partial-write offsets, with `TerminalId` plus incarnation/context epochs, not only pane IDs or connection generation. Existing `pane.send_text`/`pane.send_input` do not provide delivery IDs or context guards.

Test losses before send, partway through transmission, after server acceptance before ACK, and during partial PTY writes; repeated drops; another attached client; pane/process replacement; no-echo/dialog transitions; and offline editing/Enter boundaries. This is a medium feature spanning client draft handling and server delivery bookkeeping, rather than a configuration toggle.

No offline buffering was added to the current validated prototype. This note scopes the requested extension.
