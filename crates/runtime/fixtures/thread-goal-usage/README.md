# Thread goal usage workload

Base measured before implementation: 9f97a6d65a9b9c73d8075068ae2ccd9402bbd4bd.
The fixture has 1,024 sequential requests and ACKed responses, two semantic
publications (create, then pause checks), and a fixed 80% input / 20% output split.
The runtime regression compares durable mode with the unchanged full-publication
mode (no run store), stores the actual bus events through the same storage event
API as the recorder, and measures stored payload bytes, not JSON estimates.

measured-mixture.json is a deterministic stratified numerical replay of
52,545 sequential-shaped usage-only transitions from the read-only DB measurement:
1,024 midpoint quantiles, permuted by index * 613 modulo 1,024. It contains no
objective, thread/run identifiers, or other body text. Candidate median/p90 are
124,671 / 342,668 tokens. These are **candidate per-request estimates**, not
confirmed request attribution: provider request IDs and hosted-search provenance
were not independently joined. Coverage is 80.39% of 65,363 usage-only observed
request increments, not of all provider requests. Remaining transitions and
semantic boundaries are excluded, rather than treated as zero-token responses.
The weighted interval-average estimates are 110,562 / 307,420 (65,289 request
weight). Positive usage deltas have median/p90/max 125,413 / 330,882 / 6,233,459.
The long-tail maximum and over-threshold responses are tested separately.

Because exact request attribution remains unconfirmed, T stays at the specified
8,000,000 fallback. Also test 5,000 / 200,000 / 500,000 tokens and the measured
candidate median/p90. Require at least 90% reduction in both counts and stored
full payload for every representative workload. The conservative upper bound is
S + ceil(R/16) + floor(G/T); G uses u128. The implementation does not promise a
large reduction in all write transactions: each response still commits usage.

Read-only measurement used SQLite READ_ONLY plus a read-only filesystem mount
(including DB/WAL/SHM), existing event/body decoding, bounded ID ranges, and only
aggregate output. No migration, checkpoint, copy, compression or removal of
existing DB rows was performed. Startup replay is unchanged. The 413 restore
candidates (328 complete, 85 noncomplete) are under the investigation gates;
session changes corroborated 225 candidates but do not prove their source.
