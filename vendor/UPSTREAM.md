# WezTerm core source mirrors

`wezterm-cell`, `wezterm-char-props`, `wezterm-escape-parser`, and
`wezterm-surface` are exact source mirrors from WezTerm commit
`78cd82dbba7315814bfbff40e246b8bed4b702e7`. They are included because those
exact revisions are not all independently published on crates.io.

They remain unmodified; Astra-specific changes belong only in
`astra-wezterm-term`. An upgrade must replace these mirrors atomically with the
fork baseline and verify source diffs and licenses. All four upstream packages
are MIT licensed; `wezterm-char-props` includes generated Unicode data governed
by the upstream repository's documented Unicode terms.
