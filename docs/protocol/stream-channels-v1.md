# Explicit stream channels v1

`NET-02` adds capability `transport.stream_channels` v1 without changing ALPN
`astra/1` or application protocol N=2/N-1=1. Authentication still starts with
ClientHello. After authentication, a selected peer must send a `StreamHello`
WireMessage (tag 10) before the Request on every new bidirectional stream.

AuthResult tag 4 supplies a server-generated nonzero uint64 `stream_epoch` only
when this capability is selected and authentication succeeds. Every StreamHello
echoes that epoch. It fences an authenticated connection and is independent of
the 16-byte terminal State epoch, which is not known before attaching. Reconnect
obtains a fresh epoch; resumed attachments use the new connection's epoch.

| StreamHello field | Tag | Invariant |
|---|---|---|
| version | 1 | Exactly 1 |
| channel_kind | 2 | CONTROL=1, TERMINAL=2, FILE=3; zero/unknown rejected |
| handle | 3 | At most 128 UTF-8 bytes; mapping below |
| epoch | 4 | Equals nonzero successful AuthResult.stream_epoch |
| request_id | 5 | Nonempty, at most 128 UTF-8 bytes; exactly matches Request |

TERMINAL is an Attach request and uses its terminal ID as a nonempty handle.
FILE includes every Files/1 request and file watch. BeginUpload, WriteFileChunk,
QueryUpload, CommitUpload and AbortUpload use the transfer ID as handle; other
file requests use an empty handle. All remaining requests are CONTROL with an
empty handle. Unknown or absent commands fail before dispatch. These are
application identities, never QUIC stream IDs. An empty file handle does not
replace path/snapshot validation in the file service.

The entire encoded WireMessage preamble is limited to 1 KiB and must arrive
within five seconds. The receiver checks the declared frame length before
allocating the payload. The following Request retains the existing 4 MiB frame
limit. Kind, handle, epoch and request ID are validated before resource-specific
work. Invalid metadata fails that stream; it is never reinterpreted as legacy.

CONTROL/TERMINAL/FILE use advisory QUIC send priorities 20/10/-10 on both ends.
The metadata is read and priority selected before reading a potentially large
file Request. Ordered delivery and cancellation remain per stream; priorities
do not alter protocol ordering or ownership. Existing bounded file chunks and
separate terminal/file compression permits remain in effect. Integration tests
exercise two reliable terminal streams and control RPCs during simultaneous
multi-MiB upload/download, with bounded operation completion and eventual file
completion. This finite workload check is not a claim of starvation freedom
under an unbounded higher-priority flood; mobile weak-network testing is separate.

Managed gateway forwards the expected stream epoch and validated StreamHello as
WorkerStreamHello tags 5 and 6. The worker repeats selection and request-binding
validation. Old workers do not support this new selection: deployments must
upgrade/recycle the affected worker generation before advertising it through a
gateway; updating the gateway executable alone is not sufficient.

`COMPAT-04`: absent capability means successful AuthResult.stream_epoch stays
zero, clients send Request first and servers retain legacy Request classification.
No StreamHello is emitted speculatively. Both directions of capability absence
are covered by negotiation tests, and a real QUIC client with no offer exercises
the Request-first server path. This adapter can be removed only after the N/N-1
migration window, no earlier than application v4; removed tags must be reserved.
