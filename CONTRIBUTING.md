# Contributing

Thanks for taking the time to improve workdir.

## Development setup

```bash
git clone git@github.com:mv37-org/workdir.git
cd workdir
cargo build --release
cargo test --workspace
```

For local API work without KVM, use the mock runtime:

```bash
WORKDIR_RUNTIME=mock \
WORKDIR_ALLOW_INSECURE_RUNTIME=1 \
WORKDIR_ADMIN_KEY=sk_live_dev \
  cargo run -p sandboxd -- serve
```

The mock runtime executes commands on your host. Do not expose it to untrusted
networks.

## Required checks

Run these before opening a pull request:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo audit

cd sdk/typescript && npm ci && npm run build && npm pack --dry-run
cd ../python && python -m build
```

## Pull requests

- Keep changes focused. Separate mechanical formatting, dependency updates, and
  behavior changes when practical.
- Add or update tests for API, scheduler, runtime, billing, and security
  behavior.
- Update docs when behavior, deployment steps, config, or status changes.
- Do not commit local databases, keys, `.env` files, generated rootfs images,
  package build outputs, or runtime state.

## Security-sensitive changes

Changes touching sandbox isolation, preview proxying, auth, secrets, image
building, host networking, billing, or cross-org access need tests and a short
explanation of the failure mode being prevented.

Report vulnerabilities privately; see [SECURITY.md](SECURITY.md).

## License

By contributing, you agree that your contributions are licensed under the
AGPL-3.0-only license used by this repository. Contributions may also be offered
under workdir's commercial license.
