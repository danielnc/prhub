"""Fuzz the code that reads text agents and users type: PR and commit names, and the hook's command matcher.

Run by ClusterFuzzLite (see .clusterfuzzlite/). A crash is any exception other than the ValueError the parsers raise
on purpose for text they cannot read, or an input that makes the hook's regular expressions take too long.
"""
import sys

import atheris

with atheris.instrument_imports():
    from prhub import cli, commits, refs


def fake_remote_of(path):
    """Never shell out to git while fuzzing: a bare number or "@sha" names something in "the current clone"."""
    return ("acme", "api")


refs.remote_of = fake_remote_of
commits.remote_of = fake_remote_of

RESOLVER = refs.RefResolver(aliases={"api": "acme/api", "web": "acme/web-app"}, repos={"api": "acme/api"})


def TestOneInput(data: bytes) -> None:
    text = atheris.FuzzedDataProvider(data).ConsumeUnicodeNoSurrogates(4096)
    for parse in (RESOLVER.resolve, lambda t: commits.resolve(t, RESOLVER)):
        try:
            ref = parse(text)
        except ValueError:
            continue
        assert ref.owner and ref.repo and str(ref)
    commits.is_commit_text(text)
    verdict = cli.hook_verdict(text)
    assert verdict is None or "prhub wait" in verdict


def main() -> None:
    atheris.Setup(sys.argv, TestOneInput)
    atheris.Fuzz()


if __name__ == "__main__":
    main()
