# Security Policy

workdir runs untrusted code, so security reports are high priority.

## Reporting a vulnerability

Please do not open a public issue for a suspected vulnerability.

Use GitHub private vulnerability reporting for this repository. If that is not
available, email `security@workdir.dev` with:

- the affected component or API,
- a short impact summary,
- reproduction steps or a proof of concept,
- any logs, versions, and deployment details that help reproduce the issue.

We will acknowledge valid reports promptly, triage severity, and coordinate a
fix before public disclosure.

## Supported versions

The supported open-source target is the current `main` branch and the latest
tagged release, once releases exist. Hosted workdir deployments may run patched
code ahead of the public release while a vulnerability is being fixed.

## Scope notes

- The `mock` runtime has no isolation and is for local development only. It
  refuses to start unless `WORKDIR_ALLOW_INSECURE_RUNTIME=1` is set.
- Production isolation depends on the Firecracker runtime, the jailer, KVM,
  nftables policy, and correctly built guest images.
- Known deferred hardening work is tracked in [docs/REVIEW.md](docs/REVIEW.md).

Reports about sandbox escapes, cross-tenant access, credential leakage,
SSRF, billing bypass, privilege boundaries, and image-building abuse are all in
scope.
