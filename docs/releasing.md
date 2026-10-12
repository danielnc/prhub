# Releasing

prhub is published to [PyPI](https://pypi.org/project/prhub/) by the
[`publish.yml`](../.github/workflows/publish.yml) workflow whenever a `v*` tag is pushed. The workflow uses PyPI
[Trusted Publishing](https://docs.pypi.org/trusted-publishers/): PyPI trusts this repository's workflow through
OpenID Connect, so no API token is stored anywhere. Only a maintainer can publish.

## One-time setup (maintainer)

1. **Register a pending trusted publisher on PyPI.** Sign in at [pypi.org](https://pypi.org), open **Your account →
   Publishing** ([pypi.org/manage/account/publishing](https://pypi.org/manage/account/publishing/)), and under
   **Add a new pending publisher → GitHub** enter exactly:

   | Field | Value |
   |---|---|
   | PyPI Project Name | `prhub` |
   | Owner | `danielnc` |
   | Repository name | `prhub` |
   | Workflow name | `publish.yml` |
   | Environment name | `pypi` |

   The first successful upload creates the project. A pending publisher does not reserve the name: until that
   first upload, anyone can still register `prhub`.

2. **Create the `pypi` environment on GitHub.** In the repository, open **Settings → Environments → New
   environment**, name it `pypi`, and add yourself under **Required reviewers**, so every publish waits for your
   approval. Optionally limit **Deployment branches and tags** to the tag pattern `v*`.

## Cutting a release

1. On a branch, bump `__version__` in `prhub/__init__.py` (the only place the version lives) and move the
   `[Unreleased]` entries in `CHANGELOG.md` under the new version and date. Merge the pull request.
2. Create the GitHub release on `main`, with the CHANGELOG section as its notes. This creates and pushes the tag:

   ```bash
   gh release create v0.2.0 --target main --title "prhub 0.2.0" --notes "<the CHANGELOG section>"
   ```

3. The tag starts the workflow. It checks that the tag equals `v` + `prhub.__version__`, runs ruff and the tests,
   builds the sdist and wheel, and installs the wheel in a clean venv. Then, in order:
   1. the SLSA generator signs build provenance for the wheel and sdist, and attaches `prhub.intoto.jsonl` to the
      release;
   2. the wheel and sdist are attached to the release;
   3. only then is the package uploaded to PyPI (with PEP 740 attestations), after your approval of the `pypi`
      environment if you set one. If any earlier step fails, nothing reaches PyPI.

   Create the release before the tag exists (step 2 does both): the assets are attached to the release for the tag.
