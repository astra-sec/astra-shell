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

Activated private trial on2026-10-27: binds only Icelake's existing
private Tailscale address `100.125.87.101:4435`, run rootless as xy/UID1000, with a
new 0700 state directory, independent certificate/catalog, and only xy's existing
public authorized SSH keys. Do not bind wildcard/public addresses, alter firewall
rules, restart the shared gateway or replace either user's worker. This does not
migrate current PTYs; the old Host remains usable and the trial creates new
terminals in the same home/work directories. The trial may be stopped separately
without affecting the original service; any trial terminals must be accounted for
before stopping. Full provisioning and verification receipts live in the Apple
worktree `.build/tui-scroll-residual-20261026/`.

## Private-path UDP offload diagnosis

The first private trial authenticated and spawned/attached owned terminals, but
rich initial states timed out before a complete decode. A zero-history fixture
completed all128 inputs with 988/1793ms stop-to-import tails. A 64-history fixture
received its initial state but later timed out. This was not accepted as a usable
rich-history trial. Content-free packet metadata showed repeated PTO-shaped
backoff after bulk sends. Large packets in a TUN capture can reflect GSO/GRO;
their length alone does not prove oversized packets reached the wire.

An isolated diagnostic changed only Quinn's `enable_segmentation_offload(false)`.
The same private endpoint/certificate/authorization and existing automatic Apple
Release client then completed 1024-history alternate and primary fixtures, all128
inputs including eight reversals. This supports an offload/path compatibility
problem, without locating the faulty tunnel component or generalizing to all
Tailscale installations. The original service and user PTYs were unchanged.

`--disable-udp-segmentation-offload` exposes that existing Quinn transport setting
explicitly for this private trial. The default keeps GSO enabled. It does not
change application framing, MTU, receive/send windows, history limits, queue
capacity, protocol capabilities, or system interface settings. Disabling GSO may
cost additional CPU for bulk traffic; it is not a global performance recommendation.
Only the empty, owned private trial instance may be switched to the final build.
Actual build hashes, process/certificate preservation and final candidate results
are recorded in the Apple residual evidence directory. GUI profile creation and
physical input remain pending while native desktop input is unavailable.


## Final artifact receipt

Product source `6ce700e3e7cfd3bd1d7b1a8781f7d4f41010a68d`, Linux SHA-256
`2da53fb86b5a110de9d911b1d0adc3af5db8bdb97a8045cbd0456566f37242f3`, runs with
explicit reliable delivery and UDP offload disabled. Actual private-network final
primary fixture consumed all128 directional inputs and imported count128, with
555.099/483.963ms input-end tails. This is import timing, not physical GUI draw.
The trial has no remaining owned child PTYs and stays active for the user. Original
gateway/xy/mimi workers, registered mimi PTYs and the signed Mac app retain exact
process start identities. No public listener or firewall change was introduced.

The new GUI Host has not been saved because native desktop input is unavailable.
After unlock, add `Icelake · 滚动修复验证` for xy at100.125.87.101:4435 with existing
My Apple Device identity; certificate SHA256 is
`B1Eyu1kYBILaEwrdCvfJ3O2oVDcS1w2gkakAqBhDs8A`. Do not overwrite the old profile.
Stop only this empty trial with `systemctl --user stop astra-ui18-reliable-trial.service`;
account for user-created trial terminals before any later stop. No boot autostart
or original-PTY migration is configured.

The unchanged original33eb4b0 binary with GSO enabled passed Linux loopback rich
initial State + ACK: primary136ms and alternate135ms, each one compressed chunk.
Both loopback test PTYs and server attempts were cleaned up. Combined with the
private no-GSO control this supports a path/offload problem, without asserting
which tunnel component is faulty. Source defaults remain unchanged.
