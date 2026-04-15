import { Select } from "antd";
import { useGraphStore } from "../store/graphStore";
import { listTenants } from "../api/client";
import { useEffect, useState } from "react";

const ALL_TENANT = "__ALL__";

export default function TenantSelector() {
  const activeTenant = useGraphStore((s) => s.activeTenant);
  const setTenant = useGraphStore((s) => s.setTenant);
  const [tenants, setTenants] = useState<string[]>([ALL_TENANT]);

  useEffect(() => {
    listTenants()
      .then((resp) => {
        const ids = resp.tenants.map((t) => t.id);
        if (ids.length > 0) setTenants(ids);
      })
      .catch(() => {});
  }, []);

  return (
    <Select
      size="small"
      value={activeTenant}
      onChange={setTenant}
      style={{ width: 160 }}
      options={tenants.map((t) => ({
        label: t === ALL_TENANT ? "All Tenants" : t,
        value: t,
      }))}
    />
  );
}
