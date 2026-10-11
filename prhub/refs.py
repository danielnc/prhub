"""Parse the many ways people and agents name a pull request into one canonical `owner/repo#N`."""
from __future__ import annotations

import glob
import os
import re
import subprocess
from dataclasses import dataclass

URL_RE = re.compile(r"github\.com/([\w.-]+)/([\w.-]+)/pull/(\d+)")
FULL_RE = re.compile(r"^([\w.-]+)/([\w.-]+)#(\d+)$")
NICK_RE = re.compile(r"^([\w .-]+?)\s*#\s*(\d+)$")
NUM_RE = re.compile(r"^#?(\d+)$")
REMOTE_RE = re.compile(r"github\.com[:/]([\w.-]+)/([\w.-]+?)(?:\.git)?/?$")


@dataclass(frozen=True, order=True)
class PRRef:
    owner: str
    repo: str
    number: int

    def __str__(self) -> str:
        return f"{self.owner}/{self.repo}#{self.number}"

    @property
    def slug(self) -> str:
        """Filesystem-safe name for this PR's state file."""
        return f"{self.owner}__{self.repo}__{self.number}"

    @property
    def url(self) -> str:
        return f"https://github.com/{self.owner}/{self.repo}/pull/{self.number}"


def remote_of(path: str) -> tuple[str, str] | None:
    """owner, repo of the git checkout at `path` (its `origin` remote), or None."""
    try:
        url = subprocess.run(["git", "-C", path, "remote", "get-url", "origin"],
                             capture_output=True, text=True, timeout=5).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return None
    m = REMOTE_RE.search(url)
    return (m.group(1), m.group(2)) if m else None


def index_clones(roots: list[str], depth: int = 3) -> dict[str, str]:
    """repo name (lowercased) -> 'owner/repo', from the origin remotes of git clones under `roots`."""
    idx: dict[str, str] = {}
    for root in roots:
        root = os.path.expanduser(root)
        for level in range(1, depth + 1):
            for git_dir in glob.glob(os.path.join(root, *(["*"] * level), ".git")):
                r = remote_of(os.path.dirname(git_dir))
                if r:
                    idx.setdefault(r[1].lower(), f"{r[0]}/{r[1]}")
    return idx


class RefResolver:
    """Turns user text into a PRRef. `aliases` maps nicknames ("backend") to repo names or owner/repo;
    `repos` maps repo names to owner/repo (usually from `index_clones`)."""

    def __init__(self, aliases: dict[str, str] | None = None, repos: dict[str, str] | None = None,
                 cwd: str | None = None):
        self.aliases = {k.lower(): v for k, v in (aliases or {}).items()}
        self.repos = {k.lower(): v for k, v in (repos or {}).items()}
        self.cwd = cwd

    def _repo_for(self, name: str) -> str | None:
        name = name.strip().lower()
        target = self.aliases.get(name, name)
        if "/" in target:
            return target
        return self.repos.get(target.lower())

    def resolve(self, text: str) -> PRRef:
        t = text.strip()
        if m := URL_RE.search(t):
            return PRRef(m.group(1), m.group(2), int(m.group(3)))
        if m := FULL_RE.match(t):
            return PRRef(m.group(1), m.group(2), int(m.group(3)))
        if m := NUM_RE.match(t):
            r = remote_of(self.cwd or os.getcwd())
            if not r:
                raise ValueError(f"'{t}' is a bare number but the current directory is not a GitHub checkout")
            return PRRef(r[0], r[1], int(m.group(1)))
        if m := NICK_RE.match(t):
            repo = self._repo_for(m.group(1))
            if repo:
                owner, name = repo.split("/", 1)
                return PRRef(owner, name, int(m.group(2)))
            raise ValueError(f"unknown repo nickname '{m.group(1)}': add it to [aliases] in the prhub config")
        raise ValueError(f"cannot read '{t}' as a pull request (try a URL or owner/repo#N)")
