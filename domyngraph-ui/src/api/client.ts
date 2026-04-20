import type { GraphResponse, GraphNode } from "../types/graph";

const API_BASE = import.meta.env.VITE_API_URL || "http://localhost:8080";

async function request<T>(path: string, options?: RequestInit): Promise<T> {
  const res = await fetch(`${API_BASE}${path}`, {
    headers: { "Content-Type": "application/json" },
    ...options,
  });
  if (!res.ok) {
    const body = await res.json().catch(() => ({}));
    throw new Error(body.error || body.detail || `API error ${res.status}`);
  }
  return res.json();
}

export async function expandVertex(
  vertexId: string,
  tenant: string,
  depth = 1,
  limit = 50,
  edgeTypes?: string
): Promise<GraphResponse> {
  const params = new URLSearchParams({
    vertex_id: vertexId,
    tenant,
    depth: String(depth),
    limit: String(limit),
  });
  if (edgeTypes) params.set("edge_types", edgeTypes);
  return request<GraphResponse>(`/api/graph/expand?${params}`);
}

export async function searchGraph(
  q: string,
  tenant: string,
  limit = 20
): Promise<GraphResponse> {
  const params = new URLSearchParams({ q, tenant, limit: String(limit) });
  return request<GraphResponse>(`/api/graph/search?${params}`);
}

export async function getVertexDetail(
  vertexId: string,
  tenant: string
): Promise<GraphNode> {
  const params = new URLSearchParams({ tenant });
  return request<GraphNode>(`/api/graph/vertex/${vertexId}?${params}`);
}

export async function loadInitialGraph(tenant: string, limit?: number): Promise<GraphResponse> {
  const effectiveLimit = limit ?? (tenant === "__ALL__" ? 1000 : 3000);
  return request<GraphResponse>(`/api/graph/overview?tenant=${tenant}&limit=${effectiveLimit}`);
}

export async function listProcedures(): Promise<{ procedures: string[] }> {
  return request("/api/procedures");
}

export async function runProcedure(
  name: string,
  params: Record<string, unknown>,
  tenant: string
) {
  return request("/api/procedures/run", {
    method: "POST",
    body: JSON.stringify({ name, params, tenant }),
  });
}

export async function runAlgorithm(body: {
  algorithm: string;
  params: Record<string, unknown>;
  tenant: string;
  config: Record<string, unknown>;
}): Promise<{ job_id: string; status: string }> {
  return request("/api/algorithms/run", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

export async function getAlgorithmJob(
  jobId: string
): Promise<{
  job_id: string;
  status: string;
  progress: string;
  result: unknown;
  error: string | null;
  elapsed_ms: number;
}> {
  return request(`/api/algorithms/${jobId}`);
}

export async function getSchemaStatus(): Promise<{
  indexes: unknown;
  schema_version: unknown;
}> {
  return request("/api/schema/status");
}

export async function listTenants(): Promise<{
  tenants: { id: string; vertex_count: number; edge_count: number }[];
}> {
  return request("/api/tenants");
}

export async function getHealth(): Promise<Record<string, unknown>> {
  return request("/api/health");
}

export interface GremlinQueryResult {
  result: unknown;
  count: number;
  elapsed_ms: number;
}

export async function executeGremlinQuery(
  query: string,
  timeoutS = 30
): Promise<GremlinQueryResult> {
  return request<GremlinQueryResult>("/api/gremlin/query", {
    method: "POST",
    body: JSON.stringify({ query, timeout_s: timeoutS }),
  });
}
