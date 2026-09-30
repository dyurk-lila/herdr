# Editing prediction design

The client keeps one active line model and an ordered queue of edit operations.
Rendering uses its cached projection, never the authoritative terminal buffer.
A changed remote row and cursor must match an exact queue prefix. The earliest
matching prefix is retired, preserving later speculative operations. Cancelling
sequences remain ambiguous until a different screen arrives or prediction expires.

## Tractable scope

- Single-row, single-cell text with stable geometry and input bounds.
- Append and middle insertion, Backspace, Delete, Left/Right and End.
- Unicode scalars only when Ghostty and unicode-width both report one cell and
  insertion preserves surrounding grapheme boundaries.
- Learned word deletion per exact gesture; whitespace, punctuation, Unicode word
  and trailing-whitespace hypotheses remain separate until observed evidence
  distinguishes them.
- Learned single left-click positioning in an application that reports mouse
  input. Hidden software carets, selections, drag, multiple clicks and pixels
  currently disable click prediction.
- Exact learned prompt reuse for recognized agent editors; machine/agent behavior
  profiles persist without draft content, screen cells or cursor bounds.

Home, wrapping, general grapheme editing, wide characters and placeholder elements
need a stronger logical editor model. Typed Enter and unsupported operations retain
the normal authoritative input route. Persistent profiles are local to this client
installation and scoped by the exact SSH destination; they are not uploaded.

## Agent updates

Activity/readiness status is advisory and no agent version is pinned. Live geometry,
exact prompt fingerprints and matching echoes govern reuse. New prompt layouts or
unsupported edit semantics use authoritative rendering. Contradictory echoes remove
the machine/agent profile; fresh echoes can learn the replacement behavior. This
avoids depending on agent-specific internal editor APIs, but a behavior change can
still cause one transient incorrect prediction before its echo is observed.

Persistence uses an invalidation epoch: stale client observations cannot restore
removed profiles, and concurrent invalidations still remove their own target.
Background queues are bounded; overflow resets shared learning and pauses reuse
until the newer epoch is visible. Canonical input delivery stays independent.

## Prior art and constraints

[Mosh's predictor](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/frontend/terminaloverlay.cc)
provides useful conservative cursor/deletion behavior. Its server also supplies
screen-associated echo acknowledgements; the [Mosh paper, section 3.2](https://mosh.org/mosh-paper.pdf)
explains why transport receipt and application redraw differ. Our unchanged
server has no such watermark, so screen matching remains a bounded heuristic.

[Claude's editing shortcuts](https://code.claude.com/docs/en/interactive-mode#word-boundaries-in-editing-shortcuts)
distinguish whitespace-based Ctrl-W from punctuation-based Option-Delete.
[Codex's editor](https://github.com/openai/codex/blob/60947e234156ac12bdb7fba2477d3965f166bd34/codex-rs/tui/src/bottom_pane/textarea.rs)
uses Unicode word boundaries, special punctuation handling, and script-specific
Backspace behavior. A universal word/grapheme deletion rule would therefore be wrong.

[Unicode grapheme boundaries](https://unicode.org/reports/tr29/#Grapheme_Cluster_Boundaries)
and [Ghostty's width API](https://github.com/ghostty-org/ghostty/blob/44f2a44df7e8c4a0c6df3f7d872ef3d7ead88e51/include/ghostty/vt/unicode.h)
show why codepoints cannot generally be treated as independent terminal cells.
The prototype deliberately rejects cluster extensions and wider glyphs.

[iTerm2 Composer and input buffering](https://iterm2.com/documentation-menu-items.html)
illustrate a separate option: an independent local draft editor with explicit send.
That can support broader Unicode and multiline editing, but needs target/context
checks and changes the ordinary direct-editing interaction.

[QUIC](https://www.rfc-editor.org/rfc/rfc9000.html#section-3.2) delivers stream bytes;
it does not acknowledge editor processing or provide application-level replay
safety after a replacement connection. A negotiated server extension with input
IDs, deduplication, context identity and an echo watermark would remove the main
reconciliation ambiguity. Reliable disconnected typing remains scoped in
[OFFLINE_TYPING.md](../OFFLINE_TYPING.md), rather than changing generation-one codecs.
