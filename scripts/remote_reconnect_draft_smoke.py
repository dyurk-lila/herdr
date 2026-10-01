#!/usr/bin/env python3
"""Verify local reconnect drafts in disposable native SSH agent sessions.

Uses an existing remote host, generated empty directories, and unsubmitted
drafts. Only a test client's owned SSH bridge is interrupted. Raw agent screens
are discarded; artifacts remain private and include target/path metadata.

    python3 scripts/remote_reconnect_draft_smoke.py --target test-host --agent-bin-dir /opt/agents/bin --mode both
"""

import argparse
from concurrent.futures import ThreadPoolExecutor
from dataclasses import asdict, dataclass
from enum import StrEnum
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
from threading import Event
import time
import unicodedata
import uuid

from remote_agent_latency_smoke import (
    AgentClient,
    CursorScreen,
    EDIT_KEYS,
    EditOperation,
    binary_digest,
    draft_rows as agent_draft_rows,
    own_pane_agent_metadata,
    sample_key,
    wait_for_agent,
)
from remote_latency_smoke import Remote, WIDTH, bridge_children

PANEL_TITLE = "Reconnect draft"
PANEL_QUEUED = "Queued locally"
BASE_DRAFT = "ZQreconnect"
CHANGED_DRAFT = "ZQchanged"
BURST_SUFFIX = "méoz"
SECOND_SUFFIX = "λnext"
WIDE_SUFFIX = "界🧪λ"
PREFIX_KEY = b"\x02"
ESCAPE_KEY = b"\x1b"
BURST = (
    b"mnop" + EDIT_KEYS[EditOperation.LEFT] * 2
    + EDIT_KEYS[EditOperation.BACKSPACE] + "é".encode()
    + EDIT_KEYS[EditOperation.RIGHT] + EDIT_KEYS[EditOperation.DELETE]
    + EDIT_KEYS[EditOperation.END] + b"z"
)
EXPECTED_FAILURES = (OSError, RuntimeError, ValueError, subprocess.SubprocessError)
MAX_DIAGNOSTIC_RECORDS = 64
MISMATCH_BOOL_FIELDS = (
    "boot_changed", "agent_changed", "geometry_changed", "size_changed",
    "modes_changed", "cursor_visibility_changed", "cursor_changed",
    "bounds_changed", "row_length_changed",
)
MISMATCH_COUNT_FIELDS = (
    "symbol_changes", "style_changes", "skip_changes", "hyperlink_changes",
    "outside_input_changes",
)
LOG_FIELD = re.compile(r'(?<!\S)([a-z_]+)=(?:"([^"]*)"|([^\s]+))(?=\s|$)')


class Scenario(StrEnum):
    RESTORE = "restore"
    SECOND_DROP = "second-drop"
    CHANGED_CONTEXT = "changed-context"
    CLIENT_RESET = "client-reset"
    WIDE_UNICODE = "wide-unicode"


class BridgePhase(StrEnum):
    GATED = "gated"
    RELEASED = "released"


class MismatchPhase(StrEnum):
    ATTEMPT = "attempt"
    OBSERVE = "observe"
    ECHO = "echo"


class RestoreFailure(RuntimeError):
    def __init__(self, diagnosis):
        super().__init__("Restored editor did not match the generated draft and cursor")
        self.diagnosis = diagnosis


@dataclass
class BridgeEvent:
    pid: int
    parent_pid: int
    session: str
    phase: BridgePhase
    timestamp_ns: int


def parse_reconnect_diagnostics(lines):
    result = {"context_mismatches": [], "echo_timeouts": 0, "malformed_records": 0, "omitted_records": 0}
    for line in lines:
        fields = {match[1]: match[2] if match[2] is not None else match[3] for match in LOG_FIELD.finditer(line)}
        if "event" not in fields:
            continue
        if fields["event"] == "reconnect.echo_timeout":
            result["echo_timeouts"] += 1
            continue
        if fields["event"] == "reconnect.context_mismatch":
            required = ("phase", *MISMATCH_BOOL_FIELDS, *MISMATCH_COUNT_FIELDS)
            if any(name not in fields for name in required):
                result["malformed_records"] += 1
                continue
            if fields["phase"] not in tuple(MismatchPhase) or any(fields[name] not in ("true", "false") for name in MISMATCH_BOOL_FIELDS) or any(re.fullmatch(r"[0-9]+", fields[name]) is None for name in MISMATCH_COUNT_FIELDS):
                result["malformed_records"] += 1
                continue
            record = {"phase": fields["phase"]}
            record.update({name: fields[name] == "true" for name in MISMATCH_BOOL_FIELDS})
            record.update({name: int(fields[name]) for name in MISMATCH_COUNT_FIELDS})
        else:
            continue
        if len(result["context_mismatches"]) >= MAX_DIAGNOSTIC_RECORDS:
            result["context_mismatches"].pop(0)
            result["omitted_records"] += 1
        result["context_mismatches"].append(record)
    return result


