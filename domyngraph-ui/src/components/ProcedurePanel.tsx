import { Button, Select, Input, Space, Typography, Alert, Spin, Descriptions } from "antd";
import { useState, useEffect, useCallback } from "react";
import { useGraphStore } from "../store/graphStore";
import { listProcedures, runProcedure } from "../api/client";

const PROC_HELP: Record<string, { description: string; example: string }> = {
  kHop: {
    description: "Traverse k hops from a vertex by name, returning all paths found.",
    example: '{"name": "NVIDIA", "hops": 2}',
  },
  getByExternalId: {
    description: "Look up a vertex by its external UUID (the external_id property).",
    example: '{"externalId": "demo-nvidia"}',
  },
  indexStatus: {
    description: "Returns the status of all graph indexes. No parameters needed.",
    example: "{}",
  },
};

export default function ProcedurePanel() {
  const activeTenant = useGraphStore((s) => s.activeTenant);
  const [procedures, setProcedures] = useState<string[]>([]);
  const [loading, setLoading] = useState(true);
  const [selected, setSelected] = useState<string | null>(null);
  const [params, setParams] = useState<string>("");
  const [running, setRunning] = useState(false);
  const [result, setResult] = useState<Record<string, any> | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [elapsed, setElapsed] = useState<number>(0);

  useEffect(() => {
    listProcedures()
      .then((resp) => setProcedures(resp.procedures))
      .catch(() => setProcedures([]))
      .finally(() => setLoading(false));
  }, []);

  const handleSelect = useCallback((name: string) => {
    setSelected(name);
    setResult(null);
    setError(null);
    const help = PROC_HELP[name];
    if (help) {
      setParams(help.example);
    } else {
      setParams("");
    }
  }, []);

  const handleRun = useCallback(async () => {
    if (!selected) return;
    setRunning(true);
    setError(null);
    setResult(null);
    const start = performance.now();
    try {
      let parsedParams: Record<string, unknown> = {};
      if (params.trim()) {
        try {
          parsedParams = JSON.parse(params);
        } catch {
          setError("Params must be valid JSON");
          setRunning(false);
          return;
        }
      }
      const resp = await runProcedure(selected, parsedParams, activeTenant);
      setElapsed(Math.round(performance.now() - start));
      setResult(resp);
    } catch (err: unknown) {
      setElapsed(Math.round(performance.now() - start));
      setError(err instanceof Error ? err.message : "Procedure failed");
    } finally {
      setRunning(false);
    }
  }, [selected, params, activeTenant]);

  if (loading) return <Spin style={{ padding: 24 }} />;

  const help = selected ? PROC_HELP[selected] : null;

  return (
    <div style={{ padding: 16 }}>
      <Typography.Title level={5}>Procedure Runner</Typography.Title>
      <Space direction="vertical" style={{ width: "100%" }} size="middle">
        <Select
          placeholder="Select procedure"
          value={selected}
          onChange={handleSelect}
          style={{ width: "100%" }}
          options={procedures.map((p) => ({ label: p, value: p }))}
          showSearch
        />

        {help && (
          <Typography.Paragraph type="secondary" style={{ fontSize: 12, margin: 0 }}>
            {help.description}
          </Typography.Paragraph>
        )}

        <Input.TextArea
          placeholder='Parameters (JSON): {"key": "value"}'
          value={params}
          onChange={(e) => setParams(e.target.value)}
          rows={3}
          style={{ fontFamily: "monospace", fontSize: 12 }}
        />
        <Button
          type="primary"
          onClick={handleRun}
          loading={running}
          disabled={!selected}
          block
        >
          Run {selected || "Procedure"}
        </Button>

        {error && <Alert type="error" message={error} showIcon />}

        {result && (
          <>
            <Descriptions size="small" column={2}>
              <Descriptions.Item label="Procedure">{selected}</Descriptions.Item>
              <Descriptions.Item label="Time">{elapsed}ms</Descriptions.Item>
            </Descriptions>
            <pre
              style={{
                fontSize: 11,
                maxHeight: 300,
                overflow: "auto",
                background: "#f5f5f5",
                padding: 8,
                borderRadius: 4,
                whiteSpace: "pre-wrap",
                wordBreak: "break-word",
              }}
            >
              {JSON.stringify(result, null, 2)}
            </pre>
          </>
        )}
      </Space>
    </div>
  );
}
