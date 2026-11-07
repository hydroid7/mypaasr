@AGENTS.md

## Claude-specific notes

- Before changing behaviour, read the relevant section of
  `docs/design.md`. The design document wins over the README if
  they disagree. If the code disagrees with the design, ask whether the code
  or the document is wrong.
- Run `cargo test` and `cargo clippy --all-targets` before reporting work as
  done. Mention whether the Docker end-to-end tests were run.
- Commit only when asked. Work happens on the `mypaas-rs` branch.
