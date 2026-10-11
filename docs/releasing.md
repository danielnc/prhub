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
2. Tag the merge commit on `main` and push the tag:

   ```bash
   git tag -a v0.2.0 -m "prhub 0.2.0"
   git push origin v0.2.0
   ```

3. The workflow checks that the tag equals `v` + `prhub.__version__`, runs ruff and the tests, builds the sdist and
   wheel, installs the wheel in a clean venv, and then waits for your approval of the `pypi` environment before
   uploading.
4. Create a GitHub release from the tag with the CHANGELOG section as its notes.
