# Contributing

Thanks for your interest in energie. Bug reports, new device drivers, documentation and
translations are all welcome. The most useful improvements right now are the ones that make it
work beyond its original house: other time zones, other batteries, and an English dashboard.

## Before you start

For anything larger than a small fix, please open an issue first so we can agree on the
approach. Changes to control logic get particular scrutiny, because they act on real equipment.

## Development

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

No hardware is needed: the tests use protocol-level simulators and a virtual clock, and
`mode = "simulate"` runs the entire application against a model house.

## Guidelines

- `crates/core` stays pure: no I/O, no wall clock, no async.
- Each test should prove a behaviour. Name it after the rule it pins down, and use values
  a reviewer can check by hand.
- Devices stay read-only unless the configuration explicitly opts in.
- Secrets come from files and never appear in logs.
- Don't include real addresses, serial numbers, tokens, locations or other personal data in
  code, fixtures or issues.
- Keep commits focused, and explain why in the commit message.

To add a device, follow [`docs/PLUGINS.md`](docs/PLUGINS.md).

## Licensing of contributions

energie is licensed under the [AGPL-3.0-or-later](LICENSE). By submitting a contribution, you
agree that it is your own work or that you have the right to submit it, and that it is
licensed under the same terms.

## Conduct

Everyone taking part is expected to follow the [code of conduct](CODE_OF_CONDUCT.md).