def collect_reconnect_diagnostics(case_dir):
    paths = list((case_dir / "config").rglob("herdr-client.log"))
    if len(paths) > 1:
        raise RuntimeError("Expected at most one owned native-client diagnostic log")
    if not paths:
        return parse_reconnect_diagnostics(())
    path = paths[0]
    try:
        if path.is_symlink() or path.stat().st_size > 8 * 1024 * 1024:
            raise RuntimeError("Owned native-client diagnostic log exceeded its bounds")
        with path.open(encoding="utf-8") as lines:
            return parse_reconnect_diagnostics(lines)
    finally:
        path.unlink(missing_ok=True)


class BridgeGate:
    def __init__(self, root, mux_dir, target, real_ssh):
        self.root = root / "ssh-control"
        self.root.mkdir(mode=0o700)
        self.gate = self.root / "gate"
        self.active = self.root / "active-session.json"
        self.events = self.root / "events.jsonl"
        self.mux_dir = mux_dir
        self.target = target
        self.real_ssh = real_ssh
        self.wrapper = self.root / "ssh"
        self.wrapper.write_text(self.wrapper_source(), encoding="utf-8")
        self.wrapper.chmod(0o700)

    def wrapper_source(self):
        return f'''#!{sys.executable}
import json
import os
from pathlib import Path
import sys
import time
gate = Path({str(self.gate)!r})
active = Path({str(self.active)!r})
events = Path({str(self.events)!r})
args = sys.argv[1:]
if {self.target!r} in args and any("remote-client-bridge" in arg for arg in args):
    session = json.loads(active.read_text())["session"]
    if not any(session in arg and "remote-client-bridge" in arg for arg in args):
        raise SystemExit("test bridge has unexpected session")
    parent = os.getppid()
    def event(phase):
        record = {{"pid":os.getpid(),"parent_pid":parent,"session":session,"phase":phase,"timestamp_ns":time.monotonic_ns()}}
        with events.open("a", encoding="utf-8") as out:
            out.write(json.dumps(record)+"\\n")
    if gate.exists():
        event({str(BridgePhase.GATED)!r})
        deadline = time.monotonic()+45
        while gate.exists():
            if os.getppid()!=parent or time.monotonic()>deadline:
                raise SystemExit("test bridge gate expired or owner exited")
            time.sleep(.02)
    event({str(BridgePhase.RELEASED)!r})
env = os.environ.copy()
env.pop("XDG_CONFIG_HOME", None)
env.pop("XDG_STATE_HOME", None)
os.execve({str(self.real_ssh)!r}, [{str(self.real_ssh)!r}, "-o", "ControlMaster=auto", "-o", {"ControlPath=" + str(self.mux_dir / "%C")!r}, "-o", "ControlPersist=120", *args], env)
'''

    def select(self, session):
        self.active.write_text(json.dumps({"session": session}), encoding="utf-8")

    def release(self):
        self.gate.unlink(missing_ok=True)

    def read_events(self):
        if not self.events.exists():
            return []
        if self.events.stat().st_size > 65536:
            raise RuntimeError("Test bridge event log exceeded its bound")
        records = []
        for line in self.events.read_text().splitlines(keepends=True):
            if not line.endswith("\n"):
                continue
            record = BridgeEvent(**json.loads(line))
            record.phase = BridgePhase(record.phase)
            records.append(record)
        return records

    def interrupt(self, client, session):
        candidates = bridge_children(client.process.pid)
        if len(candidates) != 1:
            raise RuntimeError(f"Expected one owned SSH bridge; found {len(candidates)}")
        pid, command = next(iter(candidates.items()))
        self.gate.touch(mode=0o600)
        started = time.monotonic_ns()
        if pid not in bridge_children(client.process.pid) or bridge_children(client.process.pid)[pid] != command:
            raise RuntimeError("Owned bridge changed before interruption")
        os.kill(pid, signal.SIGTERM)
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            client.pump(.1)
            events = [event for event in self.read_events() if event.session == session and event.parent_pid == client.process.pid and event.phase == BridgePhase.GATED and event.timestamp_ns >= started]
            if events and "reconnecting" in client.screen.text().lower():
                return {"interrupted_bridge_pid": pid, "gated_replacement_pid": events[-1].pid, "reconnecting_observed": True}
        raise RuntimeError("Owned replacement bridge did not enter the reconnect gate")

    def close_mux(self):
        self.release()
        result = subprocess.run([str(self.real_ssh), "-o", f"ControlPath={self.mux_dir / '%C'}", "-O", "exit", self.target], capture_output=True, text=True, timeout=15)
        if result.returncode and not any(phrase in result.stderr for phrase in ("No such file or directory", "Connection refused")):
            raise RuntimeError(f"Owned SSH mux cleanup failed: {result.stderr.strip()}")


