import { useState } from "react";
import { Button, Input, Space, Tag, Select, message } from "antd";
import { PlayCircleOutlined, ClearOutlined } from "@ant-design/icons";
import { executeGremlinQuery } from "../api/client";
import { useGraphStore } from "../store/graphStore";

const { TextArea } = Input;

const SAMPLE_QUERIES: { label: string; query: string; description: string }[] = [
  {
    label: "Count all vertices",
    query: "g.V().count()",
    description: "Total vertex count across all tenants",
  },
  {
    label: "Count all edges",
    query: "g.E().count()",
    description: "Total edge count across all tenants",
  },
  {
    label: "List all tenants",
    query: "g.V().values('tenant_id').dedup().toList()",
    description: "Unique tenant IDs (tickers) in the graph",
  },
  {
    label: "AAPL: First 20 entities",
    query: "g.V().has('tenant_id','AAPL').limit(20).elementMap().toList()",
    description: "Browse first 20 vertices for Apple",
  },
  {
    label: "AAPL: Entity types breakdown",
    query: "g.V().has('tenant_id','AAPL').groupCount().by('entity_type')",
    description: "How many ORG, FIN_METRIC, RISK_FACTOR, etc.",
  },
  {
    label: "AAPL: Top 10 by degree (most connected)",
    query: "g.V().has('tenant_id','AAPL').project('name','degree').by('name').by(bothE().count()).order().by(select('degree'),desc).limit(10).toList()",
    description: "Highest-connected entities for Apple",
  },
  {
    label: "AAPL: 1-hop from Apple Inc.",
    query: "g.V().has('external_id','AAPL:Apple Inc.:ORG').both().dedup().limit(30).elementMap().toList()",
    description: "All direct neighbors of Apple Inc. entity",
  },
  {
    label: "AAPL: 2-hop bounded traversal",
    query: "g.V().has('external_id','AAPL:Apple Inc.:ORG').out('Discloses').out().dedup().limit(30).elementMap().toList()",
    description: "Two hops from Apple Inc. via Discloses edges",
  },
  {
    label: "AAPL: Point lookup by external_id",
    query: "g.V().has('external_id','AAPL:Revenue:FIN_METRIC').elementMap().toList()",
    description: "Retrieve a specific vertex by its external_id",
  },
  {
    label: "Edge label distribution (sample)",
    query: "g.E().limit(10000).label().groupCount()",
    description: "Distribution of relationship types across 10K edges",
  },
  {
    label: "NVDA: All risk factors",
    query: "g.V().has('tenant_id','NVDA').has('entity_type','RISK_FACTOR').limit(20).elementMap().toList()",
    description: "Risk factor entities for NVIDIA",
  },
  {
    label: "MSFT: Financial metrics",
    query: "g.V().has('tenant_id','MSFT').has('entity_type','FIN_METRIC').limit(20).elementMap().toList()",
    description: "Financial metric entities for Microsoft",
  },
];

interface HistoryEntry {
  query: string;
  elapsed_ms: number;
  count: number;
  timestamp: number;
}

export default function QueryConsole() {
  const [query, setQuery] = useState("");
  const [result, setResult] = useState<unknown>(null);
  const [elapsed, setElapsed] = useState<number | null>(null);
  const [count, setCount] = useState<number | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [history, setHistory] = useState<HistoryEntry[]>([]);
  const activeTenant = useGraphStore((s) => s.activeTenant);

  const handleRun = async () => {
    if (!query.trim()) return;
    setLoading(true);
    setError(null);
    setResult(null);
    setElapsed(null);
    setCount(null);
    try {
      const resp = await executeGremlinQuery(query.trim());
      setResult(resp.result);
      setElapsed(resp.elapsed_ms);
      setCount(resp.count);
      setHistory((prev) => [
        { query: query.trim(), elapsed_ms: resp.elapsed_ms, count: resp.count, timestamp: Date.now() },
        ...prev.slice(0, 19),
      ]);
    } catch (err: any) {
      const detail = err?.message || "Query failed";
      setError(detail);
      message.error(detail.slice(0, 120));
    } finally {
      setLoading(false);
    }
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
      e.preventDefault();
      handleRun();
    }
  };

  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%", gap: 8 }}>
      <div style={{ display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap" }}>
        <Select
          placeholder="Sample queries..."
          style={{ flex: 1, minWidth: 200 }}
          onChange={(val) => setQuery(val)}
          options={SAMPLE_QUERIES.map((sq) => ({
            label: `${sq.label} — ${sq.description}`,
            value: sq.query,
          }))}
          showSearch
          optionFilterProp="label"
          allowClear
          size="small"
        />
        {activeTenant && (
          <Tag color="blue" style={{ margin: 0 }}>tenant: {activeTenant}</Tag>
        )}
      </div>

      <TextArea
        value={query}
        onChange={(e) => setQuery(e.target.value)}
        onKeyDown={handleKeyDown}
        placeholder="Enter Gremlin query... (Cmd+Enter to run)"
        autoSize={{ minRows: 3, maxRows: 8 }}
        style={{ fontFamily: "monospace", fontSize: 12 }}
      />

      <Space>
        <Button
          type="primary"
          icon={<PlayCircleOutlined />}
          loading={loading}
          onClick={handleRun}
          disabled={!query.trim()}
          size="small"
        >
          Run
        </Button>
        <Button
          icon={<ClearOutlined />}
          onClick={() => { setQuery(""); setResult(null); setError(null); setElapsed(null); setCount(null); }}
          size="small"
        >
          Clear
        </Button>
        {elapsed !== null && (
          <Tag>{elapsed}ms</Tag>
        )}
        {count !== null && (
          <Tag color="green">{count} result{count !== 1 ? "s" : ""}</Tag>
        )}
      </Space>

      {error && (
        <pre
          style={{
            fontSize: 11,
            fontFamily: "monospace",
            background: "#fff2f0",
            border: "1px solid #ffccc7",
            padding: 8,
            borderRadius: 4,
            whiteSpace: "pre-wrap",
            wordBreak: "break-word",
            maxHeight: 150,
            overflow: "auto",
            margin: 0,
          }}
        >
          {error}
        </pre>
      )}

      {result !== null && (
        <pre
          style={{
            fontSize: 11,
            fontFamily: "monospace",
            flex: 1,
            background: "#fafafa",
            border: "1px solid #d9d9d9",
            padding: 8,
            borderRadius: 4,
            whiteSpace: "pre-wrap",
            wordBreak: "break-word",
            overflow: "auto",
            margin: 0,
            minHeight: 100,
          }}
        >
          {JSON.stringify(result, null, 2)}
        </pre>
      )}

      {history.length > 0 && (
        <div style={{ marginTop: 4 }}>
          <div style={{ fontSize: 11, color: "#999", marginBottom: 4 }}>Recent queries</div>
          <div style={{ display: "flex", flexDirection: "column", gap: 2, maxHeight: 120, overflow: "auto" }}>
            {history.map((h, i) => (
              <div
                key={i}
                onClick={() => setQuery(h.query)}
                style={{
                  fontSize: 11,
                  fontFamily: "monospace",
                  cursor: "pointer",
                  padding: "2px 6px",
                  borderRadius: 3,
                  background: "#f5f5f5",
                  display: "flex",
                  justifyContent: "space-between",
                  gap: 8,
                }}
              >
                <span style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                  {h.query}
                </span>
                <span style={{ color: "#999", whiteSpace: "nowrap" }}>{h.elapsed_ms}ms</span>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
