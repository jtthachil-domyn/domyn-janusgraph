export interface GraphNode {
  id: string;
  label: string;
  type: string;
  hydrated: boolean;
  properties: Record<string, unknown>;
}

export interface GraphEdge {
  id: string;
  source: string;
  target: string;
  label: string;
  direction: "out" | "in" | "both";
  properties: Record<string, unknown>;
}

export interface GraphMeta {
  total_nodes: number;
  total_edges: number;
  truncated: boolean;
  cursor: string | null;
}

export interface GraphResponse {
  nodes: GraphNode[];
  edges: GraphEdge[];
  meta: GraphMeta;
}

export interface ExpandRecord {
  depth: number;
  edgeTypes: string | null;
}

export interface Perspective {
  name: string;
  nodeTypes: string[] | null;
  edgeTypes: string[] | null;
}

export interface StylingRule {
  match: { type?: string; hasProperty?: string };
  style: { fill?: string; size?: number | ((n: GraphNode) => number) };
}
