# Lattice — CLV Agent Protocol

> Defines the contract an **agent** (or test runner, or runtime tracer) follows so **Lattice** can attribute work and test state correctly, plus the **documentation responsibilities** agents carry. Schemas live in `DATA_MODEL.md`.
>
> Two audiences: (1) the Lattice build — the exact format the collector parses; (2) Claude Code users — a drop-in pattern to make their own agent teams observable and to wire into `team-sprint`.

---

## 1. The idea in one line

Every agent prints **CLV-tagged lines to stdout** when it touches code or runs a test. Lattice reads those lines, works out *which agent did what to which node in which run*, and lights up the graph — including an **agent layer** with a **live roster**. It's just stdout, so it works for any language, runner, or framework.

---

## 2. The wire contract

### 2.1 Canonical line
```
#CLV1 {"event":"activity","agent":"tdd-green","session":"sess-abc123","pid":48213,"node":"fn:src/auth/login.rs:authenticate","action":"modified","msg":"implemented happy path"}
```
- Prefix **`#CLV1 `** (trailing space), then **one single-line JSON object**. One event per line.
- Untagged lines are ignored by the collector and pass through to the terminal unchanged.

### 2.2 Event types
- **`activity`** — "I touched this code."
- **`test`** — "a test ran against this node."
- **`status`** — "set this node's state directly" (e.g. `running` before a long task).
- **`hotedge`** — call-path enter/exit. **Usually machine-emitted** by the runtime `tracing` subscriber, not hand-written by agents.

```
#CLV1 {"event":"test","agent":"tdd-red","session":"sess-abc123","pid":48213,"node":"fn:src/auth/login.rs:authenticate","outcome":"fail","durationMs":14,"msg":"expected Ok"}
#CLV1 {"event":"status","agent":"tdd-green","session":"sess-abc123","node":"fn:src/auth/login.rs:authenticate","outcome":"running"}
#CLV1 {"event":"hotedge","session":"sess-abc123","pid":48213,"edge":"e:authenticate->verify_token","state":"enter"}
```

### 2.3 Field reference

| Field | Required for | Meaning |
|---|---|---|
| `event` | all | `activity` \| `test` \| `status` \| `hotedge` |
| `session` | all | One ID per run — **shared by every agent in the run**. Distinguishes concurrent runs in different terminals. |
| `pid` | recommended | OS process id. Distinguishes processes within a run. |
| `agent` | agent events | Stable agent identifier (§3). |
| `node` | activity/test/status | Target node ID, `type:path:symbol[:child]`. |
| `edge` | hotedge | Target edge ID. |
| `action` | activity | `created` \| `modified` \| `deleted` |
| `outcome` | test/status | `pass` \| `fail` \| `skip` \| `running` |
| `state` | hotedge | `enter` \| `exit` |
| `durationMs` | optional | Test duration. |
| `msg` | optional | Free-text detail shown on hover. |

### 2.4 Correlation model
```
session  - one orchestration run (shared by all agents)
  +- pid        - one process within the run
       +- agent - one contributor within the process
```
**Each agent gets its own `pid`** (the OS assigns it on spawn). One `session` groups many concurrent agents; one `pid` maps to one `agent`. This keeps a `security-scanner` and a `tdd-green` separate even running side by side, and keeps parallel test runs from contaminating each other's node colours.

---

## 3. Naming agents

Short, stable, role-based IDs — keep them constant across runs so the roster accumulates a coherent picture:

| Agent ID | Role (`agent_type`) |
|---|---|
| `tdd-red` | tests-first |
| `tdd-green` | implementation |
| `refactor` | refactoring |
| `security-scanner` | security |
| `test-runner` | execution |
| `reviewer` | review |

---

## 4. The agent registry (live roster)

Lattice keeps an `agents` table keyed by `process_id` (`DATA_MODEL.md` §B.3). You don't manage it directly — the backend **upserts** a row the first time it sees a CLV line from a process:
- First CLV line from a `pid` → row created/updated, `status: active`, linked to `session_id`.
- Clean shutdown or crash → `status: inactive`.
- **Respawn** of an agent type → a **new row**, same `agent_type`/`color`, fresh `pid`.

A dormant colour/role re-activates by spawning a new process emitting under the same `agent` id. Orchestrators may also pre-register at spawn time.

---

## 5. Protocol versioning

The sentinel encodes the version: **`#CLV1`**. Each process is **pinned** to its version in `protocol_versions` (keyed by `process_id`). A breaking change bumps to `#CLV2`; the collector can support both during a transition, and `deprecated_at` lets the backend warn before a cutover. The `CLV` name and sentinel are stable regardless of the product name.

---

## 6. Documentation responsibilities (required)

Lattice surfaces extracted documentation to **non-technical stakeholders** watching the build (`SPEC.md` §6.5, §9.5). Agents are therefore responsible for keeping that documentation accurate — **good docs are part of the contract, not optional.**

