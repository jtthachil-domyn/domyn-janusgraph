/**
 * API client for DomynGraph RAG + Ingest services.
 */

const INGEST_BASE = import.meta.env.VITE_INGEST_URL || "http://localhost:8081";
const RAG_BASE = import.meta.env.VITE_RAG_URL || "http://localhost:8083";

/* ------------------------------------------------------------------ */
/* Ingest Service                                                      */
/* ------------------------------------------------------------------ */

export async function uploadDocuments(
  files: File[],
  tenantId = "default",
): Promise<{ uploaded: { filename: string; path: string }[]; count: number }> {
  const form = new FormData();
  files.forEach((f) => form.append("files", f));
  form.append("tenant_id", tenantId);

  const res = await fetch(`${INGEST_BASE}/api/v1/upload`, {
    method: "POST",
    body: form,
  });
  if (!res.ok) throw new Error(`Upload failed: ${res.status}`);
  return res.json();
}

export async function buildKG(
  tenantId = "default",
  entityTypes?: string,
): Promise<{ triplets_count: number; chunks_count: number; output_dir: string }> {
  const res = await fetch(`${INGEST_BASE}/api/v1/kg/build`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ tenant_id: tenantId, entity_types: entityTypes }),
  });
  if (!res.ok) throw new Error(`KG build failed: ${res.status}`);
  return res.json();
}

export async function indexVector(tenantId = "default"): Promise<{ chunks_indexed: number; triplets_indexed: number }> {
  const res = await fetch(`${INGEST_BASE}/api/v1/index/vector`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ tenant_id: tenantId }),
  });
  if (!res.ok) throw new Error(`Vector index failed: ${res.status}`);
  return res.json();
}

export async function indexGraph(tenantId = "default"): Promise<{ indexed: number; errors: number; total: number }> {
  const res = await fetch(`${INGEST_BASE}/api/v1/index/graph`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ tenant_id: tenantId }),
  });
  if (!res.ok) throw new Error(`Graph index failed: ${res.status}`);
  return res.json();
}

export async function getIngestStatus(tenantId: string): Promise<{
  tenant_id: string;
  uploaded_pdfs: number;
  pdf_files: string[];
  triplets: number;
  chunks: number;
}> {
  const res = await fetch(`${INGEST_BASE}/api/v1/status/${tenantId}`);
  if (!res.ok) throw new Error(`Status check failed: ${res.status}`);
  return res.json();
}

export async function generateSchema(text: string): Promise<{ schema: Record<string, unknown> }> {
  const res = await fetch(`${INGEST_BASE}/api/v1/schema/generate`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(text),
  });
  if (!res.ok) throw new Error(`Schema generation failed: ${res.status}`);
  return res.json();
}

/* ------------------------------------------------------------------ */
/* RAG Service (SSE streaming)                                         */
/* ------------------------------------------------------------------ */

export interface RAGEvent {
  event: string;
  data: string;
}

export type RAGMode = "graph" | "vector" | "hybrid";

export async function* streamRAGQuery(
  query: string,
  mode: RAGMode = "graph",
  options: {
    n_chunks?: number;
    n_triplets?: number;
    business_context?: Record<string, string>;
  } = {},
): AsyncGenerator<RAGEvent> {
  const endpoint =
    mode === "graph"
      ? "/api/v1/query/graph"
      : mode === "vector"
        ? "/api/v1/query/vector"
        : "/api/v1/query/hybrid";

  const body: Record<string, unknown> = { query };
  if (options.n_chunks) body.n_chunks = options.n_chunks;
  if (options.n_triplets) body.n_triplets = options.n_triplets;
  if (options.business_context) body.business_context = options.business_context;

  const res = await fetch(`${RAG_BASE}${endpoint}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });

  if (!res.ok) throw new Error(`RAG query failed: ${res.status}`);
  if (!res.body) throw new Error("No response body");

  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;

    buffer += decoder.decode(value, { stream: true });

    const lines = buffer.split("\n");
    buffer = lines.pop() || "";

    for (const line of lines) {
      const trimmed = line.trim();
      if (trimmed.startsWith("data: ")) {
        try {
          const parsed: RAGEvent = JSON.parse(trimmed.slice(6));
          yield parsed;
        } catch {
          // skip malformed events
        }
      }
    }
  }
}
