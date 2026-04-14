import { Typography, Spin, Alert, Button, Descriptions, Tag } from "antd";
import { useEffect, useState } from "react";
import { getHealth } from "../api/client";

export default function HealthPanel() {
  const [loading, setLoading] = useState(true);
  const [data, setData] = useState<Record<string, unknown> | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await getHealth();
      setData(result);
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : "Health check failed");
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => { load(); }, []);

  if (loading) return <Spin style={{ padding: 24 }} />;
  if (error) return <Alert type="error" message={error} style={{ margin: 16 }} />;

  return (
    <div style={{ padding: 16 }}>
      <Typography.Title level={5}>System Health</Typography.Title>
      <Descriptions column={1} size="small">
        <Descriptions.Item label="API">
          <Tag color={data?.api === "ok" ? "green" : "red"}>{String(data?.api)}</Tag>
        </Descriptions.Item>
        <Descriptions.Item label="Gremlin">
          <Tag color={data?.gremlin === "ok" ? "green" : "red"}>{String(data?.gremlin)}</Tag>
        </Descriptions.Item>
        <Descriptions.Item label="Vertex Count">
          {String(data?.vertex_count ?? "N/A")}
        </Descriptions.Item>
        <Descriptions.Item label="Cache">
          <pre style={{ fontSize: 11, margin: 0 }}>
            {JSON.stringify(data?.cache, null, 2)}
          </pre>
        </Descriptions.Item>
      </Descriptions>
      <Button onClick={load} style={{ marginTop: 8 }}>Refresh</Button>
    </div>
  );
}
