# Maintaining the remote-input prototype

The custom fork is `dyurk-lila/herdr`; its prototype branch is
`dyurk/remote-input-proto`. Stable upstream releases come from `herdrdev/herdr`.
The fork's default branch should be the prototype branch.

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

## Rebuild after merging

Install the Rust version from `rust-toolchain.toml` and the Zig version required
by the merged vendored Ghostty source, then run `just check`. Build an identifiable
prototype separately from the stable executable:

```sh
HERDR_BUILD_CHANNEL=proto HERDR_BUILD_ID="$(git rev-parse --short=12 HEAD)" \
  cargo build --release --locked
install -m 755 target/release/herdr ~/.local/bin/herdr-proto
```

The `proto` release channel selects separate `herdr-proto` config, state,
machine-catalog and local socket directories. Verify that `herdr-proto machine
list --json` and `herdr machine list --json` remain separate after each upstream
merge. Keep remote test sessions distinct too; local isolation does not split
a shared remote session. `[remote].predict_input = true` enables prediction;
it remains opt-in and does not add offline input replay.
