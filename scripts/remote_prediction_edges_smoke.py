#!/usr/bin/env python3
"""Check real SSH input integrity for editing, paste, Unicode, resize and no echo."""
import argparse
import json
import os
from pathlib import Path
import shlex
import struct
import termios
import fcntl
import time
import uuid

from remote_latency_smoke import Client, Remote, cleanup_fixture

PROMPT = "EDGE> "


def fixture_command(no_echo):
    source = f'''import codecs,os,sys,termios,time
fd=sys.stdin.fileno()
old=termios.tcgetattr(fd)
raw=termios.tcgetattr(fd)
raw[3] &= ~(termios.ECHO | termios.ICANON)
raw[6][termios.VMIN]=1
raw[6][termios.VTIME]=0
termios.tcsetattr(fd,termios.TCSANOW,raw)
text=""
count=0
decoder=codecs.getincrementaldecoder("utf-8")()
escape=""
def paint():
    visible="" if {no_echo!r} else text
    os.write(1, ("\\x1b[1;1HEDGE_ACK:"+str(count).zfill(6)+"\\x1b[K\\x1b[3;1H{PROMPT}"+visible+"\\x1b[K").encode())
try:
    os.write(1,b"\\x1b[?1049l\\x1b[?25h\\x1b[?2004h\\x1b[2J")
    paint()
    while True:
        data=os.read(fd,1)
        if not data or data == b"\\x04": break
        for char in decoder.decode(data):
            if escape:
                escape+=char
                if len(escape)>2 and (char.isalpha() or char=="~"):
                    escape=""
                continue
            if char=="\\x1b":
                escape=char
                continue
            if char in ("\\x7f","\\x08"):
                text=text[:-1]
            elif char=="\\x15":
                text=""
            elif ord(char)>=32:
                text+=char
            else:
                continue
            count+=1
            time.sleep(0.18)
            paint()
finally:
    termios.tcsetattr(fd,termios.TCSANOW,old)
'''
    return shlex.join(["python3", "-u", "-c", source])


def settle(client, seconds=0.6):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        client.pump(0.02)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--remote-binary", required=True)
    parser.add_argument("--artifacts", type=Path, required=True)
    args = parser.parse_args()
    root = args.artifacts.resolve()
    root.mkdir(parents=True, exist_ok=False, mode=0o700)
    remote = Remote(args.target, args.remote_binary)
    report = {"cases": []}
    for no_echo in (False, True):
        case_dir = root / ("no-echo" if no_echo else "editable")
        case_dir.mkdir()
        config = case_dir / "config.toml"
        config.write_text("onboarding=false\n[remote]\npredict_input=true\nmanage_ssh_config=false\n")
        env = {key: value for key, value in os.environ.items() if not key.startswith("HERDR_")}
        env.update(TERM="xterm-256color", COLORTERM="truecolor", HERDR_CONFIG_PATH=str(config), XDG_CONFIG_HOME=str(case_dir / "config"), XDG_STATE_HOME=str(case_dir / "state"))
        session = "edge-smoke-" + uuid.uuid4().hex[:10]
        client = Client([str(Path(args.binary).resolve()), "--remote", args.target, "--session", session], env, case_dir)
        pane = None
        created = False
        case = {"no_echo": no_echo, "session": session, "checks": []}
        try:
            deadline = time.monotonic() + 45
            while True:
                client.pump(0)
                try:
                    remote.json("--session", session, "workspace", "list")
                    created = True
                    break
                except RuntimeError:
                    if client.process.poll() is not None or time.monotonic() >= deadline:
                        raise
                    settle(client, 0.2)
            result = remote.json("--session", session, "workspace", "create", "--cwd", "/tmp", "--label", "edge-smoke", "--focus")
            pane = result["result"]["root_pane"]["pane_id"]
            remote.cli("--session", session, "pane", "run", pane, fixture_command(no_echo))
            settle(client, 1)
            if PROMPT not in client.screen.text():
                os.write(client.master, b"\r")
            client.wait_for(lambda screen: PROMPT in screen, 15, "edge fixture")
            if no_echo:
                for index, char in enumerate("generatednoechoprobe", 1):
                    os.write(client.master, char.encode())
                    deadline = time.monotonic() + 4
                    while f"EDGE_ACK:{index:06d}" not in client.screen.text():
                        client.pump(0.02)
                        row = next(line for line in client.screen.text().splitlines() if PROMPT in line)
                        if row.split(PROMPT, 1)[1].rstrip(" ▐│▕"):
                            raise RuntimeError("Non-echoing input was displayed")
                        if time.monotonic() >= deadline:
                            raise RuntimeError("Non-echoing fixture ACK timed out")
                case["checks"].append("non-echoing prompt stayed blank for every observed frame")
            else:
                expected = ""
                for char in "abc":
                    os.write(client.master, char.encode())
                    expected += char
                    client.wait_for(lambda screen: PROMPT + expected in screen, 5, "ASCII draft")
                    settle(client)
                os.write(client.master, b"\x7f")
                expected = "ab"
                settle(client)
                os.write(client.master, b"\x1b[200~paste\x1b[201~")
                expected += "paste"
                settle(client, 1.5)
                os.write(client.master, "界é".encode())
                expected += "界é"
                settle(client, 1)
                fcntl.ioctl(client.master, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 130, 0, 0))
                client.screen.width, client.screen.height = 130, 40
                client.screen.clear()
                settle(client, 1)
                authoritative = remote.cli("--session", session, "pane", "read", pane, "--source", "visible", "--format", "text")
                rows = [line.rstrip() for line in authoritative.splitlines() if line.startswith(PROMPT.rstrip())]
                if rows != [PROMPT + expected]:
                    raise RuntimeError("Editing/paste/Unicode input differed from exact authoritative text")
                case["checks"] += ["backspace", "bracketed paste", "Unicode fallback", "resize", "exact authoritative text: " + expected]
                os.write(client.master, b"\x15")
                settle(client, 0.7)
                authoritative = remote.cli("--session", session, "pane", "read", pane, "--source", "visible", "--format", "text")
                if next(line for line in authoritative.splitlines() if line.startswith(PROMPT.rstrip())).rstrip() != PROMPT.rstrip():
                    raise RuntimeError("Ctrl-U did not clear authoritative input")
                case["checks"].append("Ctrl-U")
            case["passed"] = True
        except Exception as error:
            case.update(passed=False, error=str(error))
        finally:
            case["cleanup_errors"] = cleanup_fixture(client, remote, session, case_dir, pane, created)
            report["cases"].append(case)
            (root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
            print(json.dumps(case), flush=True)
    report["passed"] = all(case["passed"] and not case["cleanup_errors"] for case in report["cases"])
    (root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
