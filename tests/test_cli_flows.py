import json
import runpy
import subprocess
import sys
import time

import pytest
from fakes import FakeClock, commit_node, healthy_daemon, pr_node

from prhub import cli, commits, daemon, store
from prhub.commits import CommitRef
from prhub.conditions import summary
from prhub.refs import PRRef

API = PRRef("acme", "api", 1)
SHA = CommitRef("acme", "api", "abcdef1")


@pytest.fixture
def clock(monkeypatch):
    c = FakeClock()
    monkeypatch.setattr(cli, "time", c)
    return c


def put_pr(ref=API, at=None, failing=(), **kw):
    store.write_pr(ref, {"ref": str(ref), "fetched_at": int(at if at is not None else time.time()),
                         "summary": summary(pr_node(number=ref.number, **kw)), "failing": list(failing)})


def put_commit(ref=SHA, at=None, node=None, missing=False):
    rec = {"ref": str(ref), "fetched_at": int(at if at is not None else time.time())}
    if missing:
        rec["missing"] = True
    else:
        rec["summary"] = commits.summarize(node or commit_node())
    store.write_commit(ref, rec)


# ---- state ----

def test_state_of_a_pr(capsys):
    put_pr(failing=[{"name": "test"}], checks="FAILURE")
    assert cli.main(["state", "acme/api#1"]) == 0
    out = capsys.readouterr().out
    assert out.startswith("acme/api#1 OPEN checks=FAILURE") and "failing: test" in out
    assert "acme/api#1" in store.watches()


def test_state_json(capsys):
    put_pr()
    assert cli.main(["state", "acme/api#1", "--json"]) == 0
    d = json.loads(capsys.readouterr().out)
    assert d["ref"] == "acme/api#1" and d["approved_head"] is True and d["failing"] == []


def test_state_without_data_waits_for_a_poll_then_gives_up(clock, capsys):
    assert cli.main(["state", "acme/api#1"]) == cli.EXIT_NO_DAEMON
    assert "no data yet" in capsys.readouterr().err and (store.home() / "nudge").exists()


def test_state_fresh_returns_once_a_new_poll_lands(clock, monkeypatch, capsys):
    put_pr(at=clock.now - 600)
    real_sleep = clock.sleep

    def sleep_then_poll(s):
        real_sleep(s)
        put_pr(at=clock.now)
    monkeypatch.setattr(clock, "sleep", sleep_then_poll)
    assert cli.main(["state", "acme/api#1", "--fresh"]) == 0
    assert "(0s old)" in capsys.readouterr().out


def test_state_of_a_commit(capsys):
    put_commit()
    assert cli.main(["state", "acme/api@abcdef1"]) == 0
    assert "checks=SUCCESS" in capsys.readouterr().out
    assert cli.main(["state", "acme/api@abcdef1", "--json", "--workflow", "CI"]) == 0
    d = json.loads(capsys.readouterr().out)
    assert d["checks"] == "SUCCESS" and d["failing"] == []


def test_state_of_a_missing_or_unknown_commit(clock, capsys):
    put_commit(missing=True)
    assert cli.main(["state", "acme/api@abcdef1"]) == 2
    assert "no such commit" in capsys.readouterr().err
    assert cli.main(["state", "acme/api@1234567"]) == cli.EXIT_NO_DAEMON


def test_bad_names_are_reported(capsys):
    assert cli.main(["state", "not a pr"]) == 2
    assert "cannot read" in capsys.readouterr().err


# ---- wait: PRs ----

def test_wait_returns_when_done(clock, capsys):
    healthy_daemon()
    put_pr(at=clock.now)
    assert cli.main(["wait", "acme/api#1", "--until", "done"]) == cli.EXIT_MET
    assert capsys.readouterr().out.startswith("done: acme/api#1")


def test_wait_ignores_data_from_before_it_started(clock, capsys):
    healthy_daemon()
    put_pr(at=clock.now - 600)  # ready, but old: must not end the wait
    assert cli.main(["wait", "acme/api#1", "--until", "done", "--timeout", "30"]) == cli.EXIT_TIMEOUT
    assert "no fresh data" in capsys.readouterr().out


def test_wait_times_out_with_the_last_state(clock, capsys):
    healthy_daemon()
    put_pr(at=clock.now, checks="PENDING")
    assert cli.main(["wait", "acme/api#1", "--until", "green", "--timeout", "30"]) == cli.EXIT_TIMEOUT
    assert "checks=PENDING" in capsys.readouterr().out


