import {
  ReloadOutlined,
  BranchesOutlined,
  ApartmentOutlined,
  RadarChartOutlined,
  SwapOutlined,
  DeleteOutlined,
} from "@ant-design/icons";
import { Button, Space, Tooltip, Select } from "antd";
import { useGraphStore } from "../store/graphStore";

export default function Toolbar() {
  const activeLayout = useGraphStore((s) => s.activeLayout);
  const setLayout = useGraphStore((s) => s.setLayout);
  const showEdgeArrows = useGraphStore((s) => s.showEdgeArrows);
  const toggleEdgeArrows = useGraphStore((s) => s.toggleEdgeArrows);
  const resetView = useGraphStore((s) => s.resetView);
  const resetWorkspace = useGraphStore((s) => s.resetWorkspace);
  const activePerspective = useGraphStore((s) => s.activePerspective);
  const perspectives = useGraphStore((s) => s.perspectives);
  const setPerspective = useGraphStore((s) => s.setPerspective);

  return (
    <Space size="small" wrap style={{ padding: "8px 12px", background: "#fafafa", borderBottom: "1px solid #e5e7eb" }}>
      <Tooltip title="Force Layout">
        <Button
          type={activeLayout === "force" ? "primary" : "default"}
          icon={<BranchesOutlined />}
          size="small"
          onClick={() => setLayout("force")}
        />
      </Tooltip>
      <Tooltip title="Dagre Layout">
        <Button
          type={activeLayout === "dagre" ? "primary" : "default"}
          icon={<ApartmentOutlined />}
          size="small"
          onClick={() => setLayout("dagre")}
        />
      </Tooltip>
      <Tooltip title="Radial Layout">
        <Button
          type={activeLayout === "radial" ? "primary" : "default"}
          icon={<RadarChartOutlined />}
          size="small"
          onClick={() => setLayout("radial")}
        />
      </Tooltip>

      <span style={{ borderLeft: "1px solid #d9d9d9", height: 20, margin: "0 4px" }} />

      <Tooltip title={showEdgeArrows ? "Hide Arrows" : "Show Arrows"}>
        <Button
          type={showEdgeArrows ? "primary" : "default"}
          icon={<SwapOutlined />}
          size="small"
          onClick={toggleEdgeArrows}
        />
      </Tooltip>

      <span style={{ borderLeft: "1px solid #d9d9d9", height: 20, margin: "0 4px" }} />

      <Select
        size="small"
        value={activePerspective}
        onChange={setPerspective}
        style={{ width: 160 }}
        options={perspectives.map((p) => ({ label: p.name, value: p.name }))}
      />

      <span style={{ borderLeft: "1px solid #d9d9d9", height: 20, margin: "0 4px" }} />

      <Tooltip title="Reset View">
        <Button icon={<ReloadOutlined />} size="small" onClick={resetView} />
      </Tooltip>
      <Tooltip title="Clear All">
        <Button
          icon={<DeleteOutlined />}
          size="small"
          danger
          onClick={resetWorkspace}
        />
      </Tooltip>
    </Space>
  );
}
