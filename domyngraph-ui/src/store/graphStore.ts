import { create } from "zustand";
import type {
  GraphNode,
  GraphEdge,
  GraphResponse,
  ExpandRecord,
  Perspective,
} from "../types/graph";

const DEFAULT_PERSPECTIVES: Perspective[] = [
  { name: "Full Graph", nodeTypes: null, edgeTypes: null },
  {
    name: "People & Companies",
    nodeTypes: ["Person", "Company"],
    edgeTypes: ["RELATION", "WORKS_AT"],
  },
];

interface GraphState {
  baseGraph: { nodes: Map<string, GraphNode>; edges: Map<string, GraphEdge> };
  viewGraph: { nodes: Map<string, GraphNode>; edges: Map<string, GraphEdge> };
  graphVersion: number;

  focusedNodeId: string | null;
  primarySelectedNode: string | null;
  primarySelectedEdge: string | null;
  selectedNodes: Set<string>;

  hiddenNodes: Set<string>;
  pinnedNodes: Set<string>;
  expandedNodes: Map<string, ExpandRecord>;

  activeTenant: string;
  activePerspective: string;
  activeLayout: "force" | "dagre" | "radial";
  showEdgeArrows: boolean;
  perspectives: Perspective[];

  addGraphData: (response: GraphResponse) => void;
  hydrateNode: (id: string, fullProps: Record<string, unknown>) => void;
  hideNode: (id: string) => void;
  unhideNode: (id: string) => void;
  pinNode: (id: string) => void;
  selectNode: (id: string) => void;
  selectEdge: (id: string) => void;
  multiSelect: (ids: string[]) => void;
  clearSelection: () => void;
  setFocus: (id: string | null) => void;
  recordExpand: (id: string, record: ExpandRecord) => void;
  collapseNode: (id: string) => void;
  isExpanded: (id: string) => boolean;
  shouldReExpand: (
    id: string,
    depth: number,
    edgeTypes: string | null
  ) => boolean;
  resetView: () => void;
  resetWorkspace: () => void;
  setTenant: (tenant: string) => void;
  setPerspective: (name: string) => void;
  setLayout: (layout: "force" | "dagre" | "radial") => void;
  toggleEdgeArrows: () => void;
  recomputeViewGraph: () => void;
}