def ssh_text(target, command):
    result = subprocess.run(["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", target, shlex.join(command)], capture_output=True, text=True, timeout=45)
    if result.returncode:
        raise RuntimeError(f"Remote diagnostic command failed: {result.stderr.strip()}")
    return result.stdout


def authoritative(remote, session, pane, *, timeout=45):
    return remote.cli("--session", session, "pane", "read", pane, "--source", "visible", "--format", "text", timeout=timeout)


def require_authoritative(remote, session, pane, expected, agent):
    rows = draft_rows(authoritative(remote, session, pane), expected, agent=agent)
    if len(rows) != 1:
        raise RuntimeError("Authoritative editor did not contain exactly the expected generated draft")
    return {"generated_draft": expected, "exact_row_count": len(rows)}


def display_width(text):
    return sum(0 if unicodedata.combining(char) else 2 if unicodedata.east_asian_width(char) in "WF" else 1 for char in text)


def draft_rows(source, expected, *, agent, composed=False):
    if composed:
        source = "\n".join(
            line[:-1] if line.endswith(" ▐") and display_width(line) == WIDTH else line
            for line in source.splitlines()
        )
    return agent_draft_rows(source, expected, composed=composed, agent=agent)


def exact_draft_cursor(screen, expected):
    rows = draft_rows(screen.text(), expected, composed=True, agent=screen.agent)
    if len(rows) != 1:
        return False
    row, line = rows[0]
    start = display_width(line[:line.index(expected)])
    return screen.row == row and screen.col == start + display_width(expected)


def generated_evidence(client, expected):
    rows = draft_rows(client.screen.text(), expected, composed=True, agent=client.screen.agent)
    return {
        "outer_cursor": client.screen.evidence()["outer_cursor"],
        "generated_rows": [{"row": row, "start_col": display_width(line[:line.index(expected)]), "draft": expected} for row, line in rows],
    }


def read_authoritative_while_pumping(worker, client, remote, session, pane, deadline):
    started = time.monotonic()
    # Remote retries only a failed pre-authentication banner, at most once.
    timeout = max(.01, (deadline - started) / 2)
    result = worker.submit(authoritative, remote, session, pane, timeout=timeout)
    while not result.done():
        client.pump(.1)
    source = result.result()
    return source, (time.monotonic() - started) * 1000


