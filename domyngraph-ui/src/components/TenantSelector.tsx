import { Select } from "antd";
import { useGraphStore } from "../store/graphStore";
import { listTenants } from "../api/client";
import { useEffect, useState } from "react";

export default function TenantSelector() {
  const activeTenant = useGraphStore((s) => s.activeTenant);
  const setTenant = useGraphStore((s) => s.setTenant);
  const [tenants, setTenants] = useState<string[]>(["demo"]);

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
      style={{ width: 140 }}
      options={tenants.map((t) => ({ label: `Tenant: ${t}`, value: t }))}
    />
  );
}