def test_wait_stops_when_the_pr_closes(clock, capsys):
    healthy_daemon()
    put_pr(at=clock.now, state="CLOSED")
    assert cli.main(["wait", "acme/api#1", "--until", "merged"]) == cli.EXIT_IMPOSSIBLE
    assert "cannot reach merged" in capsys.readouterr().out


def test_wait_refuses_unknown_conditions_and_needs_a_daemon(capsys):
    assert cli.main(["wait", "acme/api#1", "--until", "shipped"]) == 2
    assert cli.main(["wait", "acme/api#1", "--until", "done"]) == cli.EXIT_NO_DAEMON
    err = capsys.readouterr().err
    assert "unknown condition" in err and "install-systemd" in err


def test_wait_notices_the_daemon_dying(clock, capsys):
    healthy_daemon()
    real_sleep = clock.sleep

    def sleep_and_kill(s):
        real_sleep(s)
        store.write_json(store.home() / "daemon.json", {"pid": 999999, "last_poll_ok": 1, "interval": 60})
    clock.sleep = sleep_and_kill
    assert cli.main(["wait", "acme/api#1", "--until", "done"]) == cli.EXIT_NO_DAEMON
    assert "stopped polling" in capsys.readouterr().err


def test_wait_records_who_is_waiting(clock, monkeypatch):
    monkeypatch.setenv("PRHUB_CALLER", "agent-7")
    healthy_daemon()
    put_pr(at=clock.now)
    cli.main(["wait", "acme/api#1", "--until", "done"])
    assert store.watches()["acme/api#1"]["by"] == "agent-7"


# ---- wait: commits ----

def test_wait_commit_green_and_deployed(clock, capsys):
    healthy_daemon()
    put_commit(at=clock.now, node=commit_node(deployments=[("a" * 40, "production", "2026-10-11T09:00:00Z",
                                                            "SUCCESS")]))
    assert cli.main(["wait", "acme/api@abcdef1", "--until", "green"]) == cli.EXIT_MET
    assert cli.main(["wait", "acme/api@abcdef1", "--until", "deployed", "--env", "production"]) == cli.EXIT_MET
    assert "production=live" in capsys.readouterr().out



def test_wait_deployed_matches_a_short_environment_name_and_names_a_wrong_one(clock, capsys):
    healthy_daemon()
    put_commit(at=clock.now, node=commit_node(deployments=[("a" * 40, "prod", "2026-10-11T09:00:00Z", "SUCCESS")]))
    assert cli.main(["wait", "acme/api@abcdef1", "--until", "deployed"]) == cli.EXIT_MET
    capsys.readouterr()
    assert cli.main(["wait", "acme/api@abcdef1", "--until", "deployed", "--env", "qa",
                     "--timeout", "4000"]) == cli.EXIT_TIMEOUT
    out, err = capsys.readouterr()
    assert "environments seen: prod" in err and "environments seen: prod" in out


def test_wait_commit_fails_fast_on_a_red_run(clock, capsys):
    healthy_daemon()
    put_commit(at=clock.now, node=commit_node(runs=[("CI", "COMPLETED", "FAILURE")]))
    assert cli.main(["wait", "acme/api@abcdef1", "--until", "green"]) == cli.EXIT_IMPOSSIBLE
    assert "failed: CI" in capsys.readouterr().out


def test_wait_commit_timeout_missing_and_errors(clock, capsys):
    healthy_daemon()
    put_commit(at=clock.now, node=commit_node(runs=[("CI", "IN_PROGRESS", None)]))
    assert cli.main(["wait", "acme/api@abcdef1", "--until", "checks", "--timeout", "4000"]) == cli.EXIT_TIMEOUT
    assert "checks=PENDING" in capsys.readouterr().out
    assert cli.main(["wait", "acme/api@abcdef1", "--until", "merged"]) == 2
    put_commit(at=clock.now, missing=True)
    assert cli.main(["wait", "acme/api@abcdef1", "--until", "green"]) == 2
    store.write_json(store.home() / "daemon.json", {})
    assert cli.main(["wait", "acme/api@abcdef1", "--until", "green"]) == cli.EXIT_NO_DAEMON


def test_wait_commit_notices_the_daemon_dying(clock, capsys):
    healthy_daemon()
    real_sleep = clock.sleep

    def sleep_and_kill(s):
        real_sleep(s)
        store.write_json(store.home() / "daemon.json", {"pid": 999999, "last_poll_ok": 1, "interval": 60})
    clock.sleep = sleep_and_kill
    assert cli.main(["wait", "acme/api@abcdef1", "--until", "green"]) == cli.EXIT_NO_DAEMON


# ---- watch, list, status ----