export const useGraphStore = create<GraphState>((set, get) => ({
  baseGraph: { nodes: new Map(), edges: new Map() },
  viewGraph: { nodes: new Map(), edges: new Map() },
  graphVersion: 0,

  focusedNodeId: null,
  primarySelectedNode: null,
  primarySelectedEdge: null,
  selectedNodes: new Set(),

  hiddenNodes: new Set(),
  pinnedNodes: new Set(),
  expandedNodes: new Map(),

  activeTenant: "demo",
  activePerspective: "Full Graph",
  activeLayout: "force",
  showEdgeArrows: false,
  perspectives: DEFAULT_PERSPECTIVES,

  addGraphData: (response) => {
    set((state) => {
      const nodes = new Map(state.baseGraph.nodes);
      const edges = new Map(state.baseGraph.edges);

      for (const n of response.nodes) {
        const existing = nodes.get(n.id);
        if (existing) {
          // Monotonic hydration: never downgrade
          if (existing.hydrated && !n.hydrated) {
            nodes.set(n.id, {
              ...existing,
              properties: { ...existing.properties, ...n.properties },
            });
          } else {
            nodes.set(n.id, {
              ...n,
              properties: { ...existing.properties, ...n.properties },
            });
          }
        } else {
          nodes.set(n.id, n);
        }
      }

      for (const e of response.edges) {
        if (!edges.has(e.id)) {
          edges.set(e.id, e);
        }
      }

      const newVersion = state.graphVersion + 1;
      const newBase = { nodes, edges };

      // Recompute viewGraph inline
      const view = recompute(
        newBase,
        state.perspectives.find((p) => p.name === state.activePerspective) ||
          DEFAULT_PERSPECTIVES[0],
        state.hiddenNodes
      );

      return {
        baseGraph: newBase,
        viewGraph: view,
        graphVersion: newVersion,
      };
    });
  },

  hydrateNode: (id, fullProps) => {
    set((state) => {
      const nodes = new Map(state.baseGraph.nodes);
      const existing = nodes.get(id);
      if (existing) {
        nodes.set(id, {
          ...existing,
          hydrated: true,
          properties: { ...existing.properties, ...fullProps },
        });
      }
      const newBase = { nodes, edges: state.baseGraph.edges };
      const view = recompute(
        newBase,
        state.perspectives.find((p) => p.name === state.activePerspective) ||
          DEFAULT_PERSPECTIVES[0],
        state.hiddenNodes
      );
      return {
        baseGraph: newBase,
        viewGraph: view,
        graphVersion: state.graphVersion + 1,
      };
    });
  },

  hideNode: (id) =>
    set((state) => {
      const hidden = new Set(state.hiddenNodes);
      hidden.add(id);
      const view = recompute(
        state.baseGraph,
        state.perspectives.find((p) => p.name === state.activePerspective) ||
          DEFAULT_PERSPECTIVES[0],
        hidden
      );
      return { hiddenNodes: hidden, viewGraph: view };
    }),

  unhideNode: (id) =>
    set((state) => {
      const hidden = new Set(state.hiddenNodes);
      hidden.delete(id);
      const view = recompute(
        state.baseGraph,
        state.perspectives.find((p) => p.name === state.activePerspective) ||
          DEFAULT_PERSPECTIVES[0],
        hidden
      );
      return { hiddenNodes: hidden, viewGraph: view };
    }),

  pinNode: (id) =>
    set((state) => {
      const pinned = new Set(state.pinnedNodes);
      if (pinned.has(id)) pinned.delete(id);
      else pinned.add(id);
      return { pinnedNodes: pinned };
    }),

  selectNode: (id) => set({ primarySelectedNode: id, primarySelectedEdge: null }),

  selectEdge: (id) => set({ primarySelectedEdge: id, primarySelectedNode: null }),

  multiSelect: (ids) =>
    set((state) => {
      const sel = new Set(state.selectedNodes);
      ids.forEach((id) => sel.add(id));
      return { selectedNodes: sel };
    }),

  clearSelection: () =>
    set({ primarySelectedNode: null, primarySelectedEdge: null, selectedNodes: new Set() }),

  setFocus: (id) => set({ focusedNodeId: id }),

  recordExpand: (id, record) =>
    set((state) => {
      const expanded = new Map(state.expandedNodes);
      expanded.set(id, record);
      return { expandedNodes: expanded };
    }),

  shouldReExpand: (id, depth, edgeTypes) => {
    const existing = get().expandedNodes.get(id);
    if (!existing) return true;
    if (depth > existing.depth) return true;
    if (edgeTypes !== existing.edgeTypes) return true;
    return false;
  },

  isExpanded: (id) => get().expandedNodes.has(id),

  collapseNode: (id) =>
    set((state) => {
      const nodes = new Map(state.baseGraph.nodes);
      const edges = new Map(state.baseGraph.edges);
      const expanded = new Map(state.expandedNodes);

      const neighborEdges = Array.from(edges.values()).filter(
        (e) => e.source === id || e.target === id
      );
      const neighborIds = new Set(
        neighborEdges.flatMap((e) => [e.source, e.target]).filter((nid) => nid !== id)
      );

      for (const nid of neighborIds) {
        const isConnectedToOther = Array.from(edges.values()).some(
          (e) =>
            (e.source === nid || e.target === nid) &&
            e.source !== id &&
            e.target !== id
        );
        if (!isConnectedToOther && !state.pinnedNodes.has(nid)) {
          nodes.delete(nid);
          expanded.delete(nid);
        }
      }

      for (const [eid, e] of edges) {
        if (!nodes.has(e.source) || !nodes.has(e.target)) {
          edges.delete(eid);
        }
      }

      expanded.delete(id);

      const newBase = { nodes, edges };
      const view = recompute(
        newBase,
        state.perspectives.find((p) => p.name === state.activePerspective) ||
          DEFAULT_PERSPECTIVES[0],
        state.hiddenNodes
      );

      return {
        baseGraph: newBase,
        viewGraph: view,
        expandedNodes: expanded,
        graphVersion: state.graphVersion + 1,
      };
    }),

  resetView: () =>
    set((state) => {
      const view = recompute(
        state.baseGraph,
        state.perspectives.find((p) => p.name === state.activePerspective) ||
          DEFAULT_PERSPECTIVES[0],
        new Set()
      );
      return {
        viewGraph: view,
        hiddenNodes: new Set(),
        pinnedNodes: new Set(),
        primarySelectedNode: null,
        selectedNodes: new Set(),
        focusedNodeId: null,
      };
    }),

  resetWorkspace: () =>
    set({
      baseGraph: { nodes: new Map(), edges: new Map() },
      viewGraph: { nodes: new Map(), edges: new Map() },
      graphVersion: 0,
      focusedNodeId: null,
      primarySelectedNode: null,
      selectedNodes: new Set(),
      hiddenNodes: new Set(),
      pinnedNodes: new Set(),
      expandedNodes: new Map(),
      activeTenant: "demo",
      activePerspective: "Full Graph",
      activeLayout: "force",
    }),

  setTenant: (tenant) =>
    set({
      activeTenant: tenant,
      baseGraph: { nodes: new Map(), edges: new Map() },
      viewGraph: { nodes: new Map(), edges: new Map() },
      expandedNodes: new Map(),
      graphVersion: 0,
    }),

  setPerspective: (name) =>
    set((state) => {
      const persp =
        state.perspectives.find((p) => p.name === name) ||
        DEFAULT_PERSPECTIVES[0];
      const view = recompute(state.baseGraph, persp, state.hiddenNodes);
      return { activePerspective: name, viewGraph: view };
    }),

  setLayout: (layout) => set({ activeLayout: layout }),

  toggleEdgeArrows: () =>
    set((state) => ({ showEdgeArrows: !state.showEdgeArrows })),

  recomputeViewGraph: () =>
    set((state) => {
      const persp =
        state.perspectives.find((p) => p.name === state.activePerspective) ||
        DEFAULT_PERSPECTIVES[0];
      const view = recompute(state.baseGraph, persp, state.hiddenNodes);
      return { viewGraph: view };
    }),
}));

