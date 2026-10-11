import io
import json
import os
import time

import pytest

from prhub import cli, store


@pytest.fixture
def home(tmp_path, monkeypatch):
    monkeypatch.setenv("PRHUB_HOME", str(tmp_path))
    monkeypatch.setenv("PRHUB_CONFIG", str(tmp_path / "missing.toml"))
    return tmp_path


def healthy_daemon():
    store.write_json(store.home() / "daemon.json",
                     {"pid": os.getpid(), "last_poll": int(time.time()), "last_poll_ok": int(time.time())})


# ---- caller id ----

def test_caller_prefers_prhub_caller_then_panes(monkeypatch):
    for var in ("PRHUB_CALLER", "HERDR_PANE_ID", "TMUX_PANE"):
        monkeypatch.delenv(var, raising=False)
    assert cli.caller() == "cli"
    monkeypatch.setenv("TMUX_PANE", "%3")
    assert cli.caller() == "%3"
    monkeypatch.setenv("PRHUB_CALLER", "reviewer-agent")
    assert cli.caller() == "reviewer-agent"


# ---- parser ----

def test_parser_accepts_commit_options():
    a = cli.build_parser().parse_args(["wait", "acme/api@abcdef1", "--until", "deployed", "--env", "staging",
                                       "--workflow", "deploy", "--timeout", "60"])
    assert (a.cmd, a.env, a.workflow, a.timeout) == ("wait", "staging", "deploy", 60)


def test_every_command_has_a_handler():
    sub = next(x for x in cli.build_parser()._actions if x.dest == "cmd")
    assert set(sub.choices) == set(cli.HANDLERS)


def test_version_flag(capsys):
    with pytest.raises(SystemExit):
        cli.main(["--version"])
    assert capsys.readouterr().out.startswith("prhub ")


# ---- services ----

def test_systemd_unit_runs_the_daemon():
    unit = cli.UNIT.format(exe="/usr/local/bin/prhub", path="/usr/bin:/bin")
    assert "ExecStart=/usr/local/bin/prhub daemon" in unit and "Environment=PATH=/usr/bin:/bin" in unit
    assert "WantedBy=default.target" in unit


def test_launchd_plist_runs_the_daemon():
    plist = cli.PLIST.format(exe="/opt/bin/prhub", path="/usr/bin", log="/tmp/d.log")
    assert "<string>/opt/bin/prhub</string><string>daemon</string>" in plist and "KeepAlive" in plist


# ---- state directory ----

def test_empty_prhub_home_means_the_default(monkeypatch):
    monkeypatch.setenv("PRHUB_HOME", "")
    assert str(store.home()).endswith(os.path.join(".local", "state", "prhub"))


# ---- status and the hook ----

def test_status_without_a_daemon_exits_4(home, capsys):
    assert cli.main(["status"]) == cli.EXIT_NO_DAEMON
    assert "never ran" in capsys.readouterr().out


def _hook(monkeypatch, command):
    monkeypatch.setattr("sys.stdin", io.StringIO(json.dumps({"tool_name": "Bash", "tool_input": {"command": command}})))


def test_hook_denies_a_loop_when_the_daemon_is_healthy(home, monkeypatch, capsys):
    healthy_daemon()
    _hook(monkeypatch, "while true; do gh pr view 1; sleep 30; done")
    assert cli.main(["hook", "claude"]) == 0
    out = json.loads(capsys.readouterr().out)
    assert out["hookSpecificOutput"]["permissionDecision"] == "deny"
    assert "prhub wait" in out["hookSpecificOutput"]["permissionDecisionReason"]


def test_hook_stands_aside_when_the_daemon_is_down(home, monkeypatch, capsys):
    _hook(monkeypatch, "while true; do gh pr view 1; sleep 30; done")
    assert cli.main(["hook", "claude"]) == 0
    assert capsys.readouterr().out == ""


def test_hook_ignores_bad_input(home, monkeypatch, capsys):
    monkeypatch.setattr("sys.stdin", io.StringIO("not json"))
    assert cli.main(["hook", "claude"]) == 0
    assert capsys.readouterr().out == ""
