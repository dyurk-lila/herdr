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

## Implementation hooks and verification

Client draft routing/composition belongs around `finish_client_shell_input`, active-surface activation completion and a separate client presentation state. Runtime input acceptance/deduplication belongs at server dispatch and bounded PTY queue admission, preserving partial-write offsets, with `TerminalId` plus incarnation/context epochs, not only pane IDs or connection generation. Existing `pane.send_text`/`pane.send_input` do not provide delivery IDs or context guards.

Test losses before send, partway through transmission, after server acceptance before ACK, and during partial PTY writes; repeated drops; another attached client; pane/process replacement; no-echo/dialog transitions; and offline editing/Enter boundaries. This is a medium feature spanning client draft handling and server delivery bookkeeping, rather than a configuration toggle.

No offline buffering was added to the current validated prototype. This note scopes the requested extension.
