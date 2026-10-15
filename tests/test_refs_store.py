import subprocess
import time

import pytest

from prhub import cli, store
from prhub.commits import CommitRef, resolve
from prhub.refs import PRRef, RefResolver, index_clones, remote_of


def clone(path, url):
    path.mkdir(parents=True)
    subprocess.run(["git", "init", "-q", str(path)], check=True)
    subprocess.run(["git", "-C", str(path), "remote", "add", "origin", url], check=True)
    return path


# ---- clones and remotes ----

def test_remote_of_reads_ssh_and_https_remotes(tmp_path):
    assert remote_of(str(clone(tmp_path / "a", "git@github.com:acme/api.git"))) == ("acme", "api")
    assert remote_of(str(clone(tmp_path / "b", "https://github.com/acme/web-app"))) == ("acme", "web-app")
    assert remote_of(str(tmp_path / "missing")) is None
    assert remote_of(str(clone(tmp_path / "c", "https://gitlab.com/acme/x.git"))) is None


def test_index_clones_finds_nested_clones(tmp_path):
    clone(tmp_path / "work" / "api", "git@github.com:acme/api.git")
    clone(tmp_path / "work" / "team" / "Web", "https://github.com/acme/Web.git")
    assert index_clones([str(tmp_path / "work")]) == {"api": "acme/api", "web": "acme/Web"}


def test_bare_numbers_and_shas_use_the_current_clone(tmp_path):
    repo = clone(tmp_path / "api", "git@github.com:acme/api.git")
    assert RefResolver(cwd=str(repo)).resolve("#42") == PRRef("acme", "api", 42)
    assert resolve("@abcdef1", cwd=str(repo)) == CommitRef("acme", "api", "abcdef1")
    with pytest.raises(ValueError, match="not a GitHub checkout"):
        RefResolver(cwd=str(tmp_path)).resolve("42")
    with pytest.raises(ValueError, match="not a GitHub clone"):
        resolve("@abcdef1", cwd=str(tmp_path))


def test_unknown_commit_nickname_and_garbage():
    with pytest.raises(ValueError, match="unknown repo nickname"):
        resolve("mystery@abcdef1")
    with pytest.raises(ValueError, match="cannot read"):
        resolve("abcdef1")


def test_cli_nicknames_come_from_config_and_clones(isolated, tmp_path, capsys):
    clone(tmp_path / "code" / "api", "git@github.com:acme/api.git")
    store.config_path().write_text(f'clone_roots = ["{tmp_path / "code"}"]\n[aliases]\nweb = "acme/web-app"\n')
    assert cli.main(["watch", "api #3", "web#4", "acme/x#5"]) == 0
    assert set(store.watches()) == {"acme/api#3", "acme/web-app#4", "acme/x#5"}
    assert (store.home() / "repos.json").exists()  # the clone index is cached


# ---- config ----

def test_config_merges_tables_and_overrides_values(isolated):
    store.config_path().write_text('interval = 120\nsearch = "is:pr is:open org:acme"\n'
                                   '[done]\nmerge_states = ["CLEAN"]\n')
    cfg = store.load_config()
    assert cfg["interval"] == 120 and cfg["search"].endswith("org:acme")
    assert cfg["done"]["merge_states"] == ["CLEAN"] and cfg["discover_every"] == 180 and cfg["clone_roots"] == []


def test_defaults_without_a_config_file():
    assert store.load_config()["search"] == "is:pr is:open author:@me"


def test_read_json_tolerates_missing_and_broken_files(tmp_path):
    assert store.read_json(tmp_path / "nope.json", {"d": 1}) == {"d": 1}
    (tmp_path / "bad.json").write_text("{not json")
    assert store.read_json(tmp_path / "bad.json") is None


def test_rate_limit_pause_expires():
    store.write_json(store.home() / "daemon.json", {"pid": 0, "rate_limited_until": 1})
    assert store.rate_limited_until() is None and not store.daemon_healthy()


def test_commit_watch_lifetimes():
    ref = CommitRef("acme", "api", "aaaaaaa")
    now = time.time()
    store.add_commit_watch(ref)  # a state read is a glance
    e = store.commit_watches()[str(ref)]
    assert now + store.STATE_KEEP - 2 <= e["keep_until"] <= now + store.STATE_KEEP + 2
    assert not store.commit_watch_expired(e, now, 3600) and store.commit_watch_expired(e, e["keep_until"], 3600)
    store.add_commit_watch(ref, keep=5000)  # a longer claim wins over a shorter one
    store.add_commit_watch(ref)
    assert store.commit_watches()[str(ref)]["keep_until"] >= now + 4990
    store.release_commit_watch(ref)  # a wait ended: tracked a little longer, then dropped
    assert store.commit_watches()[str(ref)]["keep_until"] <= time.time() + store.RELEASE_KEEP + 1
    store.release_commit_watch(CommitRef("acme", "api", "bbbbbbb"))  # nothing to release
    assert list(store.commit_watches()) == [str(ref)]
    legacy = {"added_at": 100, "last_asked": 1000}  # written before keep_until existed
    assert not store.commit_watch_expired(legacy, 1000 + 3599, 3600) and store.commit_watch_expired(legacy, 4600, 3600)