def wait_restored(client, remote, session, pane, expected, agent, *, cursor_at_end=True, deadline=None, recovery_suffix=BURST_SUFFIX):
    if deadline is None:
        deadline = time.monotonic() + 45
    max_control_read_ms = 0
    with ThreadPoolExecutor(max_workers=1) as worker:
        while True:
            client.pump(.1)
            source, read_ms = read_authoritative_while_pumping(worker, client, remote, session, pane, deadline)
            max_control_read_ms = max(max_control_read_ms, read_ms)
            rows = draft_rows(source, expected, agent=agent)
            client.pump(0)
            local_rows = draft_rows(client.screen.text(), expected, composed=True, agent=agent)
            cursor_matches = not cursor_at_end or exact_draft_cursor(client.screen, expected)
            timed_out = time.monotonic() >= deadline
            if not timed_out and len(rows) == 1 and len(local_rows) == 1 and cursor_matches:
                return {"generated_draft": expected, "exact_row_count": 1, "local": generated_evidence(client, expected), "max_control_read_ms": round(max_control_read_ms, 2)}
            if timed_out:
                candidates = (BASE_DRAFT, BASE_DRAFT + BURST_SUFFIX, BASE_DRAFT + BURST_SUFFIX + SECOND_SUFFIX, CHANGED_DRAFT, expected)
                raise RestoreFailure({
                    "expected_draft": expected,
                    "expected_authoritative_row_count": len(rows),
                    "expected_local_row_count": len(local_rows),
                    "local_cursor_matches": cursor_matches,
                    "authoritative_generated_matches": {
                        draft: len(draft_rows(source, draft, agent=agent))
                        for draft in dict.fromkeys(candidates)
                    },
                    "local_cursor": client.screen.evidence()["outer_cursor"],
                    "panel": panel_state(client, recovery_suffix),
                    "max_control_read_ms": round(max_control_read_ms, 2),
                })


def panel_state(client, suffix):
    screen = client.screen.text()
    return {"present": PANEL_TITLE in screen, "queued_label_present": PANEL_QUEUED in screen, "generated_suffix_visible": suffix in screen, "copy_recovery_label_present": "copy to recover" in screen.lower(), "copy_button_visible": "[Copy]" in screen, "discard_button_visible": "[Discard]" in screen}


def exact_held_suffix(client, suffix):
    lines = client.screen.text().splitlines()
    headers = [(row, line) for row, line in enumerate(lines[:-1]) if PANEL_TITLE in line and "[Discard]" in line]
    if len(headers) != 1:
        return False
    row, header = headers[0]
    start = display_width(header[:header.index(PANEL_TITLE)])
    end = display_width(header[:header.index("[Discard]") + len("[Discard]")])
    text = "".join(client.screen.cells[row + 1][start:end]).strip(" \u00a0")
    return text == suffix


def wait_panel_retired(client, started):
    remaining = started + 45 - time.monotonic()
    if remaining <= 0:
        raise RuntimeError("Reconnect handoff exceeded the shared restoration deadline")
    client.wait_for(lambda screen: PANEL_TITLE not in screen, remaining, "automatic reconnect handoff without a copy click")
    return {"panel_absent": True, "copy_clicked": False, "retirement_ms": round((time.monotonic() - started) * 1000, 2)}


def offline_burst(client, enabled, scenario):
    wide = scenario == Scenario.WIDE_UNICODE
    suffix = WIDE_SUFFIX if wide else BURST_SUFFIX
    sequence = f"type {WIDE_SUFFIX} once" if wide else "mnop Left Left Backspace é Right Delete End z"
    started = time.monotonic()
    os.write(client.master, WIDE_SUFFIX.encode() if wide else BURST)
    if enabled:
        client.wait_for(lambda screen: PANEL_TITLE in screen and suffix in screen, 8, "local reconnect draft suffix")
    else:
        until = time.monotonic() + .4
        while time.monotonic() < until:
            client.pump(.05)
    return {"sequence": sequence, "expected_suffix": suffix, "visible_ms": round((time.monotonic() - started) * 1000, 2), "panel": panel_state(client, suffix)}


def settle(client, seconds=.6):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        client.pump(.02)


