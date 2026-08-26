# credSync protocol — version 1

Server-authoritative, command-based synchronization over HTTPS. State flows down as ordered
snapshots; writes flow up as domain commands the host validates. Business data is never merged
by guesswork.

**Four pages is a hard ceiling.** It is a scope alarm, not a formatting rule: if this document
outgrows four pages, scope has crept and the response is to cut, never to raise the limit. CI
enforces it.

**Non-goals.** No CRDTs, no peer-to-peer sync, no partial-field diffs, no realtime transport.
A push notification is only ever a hint to run the loop, never a carrier of state.

## 1. Concepts

| Term | Definition |
|---|---|
| **Scope** | The unit of subscription and isolation, e.g. `(institution_id, enrollment_id)`. A client syncs only its subscribed scopes. Scope membership is authorized server-side at token mint. |
| **Entity** | A synced table registered with credSync: name, scope mapping, conflict class, schema version. |
| **Change log** | Append-only `sync_changes`: `(seq bigserial, scope, entity, entity_id, op upsert\|delete, snapshot jsonb, row_version, schema_version)`. Written by domain handlers via the outbox in the same transaction as the state change. **Snapshots, not diffs; deletes are tombstones.** |
| **Cursor** | The client's last applied `seq`, per scope. Pull walks the log forward from it. |
| **Command** | A client-originated domain mutation. The only way client writes reach the server. |
| **Dedupe record** | Server table keyed by command `id` storing the outcome. Replays return the recorded result without re-applying. |

## 2. Wire discipline

Every rule about encoding, size, and timing lives here. Endpoints reference it; they do not
restate it.

- **Canonical encoding is compact JSON.** Canonical means byte-stable: the same logical value
  always encodes to identical bytes. Object keys are sorted; no incidental whitespace; one
  numeric representation. Checksums and digests are computed over this encoding, so instability
  produces phantom corruption.
- **Compression** is Brotli or gzip on the wire, by negotiation.
- **Byte budgets are compressed-size budgets.** Default 100 KB per batch. A single change larger
  than the budget is still delivered alone rather than stalling the cursor.
- **Requests tolerate 30-second completion.** A cycle interrupted mid-batch resumes from the
  persisted cursor (§4).
- **Backoff is exponential with jitter**, drawn from the client's seeded entropy source so that
  simulated runs replay identically.
- The codec is isolated in `credsync-protocol`, so a binary encoding may arrive as protocol v2
  without touching the state machine.

### 2.1 Field limits

Every wire field has an explicit limit. Oversize input is **rejected, not truncated** —
truncation converts hostile input into a silently wrong value.

| Field | Type | Limit |
|---|---|---|
| `protocol` | uint | 1..=65535 |
| `scope` | string | 128 bytes, `[A-Za-z0-9:_-]` |
| `entity` | string | 64 bytes, `[a-z0-9_]` |
| `entity_id` | string | 64 bytes |
| `seq`, `next_cursor`, `server_seq` | uint64 | 1..=2^63-1 |
| `op` | enum | `upsert` \| `delete` |
| `snapshot` | object | 256 KB uncompressed; absent when `op = delete` |
| `row_version` | uint64 | 1..=2^63-1 |
| `schema_version` | uint | 1..=65535 |
| `digest`, `checksum` | lowercase hex | ≤64 chars; exact width fixed by the algorithm chosen at CS-4 |
| `has_more` | bool | — |
| `id` (command) | UUIDv7 | 16 bytes |
| `name` (command) | string | 64 bytes, `[a-z0-9_]` |
| `payload` | object | 64 KB uncompressed |
| `client_ts` | int64 | Unix ms; advisory only |
| `status` | enum | `applied` \| `rejected` \| `superseded` |
| `reason` | string | 256 bytes; required when `status = rejected` |
| `limit_bytes` | uint | 1..=1048576; a client hint — the server may return less, never more |
| `commands[]` | array | 256 entries **and** 1 MB total uncompressed, whichever binds first |
| `batches[]`, `changes[]`, `rows[]`, `results[]` | array | bounded by the byte budget, not by count |

## 3. Endpoints

### 3.1 Bootstrap — `GET /sync/bootstrap`

```
request:  ?scope=s1&after=0        # `after` resumes a partial bootstrap
response: { protocol: 1, scope: 's1', rows: [ { entity, entity_id, snapshot,
            row_version, schema_version } ], next_cursor, has_more,
            checksum, digest }
```

A device with no local state for a scope bootstraps here, then joins the log at `next_cursor`.
Paginated and byte-budgeted like pull, and resumable via `after`. Concurrent writes during
bootstrap are safe: `next_cursor` is the log position the returned rows are consistent with, so
joining the log there neither loses nor double-applies a concurrent write.

