# Upstream provenance

- Project: WezTerm `wezterm-term`
- Repository: <https://github.com/wezterm/wezterm>
- Commit: `78cd82dbba7315814bfbff40e246b8bed4b702e7`
- Imported paths: `term/src` excluding upstream-only tests, plus the compiled
  `termwiz/data/wezterm.terminfo` database
- Imported on: 2026-08-27
- License: MIT; see `LICENSE.md`

This directory is the controlled minimal fork selected by Astra ADR-0001.
Changes relative to upstream are limited to the `Astra fork` boundary in that
ADR and are covered by Astra conformance tests. Upgrades must re-import from an
explicit commit, audit the upstream diff, and update this file and the ADR.

`assets/wezterm.terminfo` is a binary terminfo database. Regenerate it from the
pinned upstream commit with `tic -x`; never pass it through UTF-8 decoding or a
text-oriented patching tool. The vendored crate's tests parse the database so a
corrupt import fails before deployment.
