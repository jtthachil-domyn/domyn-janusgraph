import { useEffect, useRef, useCallback, useState } from "react";
import { Graph } from "@antv/g6";
import { useGraphStore } from "../store/graphStore";
import { expandVertex, getVertexDetail } from "../api/client";
import type { GraphNode } from "../types/graph";
import { message } from "antd";

const TYPE_COLORS: Record<string, string> = {
  Company: "#4ea8de",
  Person: "#a78bfa",
  Concept: "#22c55e",
  Entity: "#f59e0b",
  Technology: "#06b6d4",
};

const TYPE_ABBREV: Record<string, string> = {
  Company: "Co",
  Person: "P",
  Concept: "C",
  Entity: "E",
  Technology: "T",
};

function nodeColor(node: GraphNode): string {
  return TYPE_COLORS[node.type] || "#6b7280";
}

function layoutConfig(layout: string, nodeCount = 0) {
  if (layout === "dagre") {
    return { type: "dagre" as const, rankdir: "TB", nodesep: 60, ranksep: 80 };
  }
  if (layout === "radial") {
    return { type: "radial" as const, unitRadius: 120, linkDistance: 150 };
  }
  if (nodeCount > 2000) {
    return {
      type: "d3-force" as const,
      link: { distance: 60 },
      charge: { strength: -30 },
      collide: { radius: 10 },
      simulation: { alphaDecay: 0.1 },
    };
  }
  if (nodeCount > 500) {
    return {
      type: "d3-force" as const,
      link: { distance: 100 },
      charge: { strength: -120 },
      collide: { radius: 20 },
    };
  }
  return {
    type: "d3-force" as const,
    link: { distance: 160 },
    charge: { strength: -400 },
    collide: { radius: 40 },
  };
}

function buildNodeData(viewGraph: ReturnType<typeof useGraphStore.getState>["viewGraph"]) {
  const edgeCounts = new Map<string, number>();
  for (const e of viewGraph.edges.values()) {
    edgeCounts.set(e.source, (edgeCounts.get(e.source) || 0) + 1);
    edgeCounts.set(e.target, (edgeCounts.get(e.target) || 0) + 1);
  }
  return {
    nodes: Array.from(viewGraph.nodes.values()).map((n) => ({
      id: n.id,
      data: {
        label: n.label,
        nodeType: n.type,
        edgeCount: edgeCounts.get(n.id) || 0,
        ...n.properties,
      },
      style: { fill: nodeColor(n), stroke: "#fff", lineWidth: 2 },
    })),
    edges: Array.from(viewGraph.edges.values()).map((e) => ({
      id: e.id,
      source: e.source,
      target: e.target,
      data: { edgeLabel: e.label, weight: e.properties.weight },
      style: { stroke: "#c4c4c4", lineWidth: 1 },
    })),
  };
}

