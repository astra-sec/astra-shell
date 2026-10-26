# UI-18 reliable private trial

`astrad serve --terminal-delivery reliable` explicitly omits only
`terminal.datagram_state` from the server's capability selection. It retains
semantic State, rendered-state ACK, cumulative diff, payload.zstd, history paging,
stream channels, input leases and input resume when offered by the client.
`automatic` remains the default; N/N-1 negotiation and worker validation are
unchanged. This is a configured use of the established reliable protocol, not
an adapter or an unadvertised fallback. An automatic Apple client can use it
without a client restart or special client build.

The configuration is needed for a separate xy trial: the current shared legacy
gateway advertises neither ACK/diff nor compression, and its synchronous byte
relay cannot give the worker an end-to-end state acknowledgment. Under a
controlled 5 Mbps / 20 ms RTT model with 1,025 rich primary rows and 120 wheel
inputs followed by eight reversed inputs, the same Apple Release library and
modern server take 5,067/4,193 ms from input end to final import with the six old
capabilities, versus 273/230 ms with reliable ACK/diff/zstd. The explicit server
policy with an unchanged automatic client offer passed at 231/310 ms, all128
inputs consumed, no injected packet drops. These are isolated import timings,
not live WAN/physical display claims.

Default DATAGRAM plus paged rich history currently fails with `history style ID
changed within epoch`; the renderer assumes cross-message table IDs are stable
although the schema scopes IDs to each message. This trial does not claim to fix
that issue. Reliable full-history coverage above was 1,025 primary rows; history
paging beyond a bounded full State still requires separate repair. Do not market
this configuration as general DATAGRAM/history acceptance.

Planned trial (not activated by this documentation): bind only Icelake's existing
private Tailscale address `100.125.87.101:4435`, run rootless as xy/UID1000, with a
new 0700 state directory, independent certificate/catalog, and only xy's existing
public authorized SSH keys. Do not bind wildcard/public addresses, alter firewall
rules, restart the shared gateway or replace either user's worker. This does not
migrate current PTYs; the old Host remains usable and the trial creates new
terminals in the same home/work directories. The trial may be stopped separately
without affecting the original service; any trial terminals must be accounted for
before stopping. Full provisioning and verification receipts live in the Apple
worktree `.build/tui-scroll-residual-20261026/`.
