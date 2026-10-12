# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately, through GitHub's **Report a vulnerability** button on the
[Security tab](https://github.com/danielnc/prhub/security) of this repository, not in a public issue.

Examples of what counts: a way to make prhub send your GitHub token anywhere other than `api.github.com`, to write
outside its state directory, or to make `prhub hook claude` approve a command it should refuse.

## How prhub handles your token

prhub reads the token from `gh auth token` on every poll, holds it only in memory, and sends it only to
`https://api.github.com`. It never writes the token to its state files, its log, or anywhere else.

## Verifying a release

Every release from 0.1.1 on carries SLSA build provenance (`prhub.intoto.jsonl`) next to its wheel and sdist on the
[releases page](https://github.com/danielnc/prhub/releases), signed through Sigstore by this repository's publish
workflow. To check that a wheel was built from this repository at that tag, with
[slsa-verifier](https://github.com/slsa-framework/slsa-verifier):

```bash
slsa-verifier verify-artifact prhub-0.1.1-py3-none-any.whl \
  --provenance-path prhub.intoto.jsonl --source-uri github.com/danielnc/prhub --source-tag v0.1.1
```

The PyPI upload also carries PEP 740 attestations from the same workflow.

## Supported versions

Only the latest release receives fixes.
