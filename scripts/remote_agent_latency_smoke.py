#!/usr/bin/env python3
"""Measure unsubmitted drafts in real remote coding-agent UIs.

Uses only new Herdr named sessions and generated /tmp workspaces. No model
prompt is submitted. Raw agent terminal output is discarded. Artifacts contain
generated draft rows, cursor metadata, timing, versions, and candidate debug logs,
plus SSH targets, absolute executable/workspace paths, commands, and diagnostic
errors. Treat these as private diagnostics and review/redact them before sharing.

    python3 scripts/remote_agent_latency_smoke.py --target test-host --agent-bin-dir /opt/agents/bin --agents claude
    python3 scripts/remote_agent_latency_smoke.py --target test-host --agent-bin-dir /opt/agents/bin --prediction both

With --prediction both, exits nonzero unless each agent's prediction-on typing,
post-pause and Backspace medians are less than half their prediction-off values. This
intentionally exposes a predictor which helps ordinary shells but not the
actual agent input editor. Use --self-test for local prompt matching assertions.
Add --editing for exact cursor-cell checks, midline edits, single-cell Unicode,
learned Ctrl-W/Alt-Backspace, supported same-row mouse clicks, and a rapid composed
editing burst. Each measured editing median is compared using the same threshold;
unlearned word deletions, unsupported mouse clicks, and burst timing are excluded.
"""

import argparse
from enum import StrEnum
import hashlib
import json
import os
from pathlib import Path
import shlex
import statistics
import subprocess
import sys
import tempfile
import time
import uuid

from remote_latency_smoke import Client, Remote, Screen, WIDTH

BACKSPACE_MEDIAN_KEY = "median_backspace_ms"
EDITING_MEDIANS_KEY = "editing_medians_ms"
TEST_AGENTS = ("claude", "codex", "pi", "opencode")
AGENT_STATES = ("idle", "working", "blocked", "done", "unknown")
EDITOR_STABLE_SECONDS = 6.0
TRUST_DIALOG_PHRASES = ("do you trust", "trust this folder", "trust the files", "trust the contents", "is this a project you created")
LOGIN_METHOD_PHRASES = ("select login method", "sign in with chatgpt", "claude account", "login method")
FOREGROUND_FALLBACK_LABEL = "Run without daemon this time"


class StableEditorReadiness:
    def __init__(self):
        self.since = None

    def observe(self, ready, now):
        if not ready:
            self.since = None
            return False
        if self.since is None:
            self.since = now
        return now - self.since >= EDITOR_STABLE_SECONDS


def foreground_fallback_key(screen):
    selected = [line for line in screen.splitlines() if line.lstrip().startswith(("›", "❯"))]
    if len(selected) != 1:
        return None
    if "Cancel" in selected[0]:
        return "up"
    if FOREGROUND_FALLBACK_LABEL in selected[0]:
        return "enter"
    return None


class EditOperation(StrEnum):
    LEFT = "left"
    RIGHT = "right"
    END = "end"
    DELETE = "delete"
    INSERT = "insert"
    BACKSPACE = "backspace"
    UNICODE = "unicode"
    CTRL_W = "ctrl_w"
    ALT_BACKSPACE = "alt_backspace"
    FIXTURE = "word_fixture"
    MOUSE = "mouse"


EDIT_KEYS = {
    EditOperation.LEFT: b"\x1b[D",
    EditOperation.RIGHT: b"\x1b[C",
    EditOperation.END: b"\x1b[F",
    EditOperation.DELETE: b"\x1b[3~",
    EditOperation.BACKSPACE: b"\x7f",
    EditOperation.CTRL_W: b"\x17",
    EditOperation.ALT_BACKSPACE: b"\x1b\x7f",
}


def is_draft_row(line, expected, *, composed=False, agent=None):
    if not expected or expected not in line:
        return False
    # Codex's Herdr-composed row has a scrollbar at the terminal's final cell.
    # Only remove that observed piece of chrome after actual blank padding;
    # authoritative pane reads and arbitrary draft suffixes stay exact.
    if composed and len(line) == WIDTH and line.endswith(" \u2590"):
        line = line[:-1]
    if agent == "pi":
        if composed:
            if "│" not in line:
                return False
            line = line.split("│", 1)[1]
        return line.strip(" \u00a0") == expected
    markers = ("┃",) if agent == "opencode" else ("❯", "›")
    marker = max(line.rfind(value) for value in markers)
    return marker >= 0 and line[marker + 1:].strip(" \u00a0") == expected


def pi_rule(line, *, composed=False):
    if composed:
        if "│" not in line:
            return False
        line = line.split("│", 1)[1]
    line = line.strip(" \u00a0")
    return len(line) >= 40 and all(char == "─" for char in line)


