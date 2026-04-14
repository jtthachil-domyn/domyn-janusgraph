import { Input, message } from "antd";
import { SearchOutlined } from "@ant-design/icons";
import { useGraphStore } from "../store/graphStore";
import { searchGraph } from "../api/client";
import { useState } from "react";

export default function SearchBar() {
  const [loading, setLoading] = useState(false);
  const activeTenant = useGraphStore((s) => s.activeTenant);
  const addGraphData = useGraphStore((s) => s.addGraphData);
  const setFocus = useGraphStore((s) => s.setFocus);

  const handleSearch = async (value: string) => {
    if (value.length < 2) return;
    setLoading(true);
    try {
      const resp = await searchGraph(value, activeTenant);
      if (resp.nodes.length === 0) {
        message.info("No results found");
        return;
      }
      addGraphData(resp);
      setFocus(resp.nodes[0].id);
      message.success(`Found ${resp.nodes.length} nodes`);
    } catch (err: unknown) {
      message.error(
        `Search failed: ${err instanceof Error ? err.message : "unknown"}`
      );
    } finally {
      setLoading(false);
    }
  };

  return (
    <Input.Search
      placeholder="Search graph..."
      prefix={<SearchOutlined />}
      allowClear
      loading={loading}
      onSearch={handleSearch}
      style={{ width: 280 }}
      size="small"
    />
  );
}