def run_case(args, gate, remote, agent, enabled, scenario, root):
    session = f"reconnect-draft-{uuid.uuid4().hex[:10]}-{agent}"
    existing = remote.json("session", "list", "--json")["sessions"]
    if any(item["name"] == session for item in existing):
        raise RuntimeError("Generated diagnostic session already exists")
    case_dir = root / f"{agent}-{'on' if enabled else 'off'}-{scenario}"
    case_dir.mkdir(mode=0o700)
    config = case_dir / "config.toml"
    config.write_text(f'onboarding=false\n[keys]\nprefix="ctrl+b"\n[remote]\npredict_input=true\nbuffer_reconnect_input={str(enabled).lower()}\nmanage_ssh_config=false\n', encoding="utf-8")
    gate.select(session)
    env = {key: value for key, value in os.environ.items() if not key.startswith("HERDR_")}
    env.update(TERM="xterm-256color", COLORTERM="truecolor", HERDR_CONFIG_PATH=str(config), XDG_CONFIG_HOME=str(case_dir / "config"), XDG_STATE_HOME=str(case_dir / "state"), HERDR_LOG="herdr::client::shell::reconnect_draft=debug")
    client = AgentClient([str(Path(args.binary).resolve()), "--remote", args.target, "--session", session], env, case_dir, agent)
    pane, workdir = None, None
    offline_suffix = WIDE_SUFFIX if scenario == Scenario.WIDE_UNICODE else BURST_SUFFIX
    case = {"session": session, "agent": agent, "buffer_reconnect_input": enabled, "scenario": scenario, "binary_sha256": binary_digest(args.binary), "submitted_prompt": False, "passed": False}
    try:
        deadline = time.monotonic() + 60
        while True:
            client.pump(.1)
            try:
                remote.json("--session", session, "workspace", "list")
                break
            except RuntimeError:
                if client.process.poll() is not None or time.monotonic() >= deadline:
                    raise
        workdir = ssh_text(args.target, ["mktemp", "-d", "/tmp/herdr-reconnect-draft.XXXXXXXX"]).strip()
        if not re.fullmatch(r"/tmp/herdr-reconnect-draft\.[A-Za-z0-9]+", workdir):
            raise RuntimeError("Unexpected generated diagnostic directory")
        pane = remote.json("--session", session, "workspace", "create", "--cwd", workdir, "--label", "reconnect-draft-smoke", "--focus")["result"]["root_pane"]["pane_id"]
        command = [str(Path(args.agent_bin_dir) / agent)]
        case["agent_version"] = ssh_text(args.target, [*command, "--version"]).strip()
        if agent == "claude":
            command += ["--safe-mode", "--permission-mode", "plan"]
        else:
            command += ["--sandbox", "read-only", "--ask-for-approval", "on-request", "--cd", workdir]
        remote.cli("--session", session, "pane", "run", pane, shlex.join(command))
        case.update(wait_for_agent(client, remote, session, pane, agent, workdir))
        case["detected_agent"] = own_pane_agent_metadata(remote.json("--session", session, "agent", "list")["result"]["agents"], pane, agent)
        expected = ""
        for char in BASE_DRAFT:
            expected, _ = sample_key(client, expected, char)
            settle(client, .3)
        case["before_drop"] = require_authoritative(remote, session, pane, expected, agent)
        settle(client)
        if scenario == Scenario.CLIENT_RESET:
            os.write(client.master, PREFIX_KEY)
            client.wait_for(lambda screen: " PREFIX " in screen.splitlines()[-1], 8, "client prefix mode")
            os.write(client.master, ESCAPE_KEY)
            client.wait_for(lambda screen: " PREFIX " not in screen.splitlines()[-1], 8, "client-only Escape cancellation")
            settle(client)
            case["client_reset"] = {"prefix_observed": True, "escape_cancelled": True, "remote_unchanged": require_authoritative(remote, session, pane, expected, agent)}
        case["first_drop"] = gate.interrupt(client, session)
        case["first_drop"]["offline_edit"] = offline_burst(client, enabled, scenario)
        case["first_drop"]["remote_unchanged_during_gate"] = require_authoritative(remote, session, pane, expected, agent)
        if scenario == Scenario.CHANGED_CONTEXT:
            remote.cli("--session", session, "pane", "send-keys", pane, "ctrl+u")
            remote.cli("--session", session, "pane", "send-text", pane, CHANGED_DRAFT)
            expected = CHANGED_DRAFT
            case["context_change"] = require_authoritative(remote, session, pane, expected, agent)
        elif enabled:
            expected += offline_suffix
        restoration_started = time.monotonic()
        gate.release()
        if enabled and scenario != Scenario.CHANGED_CONTEXT:
            case["handoff_panel"] = wait_panel_retired(client, restoration_started)
        case["restored"] = wait_restored(client, remote, session, pane, expected, agent, cursor_at_end=scenario != Scenario.CHANGED_CONTEXT, deadline=restoration_started + 45, recovery_suffix=offline_suffix)
        settle(client, 1)
        case["settled_once"] = require_authoritative(remote, session, pane, expected, agent)
        if scenario == Scenario.CHANGED_CONTEXT:
            case["retained_panel"] = panel_state(client, BURST_SUFFIX)
            if enabled:
                if not exact_held_suffix(client, offline_suffix):
                    raise RuntimeError("Changed editor did not preserve the exact generated recovery suffix")
                if not all(case["retained_panel"][field] for field in ("present", "generated_suffix_visible")):
                    raise RuntimeError("Changed editor did not retain the local reconnect draft")
                if not all(case["retained_panel"][field] for field in ("copy_recovery_label_present", "copy_button_visible", "discard_button_visible")):
                    raise RuntimeError("Changed editor did not advertise copy-recovery controls")
            os.write(client.master, b"K")
            expected += "K"
            case["held_online_edit"] = wait_restored(client, remote, session, pane, expected, agent)
            if enabled:
                case["held_suffix_unchanged"] = exact_held_suffix(client, offline_suffix)
                if not case["held_suffix_unchanged"]:
                    raise RuntimeError("Online keyboard input altered the held recovery suffix")
        else:
            if panel_state(client, offline_suffix)["present"]:
                raise RuntimeError("Successfully restored draft panel remained pending")
            if scenario == Scenario.SECOND_DROP:
                case["second_drop"] = gate.interrupt(client, session)
                os.write(client.master, SECOND_SUFFIX.encode())
                if enabled:
                    client.wait_for(lambda screen: PANEL_TITLE in screen and SECOND_SUFFIX in screen, 8, "second local reconnect suffix")
                else:
                    settle(client)
                case["second_drop"]["remote_unchanged_during_gate"] = require_authoritative(remote, session, pane, expected, agent)
                if enabled:
                    expected += SECOND_SUFFIX
                restoration_started = time.monotonic()
                gate.release()
                if enabled:
                    case["second_handoff_panel"] = wait_panel_retired(client, restoration_started)
                case["second_restored"] = wait_restored(client, remote, session, pane, expected, agent, deadline=restoration_started + 45, recovery_suffix=SECOND_SUFFIX)
                settle(client, 1)
                case["second_settled_once"] = require_authoritative(remote, session, pane, expected, agent)
            os.write(client.master, b"K")
            expected += "K"
            case["healthy_edit"] = wait_restored(client, remote, session, pane, expected, agent)
        case["final_authoritative"] = require_authoritative(remote, session, pane, expected, agent)
        case["passed"] = True
    except EXPECTED_FAILURES as error:
        case["error"] = str(error)
        if isinstance(error, RestoreFailure):
            case["restore_failure"] = error.diagnosis
    finally:
        case["final_panel"] = panel_state(client, offline_suffix)
        case["final_generated_evidence"] = generated_evidence(client, BASE_DRAFT)
        owned_events = [event for event in gate.read_events() if event.session == session and event.parent_pid == client.process.pid]
        case["bridge_events"] = [asdict(event) for event in owned_events]
        deliberately_gated = {event.pid for event in owned_events if event.phase == BridgePhase.GATED}
        ungated_releases = [event.pid for event in owned_events if event.phase == BridgePhase.RELEASED and event.pid not in deliberately_gated]
        case["uncontrolled_replacement_pids"] = ungated_releases[1:]
        gate.release()
        cleanup = []
        for label, action in (
            ("local native client", client.close),
            ("own named session stop", lambda: remote.cli("session", "stop", session, "--json")),
            ("own named session delete", lambda: remote.cli("session", "delete", session, "--json")),
        ):
            try:
                action()
            except EXPECTED_FAILURES as error:
                cleanup.append(f"{label}: {error}")
        if workdir and re.fullmatch(r"/tmp/herdr-reconnect-draft\.[A-Za-z0-9]+", workdir):
            try:
                ssh_text(args.target, ["rm", "-rf", "--", workdir])
            except EXPECTED_FAILURES as error:
                cleanup.append(f"own generated directory: {error}")
        case["cleanup_errors"] = cleanup
        try:
            case["reconnect_diagnostics"] = collect_reconnect_diagnostics(case_dir)
        except (OSError, RuntimeError, UnicodeError) as error:
            cleanup.append(f"own structural diagnostic log: {type(error).__name__}")
        case["raw_terminal_bytes"] = (case_dir / "terminal.ansi").stat().st_size
        if cleanup:
            case["passed"] = False
        (case_dir / "case.json").write_text(json.dumps(case, indent=2) + "\n", encoding="utf-8")
    return case


