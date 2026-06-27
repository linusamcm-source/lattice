/**
 * CLV wire schema — TypeScript mirror of `docs/orignal_specs/DATA_MODEL.md` §A.2–A.4
 * plus the Phase 0 wire payload contract.
 *
 * This is the single, `any`-free source of truth the WebSocket boundary parses into.
 * JSON keys are camelCase exactly as they arrive on the wire (`parentId`, `childIds`,
 * `sessionId`). The {@link EventEnvelope} type is a discriminated union over the
 * envelope `type` field so consumers narrow the `payload` shape with no casts.
 *
 * @module
 */

/** Structural kind of a graph node (DATA_MODEL §A.2). */
export type NodeType = 'service' | 'module' | 'file' | 'function' | 'variable' | 'test' | 'agent';

/** Liveness/test status of a node (DATA_MODEL §A.2). */
export type NodeStatus = 'unknown' | 'passing' | 'failing' | 'running' | 'stale' | 'error';

/** Relationship a directed edge encodes (DATA_MODEL §A.3). */
export type EdgeKind =
	| 'calls'
	| 'imports'
	| 'contains'
	| 'tested_by'
	| 'authored_by'
	| 'param_source'
	| 'data_flows_from';

/** A single function parameter inside a {@link NodeSignature}. */
export interface SignatureParam {
	name: string;
	type: string;
}

/** Function signature, present on `function` nodes (DATA_MODEL §A.2). */
export interface NodeSignature {
	params: SignatureParam[];
	returns: string;
}

/** Source span of a node within its file (1-based lines/cols). */
export interface SourceRange {
	startLine: number;
	startCol: number;
	endLine: number;
	endCol: number;
}

/** Attribution for the last writer of a node. */
export interface LastTouchedBy {
	kind: 'agent' | 'human';
	id: string;
	processId: number;
}

/** Git attribution for a node. */
export interface GitMeta {
	author: string;
	commit: string;
}

/** Structural/provenance metadata for a node (DATA_MODEL §A.2). */
export interface NodeMeta {
	language: string;
	filePath: string;
	range: SourceRange;
	lastTouchedBy?: LastTouchedBy;
	git?: GitMeta;
}

/**
 * A graph node (DATA_MODEL §A.2). `parentId` is `null` for roots; `childIds`
 * lists deterministic child ids. Optional fields are absent until enrichment.
 */
export interface Node {
	id: string;
	type: NodeType;
	label: string;
	parentId: string | null;
	childIds: string[];
	status: NodeStatus;
	docs?: string;
	signature?: NodeSignature;
	meta?: NodeMeta;
}

/** A directed graph edge (DATA_MODEL §A.3). `hot` is true while on the live stack. */
export interface Edge {
	id: string;
	source: string;
	target: string;
	kind: EdgeKind;
	hot: boolean;
}

/** Phase 0 `snapshot` payload — the full current graph. */
export interface SnapshotPayload {
	nodes: Node[];
	edges: Edge[];
}

/** Phase 0 `node.upsert` payload — a single inserted-or-updated node. */
export interface NodeUpsertPayload {
	node: Node;
}

/** Phase 0 `node.remove` payload — the id of the removed node. */
export interface NodeRemovePayload {
	id: string;
}

/** Phase 0 `edge.upsert` payload — a single inserted-or-updated edge. */
export interface EdgeUpsertPayload {
	edge: Edge;
}

/** Phase 0 `edge.remove` payload — the id of the removed edge. */
export interface EdgeRemovePayload {
	id: string;
}

/** Fields shared by every {@link EventEnvelope} variant (DATA_MODEL §A.4). */
export interface EnvelopeBase {
	v: 1;
	ts: string;
	sessionId: string;
}

/**
 * CLV event envelope (DATA_MODEL §A.4), narrowed to the Phase 0 wire payload
 * contract. Discriminated on `type`; switching on it narrows `payload` with no
 * casts and no `any`. Later phases extend this union with additional `type`s.
 */
export type EventEnvelope =
	| (EnvelopeBase & { type: 'snapshot'; payload: SnapshotPayload })
	| (EnvelopeBase & { type: 'node.upsert'; payload: NodeUpsertPayload })
	| (EnvelopeBase & { type: 'node.remove'; payload: NodeRemovePayload })
	| (EnvelopeBase & { type: 'edge.upsert'; payload: EdgeUpsertPayload })
	| (EnvelopeBase & { type: 'edge.remove'; payload: EdgeRemovePayload });

/** The set of envelope `type` discriminants this Phase 0 client understands. */
export type EventType = EventEnvelope['type'];