def pi_editor_rows(screen, workdir):
    lines = screen.splitlines()
    # The update notice also leaves a blank gap between rules. The actual
    # editor's lower rule is immediately followed by this test's cwd footer.
    return [
        index for index in range(1, len(lines) - 2)
        if not lines[index].strip()
        and pi_rule(lines[index - 1])
        and pi_rule(lines[index + 1])
        and lines[index + 2].strip() == workdir
    ]


def draft_rows(screen, expected, *, composed=False, agent=None):
    lines = screen.splitlines()
    matches = []
    for index, line in enumerate(lines):
        if not is_draft_row(line, expected, composed=composed, agent=agent):
            continue
        # Pi's default editor has no prompt glyph. Identify its actual input
        # body by the two adjacent horizontal editor rules, not loose text.
        if agent == "pi" and not (0 < index < len(lines) - 1 and pi_rule(lines[index - 1], composed=composed) and pi_rule(lines[index + 1], composed=composed)):
            continue
        matches.append((index, line))
    return matches


def binary_digest(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as binary:
        for chunk in iter(lambda: binary.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def same_comparison_build(baseline, predicted):
    return all(baseline.get(key) and baseline[key] == predicted.get(key)
               for key in ("binary_sha256", "agent_version"))


def own_pane_agent_metadata(agents, pane, requested_agent):
    matches = [entry for entry in agents if entry["pane_id"] == pane]
    if not matches:
        return {"agent": None, "agent_status": "unknown", "matches_requested": False, "pane_present": False}
    if len(matches) != 1:
        raise RuntimeError("Expected at most one detected agent for the generated test pane")
    entry = matches[0]
    detected = entry["agent"] if "agent" in entry else None
    state = entry["agent_status"]
    return {"agent": detected if detected in TEST_AGENTS else None, "agent_status": state if state in AGENT_STATES else "unknown", "matches_requested": detected == requested_agent, "pane_present": True}


def local_editor_ready(screen, agent):
    lower = screen.lower()
    if agent == "claude":
        return "❯" in screen and any(marker in lower for marker in ("for shortcuts", "plan mode", "shift+tab"))
    if agent == "codex":
        return "openai codex" in lower and "›" in screen and any(marker in lower for marker in ("for shortcuts", "context left"))
    if agent == "opencode":
        return "Ask anything..." in screen and "tab agents" in lower
    return agent == "pi" and sum(pi_rule(line, composed=True) for line in screen.splitlines()) >= 2


def self_test():
    scrollbar_row = "› ZQher".ljust(WIDTH - 1) + "▐"
    assert is_draft_row(scrollbar_row, "ZQher", composed=True)
    assert not is_draft_row(scrollbar_row, "ZQher")
    assert is_draft_row("❯\u00a0ZQher   ", "ZQher")
    for text in ("ZQherEXTRA", "ZQherZQher"):
        assert not is_draft_row(("› " + text).ljust(WIDTH - 1) + "▐", "ZQher", composed=True)
    assert not is_draft_row("› ZQher▐", "ZQher", composed=True)
    assert not is_draft_row("directory /tmp/herdr-agent-smoke", "herdr")
    assert is_draft_row("                   ┃  ZQher   ", "ZQher", agent="opencode")
    assert not is_draft_row("                   ┃  ZQherEXTRA", "ZQher", agent="opencode")
    pi_editor = "\n".join(("─" * 80, "ZQher", "─" * 80))
    assert draft_rows(pi_editor, "ZQher", agent="pi")
    assert not draft_rows(pi_editor.replace("ZQher", "ZQherEXTRA"), "ZQher", agent="pi")
    assert not draft_rows("ZQher", "ZQher", agent="pi")
    composed_pi = "\n".join(" " * 25 + "│" + row for row in ("─" * 114, "ZQher".ljust(114), "─" * 114))
    assert draft_rows(composed_pi, "ZQher", composed=True, agent="pi")
    assert not draft_rows(composed_pi.replace("ZQher", "ZQherZQher"), "ZQher", composed=True, agent="pi")
    generated_cwd = "/tmp/herdr-agent-smoke.fixture"
    pi_startup = "\n".join(("─" * 80, "", "─" * 80, "", "─" * 80, generated_cwd))
    assert pi_editor_rows(pi_startup, generated_cwd) == [3], "update-notice gap is not the editor"
    assert not pi_editor_rows(pi_startup, generated_cwd + "-other")
    metadata = {"binary_sha256": "candidate-a", "agent_version": "agent-1"}
    assert same_comparison_build(metadata, dict(metadata))
    assert not same_comparison_build(metadata, {**metadata, "binary_sha256": "candidate-b"})
    assert not same_comparison_build(metadata, {**metadata, "agent_version": "agent-2"})
    assert not same_comparison_build({}, {})
    screen = CursorScreen("claude")
    for byte in "❯ ZQéλ".encode("utf-8"):
        screen.feed(bytes([byte]))
    assert exact_draft_cursor(screen, "ZQéλ", 4)
    screen.feed(b"\x1b[D")
    assert exact_draft_cursor(screen, "ZQéλ", 3)
    assert not exact_draft_cursor(screen, "ZQéλ", 4)
    screen.feed(b"\x1b[2;6H")
    assert not exact_draft_cursor(screen, "ZQéλ", 3)
    entries = [{"pane_id": "w9:p1", "agent": "claude", "agent_status": "idle", "title": "private title", "tokens": {"secret": "private token"}}]
    assert own_pane_agent_metadata(entries, "w9:p1", "claude") == {"agent": "claude", "agent_status": "idle", "matches_requested": True, "pane_present": True}
    assert not own_pane_agent_metadata(entries, "w1:p1", "claude")["pane_present"]
    entries[0].update(agent="private token", agent_status="private title")
    assert own_pane_agent_metadata(entries, "w9:p1", "claude") == {"agent": None, "agent_status": "unknown", "matches_requested": False, "pane_present": True}
    readiness = StableEditorReadiness()
    assert not readiness.observe(True, 0)
    assert not readiness.observe(True, 5.9)
    assert not readiness.observe(False, 6)  # A late startup modal resets readiness.
    assert not readiness.observe(True, 7)
    assert not readiness.observe(True, 12.9)
    assert readiness.observe(True, 13)
    assert foreground_fallback_key("  1. Run without daemon this time\n› 2. Cancel") == "up"
    assert foreground_fallback_key("› 1. Run without daemon this time\n  2. Cancel") == "enter"
    assert foreground_fallback_key("  1. Run without daemon this time\n  2. Cancel") is None
    assert foreground_fallback_key("  1. Run without daemon this time\n› 3. Other") is None
    assert local_editor_ready("OpenAI Codex\n› Write a message\n100% context left", "codex")
    assert local_editor_ready("Claude Code\n❯\nplan mode", "claude")
    assert not local_editor_ready("Connecting to remote machine", "codex")
    assert not local_editor_ready("Connecting to remote machine", "claude")
    print("exact prompt, scrollbar, comparison identity, Unicode cursor, and attached editor assertions passed")


class CursorScreen(Screen):
    def __init__(self, agent=None):
        self.agent = agent
        self.cursor_visible = True
        self.cursor_shape = None
        super().__init__()

    def csi(self, final):
        if self.sequence.startswith("?") and final in ("h", "l"):
            modes = self.sequence[1:].split(";")
            if "25" in modes:
                self.cursor_visible = final == "h"
        if final == "q":
            self.cursor_shape = self.sequence.strip()
        super().csi(final)

    def evidence(self, expected=""):
        rows = []
        if expected:
            for index, line in draft_rows(self.text(), expected, composed=True, agent=self.agent):
                start = line.index(expected)
                rows.append({"row": index, "start_col": start, "draft": line[start:start + len(expected)], "nearby_cells": line[max(0, start - 3):start + len(expected) + 3]})
        return {"outer_cursor": {"x": self.col, "y": self.row, "visible": self.cursor_visible, "shape": self.cursor_shape}, "generated_rows": rows}


class AgentClient(Client):
    def __init__(self, command, env, artifact_dir, agent=None):
        super().__init__(command, env, artifact_dir)
        # Replace logging before reading any child output. The original file
        # remains empty; no account info, prior sessions or startup text is saved.
        self.log.close()
        self.log = open(os.devnull, "wb")
        self.screen = CursorScreen(agent)

    def close(self):
        # Release the test PTY first so terminal-restore output cannot block a
        # shutting-down child while this harness is no longer pumping output.
        try:
            os.close(self.master)
        finally:
            self.log.close()
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=10)


def ssh_command(args, command, timeout=30):
    result = subprocess.run(["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", args.target, shlex.join(command)], capture_output=True, text=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f"Remote {command[0]} failed: {result.stderr.strip()}")
    return result.stdout


def wait_for_agent(client, remote, session, pane, agent, workdir, timeout=180):
    deadline = time.monotonic() + timeout
    trust_accepted = False
    last_state = "agent startup"
    readiness = StableEditorReadiness()
    while time.monotonic() < deadline:
        client.pump(0.1)
        screen = remote.cli("--session", session, "pane", "read", pane, "--source", "visible", "--format", "text")
        client.pump(0)
        lower = screen.lower()
        trust_dialog = any(phrase in lower for phrase in TRUST_DIALOG_PHRASES)
        if agent == "codex" and any(phrase in lower for phrase in ("cannot use background server", "cannot use the background server")) and FOREGROUND_FALLBACK_LABEL in screen:
            readiness.observe(False, time.monotonic())
            # Preserve requested sandbox/approval settings while declining an unavailable daemon.
            key = foreground_fallback_key(screen)
            if key is not None:
                remote.cli("--session", session, "pane", "send-keys", pane, key)
                last_state = "one-time foreground compatibility fallback"
            else:
                last_state = "waiting for recognized foreground compatibility selection"
            continue
        if agent == "claude" and "set auto mode as my default permission mode" in lower and "No, keep plan mode" in screen:
            readiness.observe(False, time.monotonic())
            if "❯ Yes, set auto mode" in screen:
                remote.cli("--session", session, "pane", "send-keys", pane, "down")
            elif "❯ No, keep plan mode" in screen:
                remote.cli("--session", session, "pane", "send-keys", pane, "enter")
            last_state = "declining automatic permission mode"
            continue
        if trust_dialog and not trust_accepted and workdir in screen:
            readiness.observe(False, time.monotonic())
            # Only the empty workspace created by this invocation is approved.
            if agent == "claude":
                if "❯ Yes, I trust this folder" in screen:
                    remote.cli("--session", session, "pane", "send-keys", pane, "enter")
                    trust_accepted = True
                    last_state = "generated temporary directory trust accepted"
                elif "❯ No, exit" in screen and "Yes, I trust this folder" in screen:
                    remote.cli("--session", session, "pane", "send-keys", pane, "down")
                    last_state = "selecting generated temporary directory trust"
                continue
            if "› 1. Yes" in screen or "› 1. Trust and continue" in screen or "❯ Yes" in screen:
                remote.cli("--session", session, "pane", "send-keys", pane, "enter")
                trust_accepted = True
                last_state = "generated temporary directory trust accepted"
            continue
        if trust_dialog:
            readiness.observe(False, time.monotonic())
            last_state = "waiting for generated directory trust dialog to close"
            continue
        if "sign in" in lower and any(phrase in lower for phrase in LOGIN_METHOD_PHRASES):
            readiness.observe(False, time.monotonic())
            raise RuntimeError(f"{agent} requires authentication before its input editor is available")
        remote_ready = False
        editor_geometry = None
        if agent == "claude" and ("claude code" in lower or "opus" in lower or "sonnet" in lower) and "❯" in screen and any(marker in lower for marker in ("for shortcuts", "plan mode", "bypass permissions", "shift+tab")):
            remote_ready = True
        elif agent == "codex" and "openai codex" in lower and "›" in screen and ("for shortcuts" in lower or "context left" in lower):
            remote_ready = True
        elif agent == "opencode" and "Ask anything..." in screen and "tab agents" in lower and "ctrl+p commands" in lower:
            remote_ready = True
        elif agent == "pi" and workdir in screen:
            editor_rows = pi_editor_rows(screen, workdir)
            if len(editor_rows) == 1:
                remote_ready = True
                editor_geometry = {"authoritative_row": editor_rows[0], "input_column": 0, "layout": "plain body between adjacent horizontal rules"}
        local_screen = client.screen.text()
        local_lower = local_screen.lower()
        local_modal = FOREGROUND_FALLBACK_LABEL in local_screen or any(phrase in local_lower for phrase in TRUST_DIALOG_PHRASES) or "set auto mode as my default permission mode" in local_lower or ("sign in" in local_lower and any(phrase in local_lower for phrase in LOGIN_METHOD_PHRASES))
        if readiness.observe(remote_ready and local_editor_ready(local_screen, agent) and not local_modal, time.monotonic()):
            result = {"startup_state": last_state, "trusted_generated_directory": trust_accepted, "editor_stable_seconds": EDITOR_STABLE_SECONDS}
            if editor_geometry is not None:
                result["editor_geometry"] = editor_geometry
            return result
        if "update" in lower and "skip for now" in lower:
            last_state = "agent update prompt requires skipping"
    # Save only recognizable state labels, not arbitrary startup/account text.
    raise RuntimeError(f"{agent} editor was not ready: {last_state}")


def sample_key(client, expected, char):
    expected += char
    started = time.monotonic()
    os.write(client.master, char.encode("utf-8"))
    client.wait_for(lambda screen: bool(draft_rows(screen, expected, composed=True, agent=client.screen.agent)), 8, f"generated draft character {len(expected)}")
    visible = time.monotonic()
    evidence = client.screen.evidence(expected)
    # Allow authoritative repaint to settle without issuing another SSH/API
    # command (which could expire the predictor's short confidence window).
    settle = time.monotonic() + 0.28
    while time.monotonic() < settle:
        client.pump(0.02)
    return expected, {"character": char, "visible_ms": round((visible - started) * 1000, 2), "first_visible": evidence, "settled": client.screen.evidence(expected)}


def exact_draft_cursor(screen, expected, cursor_index):
    rows = draft_rows(screen.text(), expected, composed=True, agent=screen.agent)
    if len(rows) != 1:
        return False
    row, line = rows[0]
    return screen.row == row and screen.col == line.index(expected) + cursor_index


def editing_probes(client, expected, result):
    cursor = len(expected)
    client.wait_for(lambda _: exact_draft_cursor(client.screen, expected, cursor), 8, "initial generated draft cursor")
    result.update(samples=[], expected_draft=expected)

    def edit(operation, data, next_draft, next_cursor, *, training=False):
        nonlocal expected, cursor
        if not next_draft or next_draft[-1].isspace():
            raise ValueError("Editing probes require a nonempty draft without trailing whitespace")
        sample = {"operation": operation, "training": training, "expected_draft": next_draft, "cursor_index": next_cursor, "before": client.screen.evidence(expected)}
        result["samples"].append(sample)
        expected, cursor = next_draft, next_cursor
        result["expected_draft"] = expected
        started = time.monotonic()
        os.write(client.master, data)
        client.wait_for(lambda _: exact_draft_cursor(client.screen, expected, cursor), 8, f"exact {operation} draft and cursor")
        sample.update(visible_ms=round((time.monotonic() - started) * 1000, 2), first_visible=client.screen.evidence(expected))
        settle = time.monotonic() + 0.28
        while time.monotonic() < settle:
            client.pump(0.02)
        sample["settled"] = client.screen.evidence(expected)

    def move(operation):
        next_cursor = len(expected) if operation == EditOperation.END else cursor + (1 if operation == EditOperation.RIGHT else -1)
        edit(operation, EDIT_KEYS[operation], expected, next_cursor)

    def insert(text, operation=EditOperation.INSERT):
        edit(operation, text.encode("utf-8"), expected[:cursor] + text + expected[cursor:], cursor + len(text))

    def erase(operation):
        if operation == EditOperation.DELETE:
            edit(operation, EDIT_KEYS[operation], expected[:cursor] + expected[cursor + 1:], cursor)
        else:
            edit(operation, EDIT_KEYS[operation], expected[:cursor - 1] + expected[cursor:], cursor - 1)

    for operation in (EditOperation.LEFT, EditOperation.LEFT, EditOperation.RIGHT, EditOperation.RIGHT, EditOperation.LEFT):
        move(operation)
    insert("X")
    erase(EditOperation.DELETE)
    move(EditOperation.LEFT)
    insert("Y")
    erase(EditOperation.BACKSPACE)
    move(EditOperation.RIGHT)
    erase(EditOperation.BACKSPACE)
    for _ in range(2):
        move(EditOperation.LEFT)
        move(EditOperation.END)
    for char in "éλ":
        insert(char, EditOperation.UNICODE)
    move(EditOperation.LEFT)
    erase(EditOperation.BACKSPACE)
    erase(EditOperation.DELETE)

    prefix = expected
    insert(" alpha tail", EditOperation.FIXTURE)
    for _ in " tail":
        move(EditOperation.LEFT)
    for operation, word in ((EditOperation.CTRL_W, "alpha"), (EditOperation.ALT_BACKSPACE, "beta")):
        if operation == EditOperation.ALT_BACKSPACE:
            insert(word, EditOperation.FIXTURE)
        for training in (True, False):
            edit(operation, EDIT_KEYS[operation], prefix + "  tail", len(prefix) + 1, training=training)
            if training:
                # The first unlearned deletion must settle before repeating it.
                settle = time.monotonic() + 0.4
                while time.monotonic() < settle:
                    client.pump(0.02)
                insert(word, EditOperation.FIXTURE)

    result["mouse"] = {"status": "unsupported", "reason": "hardware cursor hidden"}
    if client.screen.cursor_visible:
        before = client.screen.evidence(expected)
        generated_row = before["generated_rows"][0]
        click_cursor = 2
        column, row = generated_row["start_col"] + click_cursor + 1, generated_row["row"] + 1
        started = time.monotonic()
        os.write(client.master, f"\x1b[<0;{column};{row}M\x1b[<0;{column};{row}m".encode("ascii"))
        deadline = started + 2
        while not exact_draft_cursor(client.screen, expected, click_cursor) and time.monotonic() < deadline:
            client.pump(0.02)
        if exact_draft_cursor(client.screen, expected, click_cursor):
            cursor = click_cursor
            result["mouse"] = {"status": "supported", "visible_ms": round((time.monotonic() - started) * 1000, 2), "before": before, "first_visible": client.screen.evidence(expected)}
            settle = time.monotonic() + 0.55
            while time.monotonic() < settle:
                client.pump(0.02)
            column += 1
            edit(EditOperation.MOUSE, f"\x1b[<0;{column};{row}M\x1b[<0;{column};{row}m".encode("ascii"), expected, cursor + 1)
        else:
            result["mouse"] = {"status": "unsupported", "reason": "same-row click did not move the cursor to the exact draft cell", "before": before, "after": client.screen.evidence(expected)}
    move(EditOperation.END)
    insert("z")
    measured = [sample for sample in result["samples"] if not sample["training"] and sample["operation"] != EditOperation.FIXTURE]
    result[EDITING_MEDIANS_KEY] = {
        operation: round(statistics.median(sample["visible_ms"] for sample in measured if sample["operation"] == operation), 2)
        for operation in EditOperation if any(sample["operation"] == operation for sample in measured)
    }
    result["burst"] = {"before": client.screen.evidence(expected), "sequence": "mnop Left Left Backspace é Right Delete End z", "no_per_key_settle": True}
    expected += "méoz"
    cursor = len(expected)
    result["expected_draft"] = expected
    result["burst"].update(expected_draft=expected, cursor_index=cursor)
    burst = b"mnop" + EDIT_KEYS[EditOperation.LEFT] * 2 + EDIT_KEYS[EditOperation.BACKSPACE] + "é".encode("utf-8") + EDIT_KEYS[EditOperation.RIGHT] + EDIT_KEYS[EditOperation.DELETE] + EDIT_KEYS[EditOperation.END] + b"z"
    started = time.monotonic()
    os.write(client.master, burst)
    client.wait_for(lambda _: exact_draft_cursor(client.screen, expected, cursor), 8, "rapid composed editing burst draft and cursor")
    result["burst"].update(visible_ms=round((time.monotonic() - started) * 1000, 2), first_visible=client.screen.evidence(expected))
    settle = time.monotonic() + 0.28
    while time.monotonic() < settle:
        client.pump(0.02)
    result["burst"]["settled"] = client.screen.evidence(expected)
    return expected


def run_case(args, remote, agent, prediction, session, artifact_dir):
    case_dir = artifact_dir / f"{agent}-{'on' if prediction else 'off'}"
    case_dir.mkdir()
    config = case_dir / "config.toml"
    config.write_text(f"onboarding = false\n[remote]\npredict_input = {str(prediction).lower()}\nmanage_ssh_config = false\n", encoding="utf-8")
    env = {key: value for key, value in os.environ.items() if not key.startswith("HERDR_")}
    env.update(TERM="xterm-256color", COLORTERM="truecolor", HERDR_CONFIG_PATH=str(config), XDG_CONFIG_HOME=str(case_dir / "config"), XDG_STATE_HOME=str(case_dir / "state"), HERDR_LOG="herdr::client::shell::prediction=debug")
    command = [str(Path(args.binary).resolve()), "--remote", args.target, "--session", session]
    binary_sha256 = binary_digest(args.binary)
    client = AgentClient(command, env, case_dir, agent)
    pane, workdir, expected = None, None, ""
    case = {"agent": agent, "prediction": prediction, "session": session, "candidate_command": command, "binary_sha256": binary_sha256, "samples": [], "submitted_prompt": False}
    try:
        deadline = time.monotonic() + 45
        while True:
            client.pump(0)
            try:
                remote.json("--session", session, "workspace", "list")
                break
            except RuntimeError:
                if client.process.poll() is not None or time.monotonic() >= deadline:
                    raise
                client.pump(0.2)
        workdir = ssh_command(args, ["mktemp", "-d", "/tmp/herdr-agent-smoke.XXXXXXXX"]).strip()
        if not workdir.startswith("/tmp/herdr-agent-smoke.") or "/" in workdir[len("/tmp/"):]:
            raise RuntimeError("Unexpected generated workspace path")
        response = remote.json("--session", session, "workspace", "create", "--cwd", workdir, "--label", f"smoke-{agent}", "--focus")
        pane = response["result"]["root_pane"]["pane_id"]
        case.update(pane=pane, generated_workdir=workdir)
        agent_command = ["env", f"PATH={args.agent_bin_dir}:/usr/local/bin:/usr/bin:/bin", str(Path(args.agent_bin_dir) / agent)]
        # Record each installed version separately: the remote user may update
        # an agent between validation runs. This is outside the timing window.
        case["agent_version"] = ssh_command(args, [*agent_command, "--version"]).strip()
        if agent == "claude":
            agent_command += ["--safe-mode", "--permission-mode", "plan"]
        elif agent == "codex":
            agent_command += ["--sandbox", "read-only", "--ask-for-approval", "on-request", "--cd", workdir]
        elif agent == "pi":
            agent_command += ["--no-session"]
        elif agent == "opencode":
            agent_command += [workdir]
        case["agent_command"] = agent_command
        remote.cli("--session", session, "pane", "run", pane, shlex.join(agent_command))
        case.update(wait_for_agent(client, remote, session, pane, agent, workdir))
        client.wait_for(lambda screen: local_editor_ready(screen, agent), 30, "attached client editor after remote startup")
        settle = time.monotonic() + 1
        while time.monotonic() < settle:
            client.pump(0.05)
        case["detected_agent"] = own_pane_agent_metadata(remote.json("--session", session, "agent", "list")["result"]["agents"], pane, agent)
        case["initial"] = client.screen.evidence()
        for index, char in enumerate("ZQherdrprobeabcdefghijklmno"[:args.samples + 2]):
            expected, sample = sample_key(client, expected, char)
            sample["training"] = index < 2
            case["samples"].append(sample)
            (case_dir / "case.json").write_text(json.dumps(case, indent=2) + "\n", encoding="utf-8")
        pause_start = time.monotonic()
        while time.monotonic() - pause_start < 1.5:
            client.pump(0.05)
        pause_ms = round((time.monotonic() - pause_start) * 1000, 2)
        expected, paused_sample = sample_key(client, expected, "p")
        case["pause_probe"] = {"idle_ms": pause_ms, **paused_sample}
        last_char = expected[-1]
        expected = expected[:-1]
        deletion_start = time.monotonic()
        os.write(client.master, b"\x7f")
        client.wait_for(lambda screen: bool(draft_rows(screen, expected, composed=True, agent=client.screen.agent)), 8, "exact draft after one Backspace")
        deleted = {"visible_ms": round((time.monotonic() - deletion_start) * 1000, 2), "screen": client.screen.evidence(expected)}
        expected, retyped = sample_key(client, expected, last_char)
        case["delete_retype"] = {"deletion": deleted, "retyped": retyped}
        suffix = expected[-min(4, len(expected) - 1):]
        backspaces = []
        for _ in suffix:
            expected = expected[:-1]
            started = time.monotonic()
            os.write(client.master, b"\x7f")
            client.wait_for(lambda screen: bool(draft_rows(screen, expected, composed=True, agent=agent)), 8, "exact draft during repeated Backspace")
            backspaces.append({"visible_ms": round((time.monotonic() - started) * 1000, 2), "screen": client.screen.evidence(expected)})
        case["backspace_samples"] = backspaces
        case[BACKSPACE_MEDIAN_KEY] = round(statistics.median(sample["visible_ms"] for sample in backspaces), 2)
        for char in suffix:
            expected, _ = sample_key(client, expected, char)
        if args.editing:
            case["editing"] = {}
            expected = editing_probes(client, expected, case["editing"])
        authoritative = remote.cli("--session", session, "pane", "read", pane, "--source", "visible", "--format", "text")
        generated = []
        for index, line in draft_rows(authoritative, expected, agent=agent):
            start = line.index(expected)
            generated.append({"row": index, "start_col": start, "draft": line[start:start + len(expected)], "nearby_cells": line[max(0, start - 3):start + len(expected) + 3]})
        if len(generated) != 1:
            raise RuntimeError("Expected exactly one authoritative prompt containing exactly the generated draft")
        case["authoritative_generated_rows"] = generated
        if args.editing:
            case["editing"]["burst"]["passed_authoritative_integrity"] = True
        case["median_visible_ms"] = round(statistics.median(sample["visible_ms"] for sample in case["samples"] if not sample["training"]), 2)
        case["passed_input_integrity"] = True
    except Exception as error:
        case.update(error=str(error), passed_input_integrity=False)
    finally:
        if "editing" in case and "expected_draft" in case["editing"]:
            expected = case["editing"]["expected_draft"]
        case["final"] = client.screen.evidence(expected)
        cleanup_errors = []
        try:
            client.close()
        except Exception as error:
            cleanup_errors.append(f"local client {client.process.pid}: {error}")
        try:
            sessions = remote.json("session", "list", "--json")["sessions"]
            if any(item["name"] == session for item in sessions):
                remote.cli("session", "stop", session, "--json")
                remote.cli("session", "delete", session, "--json")
        except Exception as error:
            cleanup_errors.append(f"named session {session}: {error}")
        try:
            if workdir and workdir.startswith("/tmp/herdr-agent-smoke.") and "/" not in workdir[len("/tmp/"):]:
                ssh_command(args, ["rm", "-rf", "--", workdir])
        except Exception as error:
            cleanup_errors.append(f"generated workspace {workdir}: {error}")
        case["cleanup_errors"] = cleanup_errors
        (case_dir / "case.json").write_text(json.dumps(case, indent=2) + "\n", encoding="utf-8")
    return case


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--target")
    parser.add_argument("--transport", choices=("ssh", "kube-exec"), default="ssh")
    parser.add_argument("--binary", default="target/release/herdr")
    parser.add_argument("--remote-binary", default=".local/bin/herdr")
    parser.add_argument("--agent-bin-dir", help="explicit absolute remote directory containing the agent executables")
    parser.add_argument("--agents", nargs="+", choices=TEST_AGENTS, default=["claude", "codex"])
    parser.add_argument("--prediction", choices=("off", "on", "both"), default="both")
    parser.add_argument("--samples", type=int, default=5)
    parser.add_argument("--editing", action="store_true", help="also measure cursor movement, midline edits, Unicode, learned word deletion, and supported mouse clicks")
    parser.add_argument("--artifacts", type=Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return 0
    if not args.target:
        parser.error("--target is required for live agent validation")
    if not args.agent_bin_dir or not Path(args.agent_bin_dir).is_absolute():
        parser.error("--agent-bin-dir must be an explicit absolute remote directory")
    if not Path(args.binary).is_file() or not 1 <= args.samples <= 20:
        parser.error("binary must exist and samples must be 1-20")
    if args.target.startswith("-") or any(ch.isspace() for ch in args.target):
        parser.error("target must be one SSH host/alias")
    artifacts = args.artifacts.resolve() if args.artifacts else Path(tempfile.mkdtemp(prefix="herdr-real-agents-"))
    artifacts.mkdir(parents=True, exist_ok=True)
    remote = Remote(args.target, args.remote_binary)
    existing = {item["name"] for item in remote.json("session", "list", "--json")["sessions"]}
    token = uuid.uuid4().hex[:10]
    predictions = (False, True) if args.prediction == "both" else (args.prediction == "on",)
    report = {"transport": args.transport, "target": args.target, "artifacts": str(artifacts), "cases": [], "submitted_prompts": False, "editing": args.editing}
    print(f"Real agent draft smoke artifacts: {artifacts}", flush=True)
    for agent in args.agents:
        for prediction in predictions:
            session = f"agent-smoke-{token}-{agent}-{'on' if prediction else 'off'}"
            if session in existing:
                raise RuntimeError("Generated test session already exists")
            print(f"Running {agent}, prediction={prediction}, session={session}", flush=True)
            case = run_case(args, remote, agent, prediction, session, artifacts)
            report["cases"].append(case)
            (artifacts / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
            print(json.dumps({key: case[key] for key in ("agent", "prediction", "median_visible_ms", "error", "passed_input_integrity") if key in case}), flush=True)
    report["passed"] = all(case.get("passed_input_integrity") and not case.get("cleanup_errors") for case in report["cases"])
    if args.prediction == "both" and report["passed"]:
        report["comparisons"] = []
        for agent in args.agents:
            baseline = next(case for case in report["cases"] if case["agent"] == agent and not case["prediction"])
            predicted = next(case for case in report["cases"] if case["agent"] == agent and case["prediction"])
            same_build = same_comparison_build(baseline, predicted)
            improvement = predicted["median_visible_ms"] < baseline["median_visible_ms"] * 0.5
            pause_improvement = predicted["pause_probe"]["visible_ms"] < baseline["pause_probe"]["visible_ms"] * 0.5
            backspace_improvement = predicted[BACKSPACE_MEDIAN_KEY] < baseline[BACKSPACE_MEDIAN_KEY] * 0.5
            report["comparisons"].append({"agent": agent, "same_binary_and_agent_version": same_build, "baseline_ms": baseline["median_visible_ms"], "prediction_ms": predicted["median_visible_ms"], "prediction_improved": improvement, "pause_baseline_ms": baseline["pause_probe"]["visible_ms"], "pause_prediction_ms": predicted["pause_probe"]["visible_ms"], "pause_improved": pause_improvement, "backspace_baseline_ms": baseline[BACKSPACE_MEDIAN_KEY], "backspace_prediction_ms": predicted[BACKSPACE_MEDIAN_KEY], "backspace_improved": backspace_improvement})
            report["passed"] = report["passed"] and same_build and improvement and pause_improvement and backspace_improvement
            if args.editing:
                baseline_edits = baseline["editing"][EDITING_MEDIANS_KEY]
                predicted_edits = predicted["editing"][EDITING_MEDIANS_KEY]
                comparisons = [
                    {"operation": operation, "baseline_ms": baseline_edits[operation], "prediction_ms": predicted_edits[operation], "prediction_improved": predicted_edits[operation] < baseline_edits[operation] * 0.5}
                    for operation in EditOperation if operation in baseline_edits and operation in predicted_edits
                ]
                report["comparisons"][-1]["editing"] = comparisons
                report["comparisons"][-1]["editing_burst"] = {"baseline_ms": baseline["editing"]["burst"]["visible_ms"], "prediction_ms": predicted["editing"]["burst"]["visible_ms"], "passed_authoritative_integrity": baseline["editing"]["burst"]["passed_authoritative_integrity"] and predicted["editing"]["burst"]["passed_authoritative_integrity"]}
                report["passed"] = report["passed"] and all(comparison["prediction_improved"] for comparison in comparisons)
    (artifacts / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({key: report[key] for key in ("artifacts", "passed", "comparisons") if key in report}, indent=2))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