def self_test():
    assert display_width(WIDE_SUFFIX) == 5 and len(WIDE_SUFFIX) == 3
    screen = CursorScreen("codex")
    expected = BASE_DRAFT + WIDE_SUFFIX
    prefix = " " * 27 + "› "
    editor = prefix + expected
    editor += " " * (WIDTH - display_width(editor) - 1) + "▐"
    end = display_width(prefix + expected)
    screen.feed((f"\x1b[6;1H{editor}\x1b[6;{end + 1}H").encode())
    assert len(draft_rows(screen.text(), expected, agent="codex", composed=True)) == 1
    assert exact_draft_cursor(screen, expected)
    assert not draft_rows(f"› {expected}X", expected, agent="codex")
    assert not draft_rows(screen.text(), expected + "X", agent="codex", composed=True)
    screen.feed((f"\x1b[40;28H{PANEL_TITLE} · copy to recover\x1b[40;131H[Discard]\x1b[41;28H{BURST_SUFFIX}").encode())

    class ScreenProbe:
        def __init__(self, screen):
            self.screen = screen

    assert exact_held_suffix(ScreenProbe(screen), BURST_SUFFIX)
    screen.feed(b"K")
    assert not exact_held_suffix(ScreenProbe(screen), BURST_SUFFIX)
    read_ready = Event()

    class PumpProbe:
        def __init__(self):
            self.calls = 0

        def pump(self, timeout):
            self.calls += 1
            if self.calls >= 2:
                read_ready.set()

    class ReadProbe:
        def cli(self, *args, timeout):
            if not read_ready.wait(5):
                raise RuntimeError("PTY drain probe did not release the blocked read")
            return BASE_DRAFT

    client = PumpProbe()
    with ThreadPoolExecutor(max_workers=1) as worker:
        source, _ = read_authoritative_while_pumping(worker, client, ReadProbe(), "generated", "own-pane", time.monotonic() + 5)
    assert source == BASE_DRAFT and client.calls >= 2
    mismatch = 'event="reconnect.context_mismatch" phase="observe" ' + " ".join(f"{name}=false" for name in MISMATCH_BOOL_FIELDS) + " " + " ".join(f"{name}=0" for name in MISMATCH_COUNT_FIELDS)
    parsed = parse_reconnect_diagnostics((
        'unrelated secret="never-retain"',
        mismatch + ' target="never-retain" pane_id="never-retain" draft="never-retain"',
        'event="reconnect.echo_timeout" draft="never-retain"',
        'event="reconnect.context_mismatch" phase="unexpected"',
    ))
    assert len(parsed["context_mismatches"]) == 1
    assert set(parsed["context_mismatches"][0]) == {"phase", *MISMATCH_BOOL_FIELDS, *MISMATCH_COUNT_FIELDS}
    assert parsed["echo_timeouts"] == 1
    assert parsed["malformed_records"] == 1 and "never-retain" not in json.dumps(parsed)
    bounded = parse_reconnect_diagnostics((mismatch.replace("symbol_changes=0", f"symbol_changes={index}") for index in range(MAX_DIAGNOSTIC_RECORDS + 1)))
    assert len(bounded["context_mismatches"]) == MAX_DIAGNOSTIC_RECORDS and bounded["omitted_records"] == 1
    assert bounded["context_mismatches"][0]["symbol_changes"] == 1
    assert bounded["context_mismatches"][-1]["symbol_changes"] == MAX_DIAGNOSTIC_RECORDS
    with tempfile.TemporaryDirectory(prefix="herdr-rdraft-selftest-", dir="/tmp") as tmp:
        root = Path(tmp)
        log = root / "config/herdr-proto/sessions/generated/herdr-client.log"
        log.parent.mkdir(parents=True)
        log.write_text(mismatch + '\nignored secret="never-retain"\n')
        collected = collect_reconnect_diagnostics(root)
        assert len(collected["context_mismatches"]) == 1 and not log.exists()
        assert "never-retain" not in json.dumps(collected)
        fake = root / "ssh-stub"
        fake.write_text("#!/bin/sh\nexit 0\n")
        fake.chmod(0o700)
        gate = BridgeGate(root, root, "test-host", fake)
        gate.select("owned-session")
        gate.gate.touch()
        process = subprocess.Popen([str(gate.wrapper), "test-host", "herdr --session owned-session remote-client-bridge"])
        try:
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline and not gate.read_events():
                time.sleep(.02)
            records = gate.read_events()
            assert len(records) == 1 and records[0].phase == BridgePhase.GATED
            assert records[0].pid == process.pid and process.poll() is None
            control = subprocess.run([str(gate.wrapper), "test-host", "herdr --version"], capture_output=True, timeout=5)
            assert control.returncode == 0 and len(gate.read_events()) == 1
            other = subprocess.run([str(gate.wrapper), "test-host", "herdr --session another-session remote-client-bridge"], capture_output=True, timeout=5)
            assert other.returncode != 0 and len(gate.read_events()) == 1
            gate.release()
            assert process.wait(timeout=5) == 0
            assert gate.read_events()[-1].phase == BridgePhase.RELEASED
        finally:
            gate.release()
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=5)
    print("owned bridge gate, continuous PTY draining, exact wide-Unicode cursor, held suffix, and bounded structural-only diagnostics assertions passed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target")
    parser.add_argument("--binary", default="target/debug/herdr")
    parser.add_argument("--remote-binary", default="herdr")
    parser.add_argument("--agent-bin-dir")
    parser.add_argument("--ssh-binary", default=shutil.which("ssh", path=os.defpath), help="native SSH executable; excludes prior task wrappers by default")
    parser.add_argument("--agents", nargs="+", choices=("claude", "codex"), default=("claude", "codex"))
    parser.add_argument("--mode", choices=("both", "on", "off"), default="both")
    parser.add_argument("--cases", nargs="+", choices=tuple(Scenario), default=tuple(Scenario))
    parser.add_argument("--artifacts")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return 0
    if not args.target or not args.agent_bin_dir:
        parser.error("--target and --agent-bin-dir are required")
    if not Path(args.agent_bin_dir).is_absolute():
        parser.error("--agent-bin-dir must be an absolute remote path")
    if args.ssh_binary is None:
        parser.error("SSH is unavailable")
    ssh = Path(args.ssh_binary).resolve()
    if not ssh.is_file() or not os.access(ssh, os.X_OK):
        parser.error("--ssh-binary must name an executable")
    root = Path(args.artifacts).resolve() if args.artifacts else Path(tempfile.mkdtemp(prefix="herdr-reconnect-draft-")).resolve()
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    candidate = root / "candidate-herdr"
    if candidate.exists():
        parser.error("artifact directory already contains a candidate binary")
    shutil.copy2(Path(args.binary).resolve(), candidate)
    candidate.chmod(0o700)
    args.binary = str(candidate)
    modes = (False, True) if args.mode == "both" else (args.mode == "on",)
    report = {"target": args.target, "artifacts": str(root), "binary_sha256": binary_digest(candidate), "cases": [], "passed": False, "submitted_model_prompts": False}
    old_path = os.environ["PATH"]
    with tempfile.TemporaryDirectory(prefix="herdr-rdraft-cm-", dir="/tmp") as mux:
        gate = BridgeGate(root, Path(mux), args.target, ssh)
        os.environ["PATH"] = str(gate.root) + os.pathsep + old_path
        remote = Remote(args.target, args.remote_binary)
        try:
            print(f"Reconnect draft artifacts: {root}", flush=True)
            for agent in args.agents:
                for enabled in modes:
                    for scenario in args.cases:
                        print(f"Running {agent}, buffering={enabled}, case={scenario}", flush=True)
                        case = run_case(args, gate, remote, agent, enabled, Scenario(scenario), root)
                        report["cases"].append(case)
                        print(json.dumps({"agent": agent, "buffering": enabled, "scenario": scenario, "passed": case["passed"], **({"error": case["error"]} if "error" in case else {})}), flush=True)
            report["passed"] = all(case["passed"] for case in report["cases"])
        finally:
            try:
                gate.close_mux()
                report["owned_mux_closed"] = True
            except EXPECTED_FAILURES as error:
                report.update(owned_mux_closed=False, mux_cleanup_error=str(error), passed=False)
            os.environ["PATH"] = old_path
            (root / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
