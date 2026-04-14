import { Button, Typography, Spin, Alert } from "antd";
import { useEffect, useState } from "react";
import { getSchemaStatus } from "../api/client";

export default function SchemaPanel() {
  const [loading, setLoading] = useState(true);
  const [data, setData] = useState<{ indexes: unknown; schema_version: unknown } | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await getSchemaStatus();
      setData(result);
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : "Failed to load schema");
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => { load(); }, []);

  if (loading) return <Spin style={{ padding: 24 }} />;
  if (error) return <Alert type="error" message={error} style={{ margin: 16 }} />;

  return (
    <div style={{ padding: 16 }}>
      <Typography.Title level={5}>Schema Status</Typography.Title>
      <Typography.Text strong>Version: </Typography.Text>
      <Typography.Text>{String(data?.schema_version ?? "unknown")}</Typography.Text>
      <Typography.Title level={5} style={{ marginTop: 16 }}>
        Indexes
      </Typography.Title>
      <pre style={{ fontSize: 11, background: "#f5f5f5", padding: 8, borderRadius: 4, maxHeight: 300, overflow: "auto" }}>
        {JSON.stringify(data?.indexes, null, 2)}
      </pre>
      <Button onClick={load} style={{ marginTop: 8 }}>Refresh</Button>
    </div>
  );
}