/**
 * Derive viewGraph from baseGraph.
 * Order: baseGraph -> perspective filter -> hiddenNodes filter
 */
function recompute(
  base: { nodes: Map<string, GraphNode>; edges: Map<string, GraphEdge> },
  perspective: Perspective,
  hiddenNodes: Set<string>
): { nodes: Map<string, GraphNode>; edges: Map<string, GraphEdge> } {
  let filteredNodes = new Map(base.nodes);

  // Step 1: perspective filter
  if (perspective.nodeTypes) {
    const allowed = new Set(perspective.nodeTypes);
    for (const [id, node] of filteredNodes) {
      if (!allowed.has(node.type)) {
        filteredNodes.delete(id);
      }
    }
  }

  // Step 2: hidden nodes filter
  for (const id of hiddenNodes) {
    filteredNodes.delete(id);
  }

  // Step 3: filter edges — both endpoints must be in visible nodes
  const visibleIds = new Set(filteredNodes.keys());
  let filteredEdges = new Map<string, GraphEdge>();

  for (const [id, edge] of base.edges) {
    if (perspective.edgeTypes && !perspective.edgeTypes.includes(edge.label)) {
      continue;
    }
    if (visibleIds.has(edge.source) && visibleIds.has(edge.target)) {
      filteredEdges.set(id, edge);
    }
  }

  return { nodes: filteredNodes, edges: filteredEdges };
}
