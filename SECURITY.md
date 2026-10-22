# Security policy

Please report vulnerabilities privately through GitHub's
[private vulnerability reporting](https://github.com/samsamurai301/zapline/security/advisories/new),
not in public issues. Include a minimal reproduction and the affected version.

You should get a reply within 72 hours. Fixes are released as patch versions
and announced in the changelog and a GitHub security advisory.

Supported versions: the latest `0.x` release.

## Checking dependencies

`cargo audit` runs clean. `.cargo/audit.toml` ignores one advisory,
RUSTSEC-2023-0071 (`rsa`), because the only path to that crate is
`sqlx-mysql`, which sqlx lists in `Cargo.lock` but zapline never builds (there
is no MySQL feature; `cargo tree -i rsa --all-features` prints nothing). JWT
RS256 uses aws-lc. Apps you generate will see the same lockfile entry and can
ignore it the same way.

## What the framework does for you

See the guide's [hardening](https://samsamurai301.github.io/zapline/hardening.html)
and [accounts](https://samsamurai301.github.io/zapline/auth.html) chapters:
path-traversal-proof static files, signed and private cookies, CSRF
protection (`SameOrigin`, `Csrf`), rate limiting with trusted proxies,
security headers, body limits, Argon2id passwords, session rotation and
revocation, PKCE for OAuth, constant-time comparisons, and development
tools (`/_dev`, the unprotected admin panel) that answer only debug builds
on this machine.
