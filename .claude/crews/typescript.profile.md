# Stack Profile — Lattice Frontend (TypeScript / SvelteKit / SvelteFlow)

> **Note:** The `frontend/` directory does NOT exist yet. This profile is built from the
> verified spec (`SPEC.md`, `DATA_MODEL.md`) and the target stack declared in SPEC.md §9.
> There are no existing house conventions to grep — any convention listed here is either
> spec-mandated or canonical SvelteKit/SvelteFlow practice. This will be updated once
> `frontend/` is scaffolded.

---

## Language & Framework

- **Language:** TypeScript — strict mode, `"strict": true` in tsconfig, no `any` at WS boundaries.
- **Framework:** SvelteKit (routing, SSR/SPA config, `+page.svelte` / `+layout.svelte` conventions).
- **Build tool:** Vite (bundler for SvelteKit).
- **Graph canvas:** `@xyflow/svelte` (SvelteFlow) — hierarchical node/edge canvas with zoom-gated lazy loading.
- **Component kit:** shadcn-svelte on TailwindCSS — copy-paste components, deep customisation, dark + light mode.
- **Theming:** SvelteFlow first-class theming for dark/light; CSS variables for shadcn-svelte tokens.
- **Package manager:** `npm`.

---

## Commands (exact — copy verbatim)

```
test:       npm test               # vitest (unit + component)
typecheck:  npm run check          # svelte-check
lint:       npm run lint           # eslint + prettier
build:      npm run build          # vite build
dev:        npm run dev            # vite dev server
e2e:        npx playwright test    # Playwright e2e
```

---

## Testing Stack

- **Unit / component:** vitest + `@testing-library/svelte` (component mounting).
- **E2E:** Playwright.
- **Coverage:** vitest `--coverage` (v8 provider).
- **TDD discipline:** RED phase first — write failing tests before implementation.

---

## CLV Wire Schema Types (strict — verified from DATA_MODEL.md)

```typescript
type NodeType   = 'service' | 'module' | 'file' | 'function' | 'variable' | 'test' | 'agent';
type NodeStatus = 'unknown' | 'passing' | 'failing' | 'running' | 'stale' | 'error';
type EdgeKind   = 'calls' | 'imports' | 'contains' | 'tested_by' | 'authored_by'
                | 'param_source' | 'data_flows_from';
type CLVEventType =
  | 'node.upsert' | 'node.remove'
  | 'edge.upsert' | 'edge.remove'
  | 'status.update' | 'test.result' | 'agent.activity'
  | 'hot_edge' | 'agent.roster' | 'snapshot' | 'error';

interface CLVSignature {
  params: Array<{ name: string; type: string }>;
  returns: string;
}

interface CLVNode {
  id: string;
  type: NodeType;
  label: string;
  parentId: string | null;
  childIds: string[];
  status: NodeStatus;
  docs?: string;
  signature?: CLVSignature;
  meta?: {
    language: string;
    filePath: string;
    range: { startLine: number; startCol: number; endLine: number; endCol: number };
    lastTouchedBy?: { kind: 'agent' | 'human'; id: string; processId: number };
    git?: { author: string; commit: string };
  };
}

interface CLVEdge {
  id: string;
  source: string;
  target: string;
  kind: EdgeKind;
  hot: boolean;
}

interface CLVEnvelope<T = CLVPayload> {
  v: 1;
  ts: string;          // ISO 8601
  sessionId: string;
  type: CLVEventType;
  payload: T;
}

// Client → Server (two allowed request types)
type ClientRequest =
  | { type: 'expand';   nodeId: string }
  | { type: 'snapshot' };
```

---

## House Conventions (spec-mandated)

1. **Lazy loading is paramount.** On connect the client receives only the top-level nodes.
   Expanding a node sends an `expand` request; the backend returns that subtree only.
   Collapsing discards rendered children to bound memory. Never pre-load what isn't visible.

2. **No `any` at WS boundaries.** Every incoming WebSocket message is parsed against the
   `CLVEnvelope` type. Use a discriminated union on `type` for narrowed payloads.

3. **Reactive Svelte stores fed by the WS client.** The WS client writes to writable stores;
   components subscribe reactively. Auto-reconnect with exponential backoff; on reconnect
   send `{ type: 'snapshot' }` to resync rather than trusting stale local state.

4. **Accessibility + theming.** SvelteFlow theming for dark/light, ARIA attributes on
   interactive nodes, keyboard navigation for the canvas.

5. **Minimal surgical changes.** Match existing file style. Touch only what the task requires.

6. **Edge filtering.** The UI allows toggling edge kinds (control flow vs data flow vs hot edges).
   Hot edges animate while `hot: true`; clear on `exit` hot_edge event.

7. **Agent layer.** Agents are top-level nodes when the agent view is active. Bidirectional:
   code node → which agents touched it; agent node → which code it touched.

---

## Anti-Patterns

- No `any` — especially at WS message boundaries or SvelteFlow node/edge data types.
- No eager subtree loading — always lazy on `expand` request, discard on collapse.
- No direct DOM manipulation — use Svelte reactivity and SvelteFlow APIs.
- No hard-coded colours — use Tailwind tokens and CSS variables so dark/light mode works.
- No `console.log` in production paths — use structured logging or remove.
- No re-implementing shadcn-svelte components from scratch — copy from the registry.