### 6.1 Update docs whenever you touch code
When you **create or modify** any code element, you **must** add or update its documentation so a non-technical reader can understand what it does:
- **Functions/methods** — a doc comment stating purpose, inputs, and outputs.
- **Variables/fields** — a brief note where the meaning isn't obvious.
- **Classes/interfaces** — what the type is responsible for.
- **Modules/services** — the module's role.

### 6.2 Cascade changes up the hierarchy
A change rarely stays local. If you alter a function in a way that changes how its **class** behaves, update the class doc too; if that changes the **module's** responsibility, update the module doc; if that changes the **service's** role, update that. **Any change in functionality must filter all the way up** so the description is accurate at *every* zoom level a stakeholder might be viewing.

```
modify function  ->  re-check & update class doc
                 ->  re-check & update module doc
                 ->  re-check & update service doc
```

### 6.3 Why this matters
Stakeholders read Lattice at whatever level they're zoomed to. Stale higher-level docs make the graph misleading precisely where non-technical viewers rely on it most. Keeping docs cascaded keeps the whole picture trustworthy.

---

## 7. Reference emitters

The orchestrator sets `CLV_SESSION` once for the run and exports it to every agent process; `pid` is read from the process itself.

### 7.1 Bash
```bash
clv() { printf '#CLV1 %s\n' "$1"; }   # usage: clv '<json-without-prefix>'
clv '{"event":"activity","agent":"tdd-green","session":"'"$CLV_SESSION"'","pid":'"$$"',"node":"fn:src/auth/login.rs:authenticate","action":"modified"}'
```

### 7.2 Python
```python
import json, os
def clv(event, node, **kw):
    rec = {"event": event, "session": os.environ["CLV_SESSION"], "pid": os.getpid(), "node": node, **kw}
    print("#CLV1 " + json.dumps(rec, separators=(",", ":")), flush=True)

clv("test", "fn:src/auth/login.rs:authenticate", agent="tdd-red", outcome="fail", durationMs=14, msg="assertion failed")
```

### 7.3 Node / TypeScript
```ts
function clv(event: string, node: string, extra: Record<string, unknown> = {}) {
  const rec = { event, session: process.env.CLV_SESSION, pid: process.pid, node, ...extra };
  process.stdout.write("#CLV1 " + JSON.stringify(rec) + "\n");
}
clv("activity", "fn:src/auth/token.ts:verifyToken", { agent: "security-scanner", action: "modified" });
```

---

## 8. Integrating into a Claude Code agent

Drop this into a sub-agent's instructions:
```
## CLV reporting (required)

You are a tracked agent with id `<AGENT_ID>`. After every meaningful action,
emit a CLV line to stdout so Lattice can attribute your work:

- After modifying or creating code:
  #CLV1 {"event":"activity","agent":"<AGENT_ID>","session":"<CLV_SESSION>","node":"<NODE_ID>","action":"modified"}
- Before a long task on a node:
  #CLV1 {"event":"status","agent":"<AGENT_ID>","session":"<CLV_SESSION>","node":"<NODE_ID>","outcome":"running"}
- After a test runs:
  #CLV1 {"event":"test","agent":"<AGENT_ID>","session":"<CLV_SESSION>","node":"<NODE_ID>","outcome":"pass|fail","msg":"..."}

Documentation (required): whenever you create or modify code, add/update its doc
comment, and cascade the change upward — update the containing class, module, and
service docs so the description stays accurate at every level.

Node IDs follow `type:path:symbol`. One event per line. Never wrap in code fences.
```
Substitute `<AGENT_ID>` per agent and `<CLV_SESSION>` from the run's environment.

---

## 9. Wiring into `team-sprint`

`team-sprint` is the natural place to mint and distribute the session:
1. **At sprint start**, generate one `CLV_SESSION` (e.g. `sprint-<epic>-<timestamp>`) and export it into every worktree/agent environment.
2. **Per node** in the dependency graph, pass the assigned agent its `AGENT_ID` and inject the CLV block (§8, including the documentation requirement).
3. Because each `team-sprint` node already runs in an **isolated git worktree**, the per-process `pid` falls out naturally and keeps concurrent nodes cleanly separated.
4. Lattice's **agent layer** then mirrors the sprint live: each agent ↔ the code it produced, with bidirectional drill-down and a real-time active/inactive roster.

---

## 10. Guidance for other Claude Code users

- **Give each agent a stable role-based ID** (§3); keep it constant across runs.
- **Mint one session per run** and share it across all agents (env var is simplest).
- **Emit `activity` on every code touch and `test`/`status` around test runs** (§2.2) via a reference emitter (§7).
- **Require the documentation discipline** (§6) so the graph stays legible to non-technical viewers.
- **Keep node IDs deterministic** (`type:path:symbol`) so the same element keeps its identity across runs.
- You don't need the whole tool to benefit: CLV lines are also a clean, greppable audit log of which agent did what.
