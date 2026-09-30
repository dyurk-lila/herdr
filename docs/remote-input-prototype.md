# Remote input prototype

This branch ports [PR #4040](https://github.com/herdrdev/herdr/pull/4040) onto stable Herdr **v0.9.3** (`7b116c05bfda646af39d2524c54e70c751f57ee8`). Original feature source: `7527c76e74def5b0c48749cc7caa33ebbadd7cfc` by itsfabioroma.

The local client predicts conservative printable ASCII after observing exact remote echo. Tentative characters are underlined until confirmed. Authoritative server state is never changed by prediction. Editing, paste, Unicode, focus/geometry/context changes, mismatch, disconnect and 750 ms of unconfirmed input clear speculative state.

The port preserves stable graphics composition, keybinding routing and connection-generation fencing. Primary remote connections also recover through the endpoint supervisor while preserving the remote panes. It uses the existing generation-one endpoint contract with an unchanged stable remote server. No UDP transport is required.

## Build and enable

Install the dependencies described by the repository's normal build instructions, then:

```sh
HERDR_BUILD_CHANNEL=proto HERDR_BUILD_ID=remote-input just build
install -m 755 target/release/herdr "$HOME/.local/bin/herdr-proto"
```

Enable this opt-in setting in the client's configuration:

```toml
[remote]
predict_input = true
```

Connect with `herdr-proto --remote YOUR_SSH_ALIAS --session YOUR_SESSION`. For side-by-side use with stable Herdr, provide a separate `HERDR_CONFIG_PATH` and separate `XDG_CONFIG_HOME`/`XDG_STATE_HOME`. SSH ProxyCommand helpers may also use XDG configuration; expose their existing configuration in the isolated config directory as needed.

## Validation and limits

The native macOS prototype passed 3,730 tests, formatting/Clippy and maintenance, architecture, integration and documentation checks. In a measured remote session, median synthetic visible latency fell from about 225 ms to 0.33 ms, while authoritative acknowledgement remained about 223 ms. Claude's real editor improved from about 222 ms to 1.27 ms. Generated drafts matched the authoritative pane and no model prompts were submitted. A rapid-typing check with five simultaneously unacknowledged characters preserved all input exactly.

Live edge checks cover Backspace, bracketed paste, Unicode fallback, resize, Ctrl-U and a non-echoing prompt. Prediction temporarily requires full composition: about 0.29 ms per update at 120×48, with similar cost for one and 15 populated panes. These are local measurements, not platform-wide guarantees. Windows was not locally cross-compiled.

Echo permission is inferred from visible output. Ordinary non-echoing prompts do not train prediction, but an application changing echo behavior within an already trained line can briefly expose speculative characters. Disable prediction when that behavior matters.

Disconnected input is currently discarded. [OFFLINE_TYPING.md](../OFFLINE_TYPING.md) scopes reliable offline draft recovery with resumable server acknowledgements; that extension is not implemented here. [Prototype maintenance](prototype-maintenance.md) describes updating this branch against later stable releases.