def test_watch_unwatch(capsys):
    assert cli.main(["watch", "acme/api#1", "acme/api#2"]) == 0
    assert set(store.watches()) == {"acme/api#1", "acme/api#2"}
    assert cli.main(["unwatch", "acme/api#2"]) == 0
    assert set(store.watches()) == {"acme/api#1"}


def test_list_filters(capsys):
    put_pr(API)
    put_pr(PRRef("acme", "api", 2), checks="PENDING")
    put_pr(PRRef("acme", "api", 3), state="MERGED")
    assert cli.main(["list"]) == 0
    assert capsys.readouterr().out.count("\n") == 2
    assert cli.main(["list", "--ready", "--json"]) == 0
    assert [r["ref"] for r in json.loads(capsys.readouterr().out)] == ["acme/api#1"]
    assert cli.main(["list", "--all", "--json"]) == 0
    assert len(json.loads(capsys.readouterr().out)) == 3


def test_status_healthy_paused_and_down(capsys):
    healthy_daemon(rate={"remaining": 4000, "resetAt": "x"}, tracked=3)
    assert cli.main(["status"]) == 0
    assert "daemon: healthy" in capsys.readouterr().out
    store.write_json(store.home() / "daemon.json", {"pid": __import__("os").getpid(), "last_poll": int(time.time()),
                                                    "rate_limited_until": int(time.time()) + 600,
                                                    "last_error": "GraphQL: API rate limit already exceeded"})
    assert cli.main(["status"]) == 0
    out = capsys.readouterr().out
    assert "PAUSED by the GitHub rate limit" in out and "last error" in out
    store.write_json(store.home() / "daemon.json", {"pid": 999999, "last_poll_ok": 1})
    assert cli.main(["status"]) == cli.EXIT_NO_DAEMON
    assert "NOT polling" in capsys.readouterr().out


# ---- services, daemon command, module entry ----

def _fake_run(calls, code=0):
    def run(cmd, **kw):
        calls.append(cmd)
        return subprocess.CompletedProcess(cmd, code, stdout="", stderr="boom" if code else "")
    return run


def test_install_launchd_writes_the_plist(isolated, monkeypatch, capsys):
    monkeypatch.setenv("HOME", str(isolated))
    calls = []
    monkeypatch.setattr(cli.subprocess, "run", _fake_run(calls))
    assert cli.main(["install-launchd"]) == 0
    plist = isolated / "Library" / "LaunchAgents" / "dev.prhub.daemon.plist"
    assert "<string>daemon</string>" in plist.read_text()
    assert calls[0][:2] == ["launchctl", "bootout"] and calls[1][:2] == ["launchctl", "bootstrap"]


def test_install_systemd_writes_and_enables_the_unit(isolated, monkeypatch, capsys):
    monkeypatch.setenv("HOME", str(isolated))
    calls = []
    monkeypatch.setattr(cli.subprocess, "run", _fake_run(calls))
    assert cli.main(["install-systemd"]) == 0
    unit = isolated / ".config" / "systemd" / "user" / "prhub.service"
    assert " daemon" in unit.read_text() and calls[-1][-2:] == ["--now", "prhub.service"]
    assert "enable-linger" in capsys.readouterr().out


def test_install_systemd_reports_a_failure(isolated, monkeypatch, capsys):
    monkeypatch.setenv("HOME", str(isolated))
    monkeypatch.setattr(cli.subprocess, "run", _fake_run([], code=1))
    assert cli.main(["install-systemd"]) == 1
    assert "boom" in capsys.readouterr().err


def test_daemon_command_runs_one_poll(monkeypatch):
    monkeypatch.setattr(daemon, "gh_token", lambda: "tok")
    monkeypatch.setattr(daemon, "discover_open", lambda q, t: ([], {"remaining": 5000}))
    assert cli.main(["daemon", "--once"]) == 0
    assert store.daemon_state()["last_poll_ok"]


def test_python_dash_m(monkeypatch, capsys):
    monkeypatch.setattr(sys, "argv", ["prhub", "--version"])
    with pytest.raises(SystemExit) as e:
        runpy.run_module("prhub", run_name="__main__")
    assert e.value.code == 0 and capsys.readouterr().out.startswith("prhub ")


def test_status_degraded_and_skipped(capsys):
    store.write_json(store.home() / "daemon.json", {"pid": __import__("os").getpid(), "last_poll": int(time.time()),
                                                    "interval": 120, "degraded_since": int(time.time()) - 600,
                                                    "last_error_transient": True, "skipped": 2,
                                                    "last_error": "HTTP 504: gateway timeout"})
    assert cli.main(["status"]) == 0
    out = capsys.readouterr().out
    assert "DEGRADED since" in out and "skipped 2" in out
