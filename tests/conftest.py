import pytest


@pytest.fixture(autouse=True)
def isolated(tmp_path, monkeypatch):
    """Every test gets its own state directory and no config file, never the developer's own."""
    monkeypatch.setenv("PRHUB_HOME", str(tmp_path / "state"))
    monkeypatch.setenv("PRHUB_CONFIG", str(tmp_path / "config.toml"))
    for var in ("PRHUB_CALLER", "HERDR_PANE_ID", "TMUX_PANE", "PRHUB_ALLOW"):
        monkeypatch.delenv(var, raising=False)
    return tmp_path
