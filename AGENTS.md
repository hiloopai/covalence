# Working in Covalence

Covalence is shared libraries for content-addressed data. Keep public APIs
small, documented and deliberate.

Read `README.md` for setup and supported commands. Cargo owns Rust builds;
`tools/glu` sequences project tasks. Keep `bin/glu` a package-selective launcher.

Name crates after their paths: `crates/lib/error` is `covalence-lib-error`, and
`tools/glu` is `covalence-tools-glu`.

`glu build` must build every configured artifact family: today the Rust
workspace and the Python wheel. Add new build steps, such as JS, there when
those packages are introduced. Plain `cargo build` must remain
usable for the Rust workspace.

Use the shared error and CLI facades. Keep concrete domain errors in their
owning crates. Keep optional terminal rendering at application boundaries.

Documentation is public; write it for readers outside the team:

- Document every public item (functions, types, enum variants, trait items).
  Each library crate sets `#![deny(missing_docs)]`.
- Start each doc with a one-line summary that stands alone. Keep the rest
  short, in plain words, with bullets for lists of cases, errors or rules.
- Define unavoidable terms (CID, DASL, DRISL, CAS) once on the crate's front
  page, with a link to the spec. A crate's `README.md`, where it has one, is
  its front page (`#![doc = include_str!("../README.md")]`).
- Check every claim about behaviour, errors and limits against the code.
  Prefer a doctest to a sentence that asserts the same thing.
- Link only what readers can follow: no internal tickets, private
  repositories or internal hostnames.
- Keep `CHANGELOG.md` based on Keep a Changelog 2.0.0. For notable user-facing
  changes, summarize the effect under `Unreleased` using Added, Changed,
  Deprecated, Removed, Fixed or Security. Mark incompatible changes with
  `**Breaking:**` and give a short upgrade note or migration link. Curate the
  entries for readers rather than copying commits; omit trivial changes.
  `glu check` validates structure, not accuracy or significance; a changelog
  edit is not required for every pull request.
- Python: docstrings are the Rust `///` comments on the PyO3 items, and
  `covalence/_native.pyi` repeats them word for word (a test checks this).
  Readers see `covalence.X`, never `covalence._native.X`.

Before handoff, run `./bin/glu ci` in the development environment. Test affected
authority boundaries and process behavior, rather than adding tests of simple
re-exports. Do not commit ignored `.context/` design files or generated artifacts.
