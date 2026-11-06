# Security Policy

## Supported versions

The latest release on the `main` branch is supported. Older releases do not
receive fixes.

## Report a vulnerability

Use GitHub Security Advisories: open the repository Security tab and choose
"Report a vulnerability". Do not open a public issue for a security problem.

Include the version, the configuration, and the smallest steps that reproduce
the problem. Do not include live keys, private model files, or personal data.

## Scope

SimpleHarn is a local, single-user harness. It binds loopback by default and
is not a multi-user service. A configuration that exposes a listener to a
network without a control token is outside the supported posture; see
`docs/reference/CONFIG.md` and `docs/reference/OPERATIONS.md`.
