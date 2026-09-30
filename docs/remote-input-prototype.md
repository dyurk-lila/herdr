# Remote input prototype

This branch ports [PR #4040](https://github.com/herdrdev/herdr/pull/4040) onto stable Herdr **v0.9.3** (`7b116c05bfda646af39d2524c54e70c751f57ee8`). Original feature source: `7527c76e74def5b0c48749cc7caa33ebbadd7cfc` by itsfabioroma.

The local client predicts an ordered sequence of single-cell text, Backspace,
Delete, Left/Right and End. These operations compose while earlier input is
awaiting echo, including insertion and deletion inside the observed input.
Tentative text is underlined; the authoritative server frame stays unchanged.
Simple Unicode such as `é` and `λ` is supported without normalization. Wide
characters, combining clusters, wrapping and selections use remote rendering.

Ctrl-W and Alt-Backspace learn independently from exact deletion echoes. They
predict only where the remaining boundary hypotheses agree. A forwarded,
single-cell left click in the same input row can learn cursor positioning;
click prediction initially requires a visible hardware cursor. Local selection,
drag, multiple clicks, modifiers and pixel coordinates remain authoritative.

Recognized Claude/Codex input rows provide fresh live input bounds. Learning is
shared per SSH destination and agent, including across named sessions and client
restarts. Exact fingerprints of learned editor prompts can warm a new message
without another initial-character round trip. Different prompts/layouts require
fresh confirmation. See [editing design](remote-editing-design.md) for scope and
prior art.

The port preserves stable graphics composition, keybinding routing and connection-generation fencing. Primary remote connections also recover through the endpoint supervisor while preserving the remote panes. It uses the existing generation-one endpoint contract with an unchanged stable remote server. No UDP transport is required.

## Build and enable

Install the dependencies described by the repository's normal build instructions, then:

```sh
HERDR_BUILD_CHANNEL=proto HERDR_BUILD_ID=remote-input just build
install -m 755 target/release/herdr "$HOME/.local/bin/herdr-proto"
```

Release builds with `HERDR_BUILD_CHANNEL=proto` use the `herdr-proto` application
directory for configuration, machine profiles, local sessions and sockets.
On macOS/Linux the config is `~/.config/herdr-proto/config.toml` and machine
profiles live under `~/.local/state/herdr-proto/`. Normal XDG root overrides still
work; changing XDG roots is unnecessary and can affect SSH ProxyCommand helpers.
Stable/preview release builds retain `herdr`; debug builds retain `herdr-dev`.

Enable this opt-in setting in the prototype client's configuration:

```toml
[remote]
predict_input = true
```

Connect with `herdr-proto --remote YOUR_SSH_ALIAS --session YOUR_SESSION`, or save
a machine and launch the client:

```sh
herdr-proto machine add YOUR_SSH_ALIAS --label test --remote-session YOUR_SESSION
herdr-proto
```

The normal Herdr machine catalog is separate. Use a dedicated remote session as
well: multiple clients attaching to the same remote session still share its
panes, focus and geometry.

Explicit `HERDR_CONFIG_PATH`, `HERDR_SOCKET_PATH` and `HERDR_CLIENT_SOCKET_PATH`
overrides remain supported. From inside another Herdr pane, clear inherited
overrides before running prototype CLI commands. Launch the interactive client
from an outer terminal unless nested Herdr is intentionally enabled.

## Validation and limits

The native macOS prototype passed 3,792 tests, formatting/Clippy, maintenance,
architecture, integration and documentation checks. Real Claude 2.1.285 editing
probes measured median Backspace latency of 219 ms without prediction and 0.63 ms
with prediction. Left/Right, End, insertion, Unicode and learned word deletion
were around 1 ms, while Delete measured 4.4 ms. Exact authoritative drafts and a
rapid mixed-edit burst passed; no model prompts were submitted.

Prediction temporarily requires full composition: 0.288 ms per update at 120×48
with one populated pane and 0.332 ms with 15 panes (1.15×). These are local
measurements, not platform-wide guarantees. Native Windows is covered by fork CI;
the local Windows cross-check requires an SDK configuration absent on this machine.

Echo permission is inferred from visible output. Ordinary non-echoing prompts do not train prediction, but an application changing echo behavior within an already trained line can briefly expose speculative characters. Disable prediction when that behavior matters.

Unknown input boundaries and wrapped lines remain authoritative. Without input
acknowledgements, cancelling edit sequences can have indistinguishable screens;
unchanged screens never retire pending edits. The queue holds at most 256 edits.
Unconfirmed prediction disappears after 750 ms. An invisible, bounded three-second
confirmation cache can recover knowledge from a late exact echo; new input or a
changed context discards unresolved history.

Profiles live in the prototype's state directory at
`client/prediction-profiles/profiles.json`. They store hashed destination identities,
agent names, prompt fingerprints and learned word-rule candidates, without draft
text or cursor positions. Writes run in the background, use a separate cross-client
lock, and replace the private file atomically. Running clients refresh every two
seconds. Different SSH aliases have different identities. Delete this profile file
with clients stopped to reset learning; malformed/unknown schemas stay untrained.

The stable remote protocol exposes neither editor version nor echo permissions.
Activity/readiness labels are advisory: an identified agent still needs live input
geometry and exact learned prompt/echo evidence. Agent detection, fingerprints and
matching echoes are evidence, not a
permission guarantee. An unexpected predicted echo invalidates that machine/agent
profile; input itself is always forwarded once, regardless of prediction.

Disconnected input is currently discarded. [OFFLINE_TYPING.md](../OFFLINE_TYPING.md) scopes reliable offline draft recovery with resumable server acknowledgements; that extension is not implemented here. [Prototype maintenance](prototype-maintenance.md) describes updating this branch against later stable releases.
