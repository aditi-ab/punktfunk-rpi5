# Contributing to punktfunk

Thanks for your interest in contributing!

## Licensing of contributions (inbound = outbound)

punktfunk is dual-licensed under **MIT OR Apache-2.0**.

> Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
> the work by you, as defined in the Apache-2.0 license, shall be dual licensed as **MIT OR
> Apache-2.0**, without any additional terms or conditions.

By opening a pull request you agree to license your contribution under these terms. This is the
standard Rust-ecosystem "inbound = outbound" model; it keeps the project's licensing unambiguous
(including the Apache-2.0 §5 contributor patent grant) and any future relicensing clean. You retain
the copyright to your contributions.

### Do not paste copyleft (or otherwise incompatibly-licensed) code

The single thing that could poison the permissive license is **copied source from a copyleft
project**. Several adjacent projects (Sunshine, Apollo, Moonlight) are GPL-3.0. You may study them
and reimplement a *technique*, protocol, or wire format — those are not copyrightable — but **never
paste their code**, and do not translate a GPL implementation line-by-line. When a comment credits
prior art, make clear it is an independent reimplementation, not a copy. The same applies to any
third party's code under a license incompatible with MIT/Apache.

If you add a new third-party dependency, it must be permissive (MIT / Apache-2.0 / BSD / ISC / Zlib /
Unicode-3.0 / etc.). `about.toml` holds the accepted-license allow-list; regenerate the attribution
file with `scripts/gen-third-party-notices.sh` when the dependency tree changes.

## Before you push

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test  --workspace
```

Generated artifacts are checked in and CI fails on drift: `include/punktfunk_core.h` (cbindgen) and
`api/openapi.json` (`cargo run -p punktfunk-host -- openapi`). Match the surrounding code's comment
density and naming. Commit messages end with the `Co-Authored-By` trailer (see `git log`).

See the [README's Build & test section](README.md#build--test-from-source) and
[Design invariants](README.md#design-invariants) for the full build/test/run guide, and the
[docs site](https://docs.punktfunk.unom.io) for architecture and per-platform guides.