### 3.2 Pull — `GET /sync/pull`

```
request:  ?scopes=s1:cursor1,s2:cursor2&limit_bytes=100000
response: { protocol: 1, batches: [ { scope, changes: [ { seq, entity, entity_id, op,
            snapshot, row_version, schema_version } ], next_cursor, has_more,
            checksum, digest } ] }
```

`has_more` drives continuation. A three-week-offline device simply walks forward.

### 3.3 Push — `POST /sync/push`

```
request:  { protocol: 1, commands: [ { id, name, scope, payload, client_ts, checksum } ] }
response: { protocol: 1, results: [ { id, status, reason?, server_seq? } ] }
```

- **Commands are validated against business rules on arrival** — deadline passed, enrollment
  inactive, referenced upload missing. The server is never a dumb row store for client writes.
- **Rejected commands surface with their reason.** The outbox entry moves to a dead-letter state
  visible to the user rather than being silently dropped.
- The protocol version is carried once, by the request envelope's `protocol` field — commands do
  not repeat it. The client's local outbox record retains the schema version each command was
  authored under, which is what §7 migrates forward.

## 4. Ordering and application

- Changes within a scope are **strictly `seq`-ordered**. Clients apply in order; a gap is a
  protocol violation and the batch is refetched.
- **The cursor is persisted in the same storage transaction as the rows it covers.** A process
  killed mid-apply therefore resumes with cursor and rows consistent.
- **Push precedes pull in every cycle**, so a client immediately observes the server's
  transformation of its own writes.
- Sync triggers: app foreground, connectivity regained, silent push hint, periodic backstop.

## 5. Integrity

- **Every pull batch carries a checksum** over its canonical encoding. Corruption in transit, in
  a flaky proxy cache, or in device flash is detected **before apply**; the batch is refetched,
  never half-applied.
- **Every command carries a payload checksum**, recorded with its dedupe entry. A replay whose
  body has been mutated is rejected as a distinct, invalid request rather than deduped as a
  success — otherwise the dedupe table becomes a way to launder tampered commands.
- **Per scope, both sides maintain a rolling state digest**: an order-independent hash over
  `(entity, entity_id, row_version)` for every live row. The server returns its digest with each
  pull; the client compares after apply. The digest is incremental — applying N changes
  individually equals applying them as a batch — and a tombstone leaves the digest as though the
  row had never existed.
- **A digest mismatch means silent divergence.** The client marks the scope tainted,
  re-bootstraps, replays its own outbox, and emits telemetry carrying *both* digests. A tainted
  scope does not block other scopes. Self-healing plus a signal, instead of quiet rot.

> Algorithm is **pending — decided at CS-4** by benchmark on target-class hardware: xxh3 for
> speed against BLAKE3 where tamper-evidence matters (`DECISIONS.md` O-001).

## 6. Conflict classes

The entity registry declares each entity's class in the host's schema definition. The simulator
generates its invariants per class, so a policy claim here is a tested property rather than
documentation.

| Class | Policy | Mechanics |
|---|---|---|
| **Institution truth** — grades, scores, schedules, statuses | Server-authoritative | Pull-only. No command may target them; refused by the registry, not by convention. |
| **Owner drafts** — reflections, residency log fields | LWW per field | Server-assigned `row_version` decides. `client_ts` is a tiebreaker hint only, because device clocks lie. The losing version returns to the device and is stored as a recovered draft. **Silent loss is a protocol violation, not a tradeoff.** |
| **Append-only streams** — submissions, attendance events | No conflict by construction | New versions never overwrite; ordering is server `seq`. |

## 7. Versioning

- **`protocol` on every request.** The server speaks N and N−1. Below N−1 it responds `426` with
  a forced-upgrade envelope the client core understands. The client then **queues its outbox and
  surfaces an upgrade prompt — it never drops queued work.**
- **Per-entity `schema_version` in every snapshot.** The client applies registered up-migrations
  to local rows. Migration composition is associative: v1→v2→v3 equals v1→v3.
- **Outbox entries record the schema version they were authored under.** An upgraded app migrates
  queued commands forward before pushing. A command whose schema the server no longer accepts is
  queued, never dropped.

## 8. Authorization

The host mints a short-lived JWT carrying an explicit scope list. **credSync validates scope
claims; it never authorizes.** A request for any scope outside the token's claims is refused, and
pull can never cross scopes regardless of client input. Revocation is honoured at token expiry,
plus an optional server-side scope blocklist for immediate cuts. TLS is assumed: this layer adds
integrity, not secrecy.