export default function GraphCanvas() {
  const containerRef = useRef<HTMLDivElement>(null);
  const graphRef = useRef<Graph | null>(null);
  const destroyedRef = useRef(false);
  const [graphReady, setGraphReady] = useState(0);
  const renderedVersionRef = useRef(-1);

  const viewGraph = useGraphStore((s) => s.viewGraph);
  const graphVersion = useGraphStore((s) => s.graphVersion);
  const showEdgeArrows = useGraphStore((s) => s.showEdgeArrows);
  const focusedNodeId = useGraphStore((s) => s.focusedNodeId);
  const activeLayout = useGraphStore((s) => s.activeLayout);
  const addGraphData = useGraphStore((s) => s.addGraphData);
  const selectNode = useGraphStore((s) => s.selectNode);
  const selectEdge = useGraphStore((s) => s.selectEdge);
  const setFocus = useGraphStore((s) => s.setFocus);
  const recordExpand = useGraphStore((s) => s.recordExpand);
  const isExpanded = useGraphStore((s) => s.isExpanded);
  const collapseNode = useGraphStore((s) => s.collapseNode);
  const hydrateNode = useGraphStore((s) => s.hydrateNode);
  const activeTenant = useGraphStore((s) => s.activeTenant);
  const primarySelectedNode = useGraphStore((s) => s.primarySelectedNode);
  const primarySelectedEdge = useGraphStore((s) => s.primarySelectedEdge);

  const handleToggleExpand = useCallback(
    async (nodeId: string) => {
      if (isExpanded(nodeId)) {
        collapseNode(nodeId);
        return;
      }
      try {
        const resp = await expandVertex(nodeId, activeTenant, 1, 50);
        addGraphData(resp);
        recordExpand(nodeId, { depth: 1, edgeTypes: null });
        setFocus(nodeId);
        if (resp.meta.truncated) {
          message.warning(
            `Showing ${resp.meta.total_nodes} of many neighbors. Filter by edge type for more.`
          );
        }
      } catch (err: unknown) {
        message.error(
          `Expand failed: ${err instanceof Error ? err.message : "unknown"}`
        );
      }
    },
    [activeTenant, addGraphData, recordExpand, setFocus, isExpanded, collapseNode]
  );

  const handleNodeClick = useCallback(
    async (nodeId: string) => {
      selectNode(nodeId);
      setFocus(nodeId);
      const node = useGraphStore.getState().viewGraph.nodes.get(nodeId);
      if (node && !node.hydrated) {
        try {
          const detail = await getVertexDetail(nodeId, activeTenant);
          hydrateNode(nodeId, detail.properties);
        } catch {
          /* best-effort */
        }
      }
    },
    [selectNode, setFocus, activeTenant, hydrateNode]
  );

  const handleEdgeClick = useCallback(
    (edgeId: string) => {
      selectEdge(edgeId);
    },
    [selectEdge]
  );

  // Effect 1: Create/destroy G6 instance when layout or arrow setting changes
  useEffect(() => {
    if (!containerRef.current) return;

    destroyedRef.current = false;
    renderedVersionRef.current = -1;

    if (graphRef.current) {
      try { graphRef.current.destroy(); } catch { /* already destroyed */ }
      graphRef.current = null;
    }

    const graph = new Graph({
      container: containerRef.current,
      autoFit: "view",
      padding: 40,
      animation: false,
      node: {
        type: "circle",
        style: {
          size: (d: Record<string, unknown>) => {
            const data = d.data as Record<string, unknown>;
            const edgeCount = (data?.edgeCount as number) || 1;
            return Math.max(8, Math.min(8 + edgeCount * 2, 32));
          },
          labelText: (d: Record<string, unknown>) =>
            (d.data as Record<string, unknown>)?.label as string || d.id as string,
          labelPlacement: "bottom",
          labelFontSize: 10,
          labelFill: "#374151",
          labelBackground: true,
          labelBackgroundFill: "rgba(255,255,255,0.85)",
          labelBackgroundRadius: 2,
          labelPadding: [1, 4],
          labelOffsetY: 4,
          iconText: (d: Record<string, unknown>) => {
            const nodeType = (d.data as Record<string, unknown>)?.nodeType as string || "";
            return TYPE_ABBREV[nodeType] || "";
          },
          iconFontSize: 10,
          iconFill: "#fff",
          iconFontWeight: "bold",
          stroke: "#fff",
          lineWidth: 2,
        },
      },
      edge: {
        type: "line",
        style: {
          stroke: "#c4c4c4",
          lineWidth: 1,
          endArrow: showEdgeArrows,
          labelText: "",
          cursor: "pointer",
          lineDash: (d: Record<string, unknown>) => {
            const lbl = (d.data as Record<string, unknown>)?.edgeLabel as string || "";
            return lbl === "SIMILAR_TO" ? [4, 4] : undefined;
          },
        },
      },
      layout: layoutConfig(activeLayout, 0),
      behaviors: ["drag-canvas", "zoom-canvas", "drag-element"],
      plugins: [
        { type: "minimap", key: "minimap", size: [120, 80] },
        {
          type: "tooltip",
          key: "edge-tooltip",
          trigger: "hover",
          getContent: (_evt: unknown, items: Array<Record<string, unknown>>) => {
            if (!items?.length) return "";
            const d = items[0]?.data as Record<string, unknown>;
            if (!d?.edgeLabel) return "";
            const w = d.weight ? ` (weight: ${d.weight})` : "";
            return `<div style="padding:4px 8px;font-size:12px;">${d.edgeLabel}${w}</div>`;
          },
        },
      ],
    });

    graph.on("node:click", (evt: any) => {
      handleNodeClick(evt.target?.id ?? evt.itemId);
    });

    graph.on("node:dblclick", (evt: any) => {
      handleToggleExpand(evt.target?.id ?? evt.itemId);
    });

    graph.on("edge:click", (evt: any) => {
      handleEdgeClick(evt.target?.id ?? evt.itemId);
    });

    graphRef.current = graph;
    setGraphReady((n) => n + 1);

    return () => {
      destroyedRef.current = true;
      try { graph.destroy(); } catch { /* safe */ }
      graphRef.current = null;
    };
  }, [activeLayout, showEdgeArrows]);

  // Effect 2: Push data into G6 when graph data actually changes
  useEffect(() => {
    const graph = graphRef.current;
    if (!graph || destroyedRef.current) return;
    if (renderedVersionRef.current === graphVersion && graphReady > 0) return;

    renderedVersionRef.current = graphVersion;

    const { nodes, edges } = buildNodeData(viewGraph);
    const nodeCount = nodes.length;

    if (nodeCount > 1000) {
      nodes.forEach((n: any) => {
        if (n.style) {
          n.style.labelText = "";
          n.style.iconText = "";
        }
        if (n.data) n.data.label = "";
      });
    }

    try {
      graph.setLayout(layoutConfig(activeLayout, nodeCount));
    } catch { /* fallback */ }

    graph.setData({ nodes, edges });
    graph.render().catch(() => {});

    if (focusedNodeId) {
      setTimeout(() => {
        if (destroyedRef.current) return;
        try {
          graph.focusElement(focusedNodeId, true);
        } catch { /* node may not be in view */ }
      }, 400);
    }
  }, [graphVersion, graphReady, viewGraph, focusedNodeId, activeLayout]);

  // Effect 3: Update visual selection styles without re-rendering data
  useEffect(() => {
    const graph = graphRef.current;
    if (!graph || destroyedRef.current) return;
    if (renderedVersionRef.current < 0) return;

    const expandedNodes = useGraphStore.getState().expandedNodes;

    const applyStyles = () => {
      try {
        const nodeUpdates = Array.from(viewGraph.nodes.values()).map((n) => {
          const isSelected = n.id === primarySelectedNode;
          const isExp = expandedNodes.has(n.id);
          return {
            id: n.id,
            style: {
              stroke: isSelected ? "#f59e0b" : isExp ? "#10b981" : "#fff",
              lineWidth: isSelected ? 3 : isExp ? 2.5 : 2,
            },
          };
        });
        if (nodeUpdates.length) graph.updateNodeData(nodeUpdates);

        const edgeUpdates = Array.from(viewGraph.edges.values()).map((e) => {
          const isSelected = e.id === primarySelectedEdge;
          return {
            id: e.id,
            style: {
              stroke: isSelected ? "#f59e0b" : "#c4c4c4",
              lineWidth: isSelected ? 2.5 : 1,
            },
          };
        });
        if (edgeUpdates.length) graph.updateEdgeData(edgeUpdates);

        graph.draw();
      } catch {
        /* graph may be mid-render or nodes not yet loaded */
      }
    };

    setTimeout(applyStyles, 50);
  }, [primarySelectedNode, primarySelectedEdge]);

  return (
    <div
      ref={containerRef}
      style={{ width: "100%", height: "100%", minHeight: 500, background: "#fafbfc" }}
    />
  );
}
