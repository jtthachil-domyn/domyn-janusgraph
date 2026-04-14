import { Table, Typography, Spin, Alert, Button, Tag } from "antd";
import { useEffect, useState } from "react";
import { listTenants } from "../api/client";
import { useGraphStore } from "../store/graphStore";

interface TenantInfo {
  id: string;
  vertex_count: number;
  edge_count: number;
}

export default function TenantPanel() {
  const [loading, setLoading] = useState(true);
  const [tenants, setTenants] = useState<TenantInfo[]>([]);
  const [error, setError] = useState<string | null>(null);
  const activeTenant = useGraphStore((s) => s.activeTenant);
  const setTenant = useGraphStore((s) => s.setTenant);

  const load = async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await listTenants();
      setTenants(result.tenants);
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : "Failed to load tenants");
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => { load(); }, []);

  if (loading) return <Spin style={{ padding: 24 }} />;
  if (error) return <Alert type="error" message={error} style={{ margin: 16 }} />;

  return (
    <div style={{ padding: 16 }}>
      <Typography.Title level={5}>Tenants</Typography.Title>
      <Table
        dataSource={tenants}
        rowKey="id"
        size="small"
        pagination={false}
        onRow={(record) => ({
          onClick: () => setTenant(record.id),
          style: { cursor: "pointer" },
        })}
        columns={[
          {
            title: "Tenant",
            dataIndex: "id",
            render: (id: string) => (
              <span>
                {id} {id === activeTenant && <Tag color="green">Active</Tag>}
              </span>
            ),
          },
          { title: "Vertices", dataIndex: "vertex_count" },
          { title: "Edges", dataIndex: "edge_count" },
        ]}
      />
      <Button onClick={load} style={{ marginTop: 8 }}>Refresh</Button>
    </div>
  );
}
