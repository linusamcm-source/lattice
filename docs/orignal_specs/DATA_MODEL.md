# Lattice — Data Model & Contracts

> The single source of truth for Lattice's **wire schema** (JSON over WebSocket) and **database schema** (SQLite / Postgres). `SPEC.md` and `AGENT_PROTOCOL.md` reference this doc rather than restating it.

---

## Part A — Wire schema (JSON over WebSocket)

Chosen over XML for cheap diff/patch and natural fit with SvelteFlow's reactive stores.

### A.1 ID convention
Every node/edge ID is **deterministic** so the same element keeps its identity across runs and restarts:
```
node: type:path:symbol[:child]     e.g.  fn:src/auth/login.rs:authenticate
edge: e:<source-symbol>-><target-symbol>   e.g.  e:authenticate->verify_token
```

### A.2 Node
```json
{
  "id": "fn:src/auth/login.rs:authenticate",
  "type": "function",
  "label": "authenticate",
  "parentId": "file:src/auth/login.rs",
  "childIds": ["var:src/auth/login.rs:authenticate:user"],
  "status": "passing",
  "docs": "Authenticates a user against the stored hash. Returns Ok(token) on success, Err(AuthError) otherwise.",
  "signature": {
    "params": [{ "name": "creds", "type": "Credentials" }],
    "returns": "Result<Token, AuthError>"
  },
  "meta": {
    "language": "rust",
    "filePath": "src/auth/login.rs",
    "range": { "startLine": 12, "startCol": 0, "endLine": 28, "endCol": 1 },
    "lastTouchedBy": { "kind": "agent", "id": "tdd-green", "processId": 48213 },
    "git": { "author": "linus", "commit": "a1b2c3d" }
  }
}
```
- **`type`**: `service | module | file | function | variable | test | agent`
- **`status`**: `unknown | passing | failing | running | stale | error`
- **`docs`**: extracted documentation (function/class/module/variable). Cascades up the hierarchy (see `AGENT_PROTOCOL.md` §6).
- **`signature`**: present for `function` nodes; drives parameter-dependency edges (A.3).

### A.3 Edge
```json
{ "id": "e:authenticate->verify_token", "source": "fn:src/auth/login.rs:authenticate",
  "target": "fn:src/auth/token.rs:verify_token", "kind": "calls", "hot": false }
```
- **`kind`**:
  - `calls` — control flow (caller → callee).
  - `imports` — module import.
  - `contains` — structural containment (mirrors parent/child).
  - `tested_by` — node ↔ its tests.
  - `authored_by` — code node ↔ agent that touched it (powers the agent layer).
  - `param_source` — this function's **input** originates from the target's return value.
  - `data_flows_from` — this function's **output** flows into the target.
- **`hot`**: `true` while on the executing stack (runtime tracer).

> `param_source` / `data_flows_from` are **static, AST-derived** — they describe data dependency, not runtime values.

### A.4 Event envelope
```json
{ "v": 1, "ts": "2026-06-27T10:32:01.123Z", "sessionId": "sess-abc123", "type": "node.upsert", "payload": {} }
```
- **`type`**: `node.upsert | node.remove | edge.upsert | edge.remove | status.update | test.result | agent.activity | hot_edge | agent.roster | snapshot | error`
- `snapshot` carries the full current graph (sent on connect / reconnect resync).
- Client→server requests: `expand` (request a subtree), `snapshot` (request full resync).

### A.5 Payloads

**`test.result`**
```json
{ "nodeId": "fn:src/auth/login.rs:authenticate", "testId": "auth::test_valid", "outcome": "fail",
  "durationMs": 14, "sessionId": "sess-abc123", "agentId": "tdd-red", "processId": 48213, "message": "expected Ok, got Err" }
```

**`agent.activity`**
```json
{ "agentId": "security-scanner", "action": "modified", "nodeId": "fn:src/auth/token.rs:verify_token",
  "sessionId": "sess-abc123", "processId": 48590 }
```

**`hot_edge`** (runtime call path)
```json
{ "edgeId": "e:authenticate->verify_token", "state": "enter", "processId": 48213,
  "sessionId": "sess-abc123", "agentId": "tdd-green", "ts": "2026-06-27T10:32:01.500Z" }
```
- **`state`**: `enter` sets `hot:true`; `exit` clears it.

**`agent.roster`** (live roster snapshot/delta)
```json
{ "sessionId": "sess-abc123", "agents": [
  { "processId": 48213, "agentId": "tdd-green", "agentType": "implementation", "color": "#2ecc71", "status": "active" },
  { "processId": 48590, "agentId": "security-scanner", "agentType": "security", "color": "#e67e22", "status": "inactive" }
]}
```

---

## Part B — Database schema

Relational, **not** vectorised (no semantic search in v1). One `sqlx`-based trait, two backends:
```
LATTICE_DB_URL=sqlite://./.lattice/graph.db
LATTICE_DB_URL=postgres://user:pass@host:5432/lattice
```
Same schema, only the driver changes. The user can run a local Docker Postgres or point at a shared network instance.

### B.1 `process_id` is the correlation thread
`process_id` appears in **every** table and ties an event back to one execution context.
- **Primary key** on process-scoped tables: `agents`, `protocol_versions`.
- **Mandatory foreign key** on event tables: `test_results`, `agent_activity`, hot-edge records.
- **Structural tables (`nodes`, `edges`) keep deterministic structural IDs as primary keys** — a node persists across many processes and is touched by many agents — and carry `process_id` / `agent_id` as **attribution** columns. (A pure `process_id` PK on nodes would break identity across runs.)

