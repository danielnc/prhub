# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately, through GitHub's **Report a vulnerability** button on the
[Security tab](https://github.com/danielnc/prhub/security) of this repository, not in a public issue.

Examples of what counts: a way to make prhub send your GitHub token anywhere other than `api.github.com`, to write
outside its state directory, or to make `prhub hook claude` approve a command it should refuse.

## How prhub handles your token

prhub reads the token from `gh auth token` on every poll, holds it only in memory, and sends it only to
`https://api.github.com`. It never writes the token to its state files, its log, or anywhere else.

## Supported versions

Only the latest release receives fixes.
