# Security Policy

## Supported Versions

Only the latest release is supported with security updates.

| Version | Supported |
|---------|-----------|
| latest  | ✅        |
| older   | ❌        |

## Reporting a Vulnerability

Please **do not** open a public GitHub issue for security vulnerabilities.

Instead, report privately by emailing the maintainer (devchya) or opening a
[GitHub Security Advisory](https://github.com/dchya24/takka/security/advisories/new).

Include:

- The affected version(s)
- A description of the vulnerability and its impact
- Steps to reproduce (if known)
- Any proposed fix (optional)

You should receive an acknowledgment within 48 hours. Once a fix is
available, a security release will be published and the advisory disclosed
responsibly.

## Security Best Practices for Contributors

- Never commit secrets, API keys, or tokens. The CI secret-scan will fail the build.
- API keys must be read from environment variables or a local `.env` file
  (which is gitignored), never hard-coded or logged.
- Run `cargo audit` locally before opening a PR to catch dependency
  vulnerabilities.
- Keep dependencies pinned in `Cargo.lock` for reproducible builds.