### B.2 Session / process / agent hierarchy
One `session` (one orchestration run) → **many** concurrent `process_id`s → each maps to exactly **one** `agent`. `session_id` groups concurrent agents, so "all agents in this run" is a query on `agents` by `session_id`.

### B.3 Agent registry lifecycle
The backend **upserts** an agent row on the **first CLV line** from a process: `status: active`, `pid` captured, linked to `session_id`. Clean shutdown or crash → `status: inactive`. **Respawn** of an agent type → a **new row**, same `agent_type` + `color`, fresh `process_id`. A dormant colour/role re-activates simply by spawning a new process that emits under the same `agent` id. (Conductor / `team-sprint` may also pre-register at spawn time.)

### B.4 Protocol version
Each process is **pinned** to its CLV version in `protocol_versions` (keyed by `process_id`). Breaking changes bump the sentinel (`#CLV1` → `#CLV2`); both can be supported during a transition, and `deprecated_at` lets the backend warn before a cutover.

### B.5 Persisted vs ephemeral
Only **parsed, structured CLV events** are written (`test_results`, `agent_activity`, hot-edge records, node/edge state). **Raw stdout is ephemeral and not persisted** — it already exists in the terminal, and storing it would bloat the DB without adding queryable value.

### B.6 Tables (DDL sketch)

```sql
CREATE TABLE sessions (
  session_id   TEXT PRIMARY KEY,
  started_at   TIMESTAMP NOT NULL,
  repo_path    TEXT NOT NULL,
  label        TEXT
);

-- process-scoped: process_id is the primary key
CREATE TABLE agents (
  process_id   BIGINT PRIMARY KEY,
  agent_id     TEXT NOT NULL,            -- e.g. tdd-green
  agent_type   TEXT NOT NULL,            -- role, e.g. implementation
  color        TEXT NOT NULL,
  status       TEXT NOT NULL,            -- active | inactive
  session_id   TEXT NOT NULL REFERENCES sessions(session_id),
  created_at   TIMESTAMP NOT NULL,
  updated_at   TIMESTAMP NOT NULL
);

CREATE TABLE protocol_versions (
  process_id   BIGINT PRIMARY KEY REFERENCES agents(process_id),
  version      TEXT NOT NULL,            -- e.g. "1"
  session_id   TEXT NOT NULL REFERENCES sessions(session_id),
  introduced_at TIMESTAMP NOT NULL,
  deprecated_at TIMESTAMP,               -- nullable
  features_json TEXT                     -- JSON describing what changed
);

-- structural: deterministic IDs are primary keys; process/agent are attribution
CREATE TABLE nodes (
  id             TEXT PRIMARY KEY,       -- type:path:symbol
  session_id     TEXT NOT NULL REFERENCES sessions(session_id),
  type           TEXT NOT NULL,
  label          TEXT NOT NULL,
  parent_id      TEXT,
  status         TEXT NOT NULL,
  docs           TEXT,                   -- extracted documentation
  signature_json TEXT,                   -- function params/returns
  meta_json      TEXT,
  last_process_id BIGINT,                -- attribution: who last touched it
  last_agent_id  TEXT,
  updated_at     TIMESTAMP NOT NULL
);

CREATE TABLE edges (
  id          TEXT PRIMARY KEY,          -- e:source->target
  session_id  TEXT NOT NULL REFERENCES sessions(session_id),
  source      TEXT NOT NULL,
  target      TEXT NOT NULL,
  kind        TEXT NOT NULL,             -- calls|imports|contains|tested_by|authored_by|param_source|data_flows_from
  hot         BOOLEAN NOT NULL DEFAULT 0
);

CREATE TABLE test_results (
  id          TEXT PRIMARY KEY,
  process_id  BIGINT NOT NULL REFERENCES agents(process_id),
  session_id  TEXT NOT NULL REFERENCES sessions(session_id),
  node_id     TEXT NOT NULL,
  test_id     TEXT NOT NULL,
  outcome     TEXT NOT NULL,             -- pass|fail|skip|running
  duration_ms INTEGER,
  agent_id    TEXT,
  message     TEXT,
  ts          TIMESTAMP NOT NULL
);

CREATE TABLE agent_activity (
  id          TEXT PRIMARY KEY,
  process_id  BIGINT NOT NULL REFERENCES agents(process_id),
  session_id  TEXT NOT NULL REFERENCES sessions(session_id),
  agent_id    TEXT NOT NULL,
  action      TEXT NOT NULL,             -- created|modified|deleted
  node_id     TEXT NOT NULL,
  ts          TIMESTAMP NOT NULL
);
```

> **Builder note:** `protocol_versions` pins each *process* to its version per the agreed design. If many processes share a version, the global catalogue (`version` / `introduced_at` / `deprecated_at` / `features`) can be normalised into its own table later — left denormalised here to match the model decided in design. `BIGINT` is used for `process_id`; adjust to the platform's PID width as needed.

### B.7 Suggested indexes
- `nodes(parent_id)` — subtree expansion.
- `nodes(session_id, type)` — agent/type filtering.
- `edges(source)`, `edges(target)`, `edges(kind)` — traversal and edge-type filtering.
- `agents(session_id, status)` — live roster queries.
- `test_results(node_id, ts)` and `agent_activity(node_id, ts)` — per-node history.
