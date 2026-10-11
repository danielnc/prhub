# Contributing to prhub

Thanks for helping. Bug reports with a reproduction, and hooks for agents other than Claude Code, are especially
welcome.

## Setup

```bash
git clone https://github.com/danielnc/prhub && cd prhub
uv sync                       # pytest, pytest-cov and ruff; prhub itself has no dependencies
make check                    # ruff and the offline test suite: GitHub is never called
pre-commit install            # optional: the gitleaks hook blocks commits that contain tokens
```

To run your checkout for real, point it at a scratch state directory so it does not disturb an installed daemon:

```bash
PRHUB_HOME=/tmp/prhub-dev uv run python -m prhub daemon --once
PRHUB_HOME=/tmp/prhub-dev uv run python -m prhub status
```

CI (`.github/workflows/ci.yml`) runs on every pull request: pytest on Python 3.11 to 3.14, ruff, a build and
clean-venv install of the wheel, and a gitleaks scan of the full history. Its `ci` job must pass, and `main` only
changes through pull requests. CodeQL and CodeRabbit also review pull requests.
Releases are cut by the maintainer: see [docs/releasing.md](docs/releasing.md).

## Making a change

1. Open an issue first for anything larger than a bug fix, so we can agree on the approach.
2. Write a failing test, then the change. Tests live in `tests/` and never touch the network: GraphQL responses are
   built by hand in the shape GitHub returns them.
3. Keep the runtime standard-library only.
4. Conditions are a contract with the agents that wait on them. If you change what `done`, `green` or `deployed`
   mean, say so in the pull request and in `CHANGELOG.md`, and pin the new meaning with a test.
5. Add a line to `CHANGELOG.md` under "Unreleased".

Agents contributing code: `AGENTS.md` has the same rules in checklist form.

## Privacy and secrets

Never commit tokens, `.env` files, real names, emails, hostnames, home paths, or private repository or organization
names. Use `acme/api` and `example.com` in tests and docs.

## Security issues

Please don't open a public issue for a vulnerability. Report it privately, as described in [SECURITY.md](SECURITY.md).

## License

By contributing you agree that your contributions are licensed under the MIT License (see `LICENSE`).
