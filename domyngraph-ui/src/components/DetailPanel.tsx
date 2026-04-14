import { Descriptions, Tag, Empty, Typography } from "antd";
import { useGraphStore } from "../store/graphStore";

function PropRow({ label, value }: { label: string; value: string }) {
  return (
    <div style={{ padding: "6px 0", borderBottom: "1px solid #f0f0f0" }}>
      <div style={{ fontSize: 11, color: "#888", marginBottom: 2 }}>{label}</div>
      <div style={{ fontSize: 12, wordBreak: "break-all" }}>{value}</div>
    </div>
  );
}

export default function DetailPanel() {
  const primarySelectedNode = useGraphStore((s) => s.primarySelectedNode);
  const primarySelectedEdge = useGraphStore((s) => s.primarySelectedEdge);
  const viewGraph = useGraphStore((s) => s.viewGraph);

  if (primarySelectedEdge) {
    const edge = viewGraph.edges.get(primarySelectedEdge);
    if (!edge) {
      return (
        <div style={{ padding: 16 }}>
          <Empty description="Edge not in current view" image={Empty.PRESENTED_IMAGE_SIMPLE} />
        </div>
      );
    }

    const sourceNode = viewGraph.nodes.get(edge.source);
    const targetNode = viewGraph.nodes.get(edge.target);

    return (
      <div style={{ padding: 16 }}>
        <Typography.Title level={5} style={{ marginBottom: 12 }}>
          Edge: {edge.label}
        </Typography.Title>
        <Tag color="geekblue">{edge.direction}</Tag>
        <Descriptions column={1} size="small" style={{ marginTop: 12 }} bordered>
          <Descriptions.Item label="Edge ID">
            <Typography.Text copyable style={{ fontSize: 11 }}>{edge.id}</Typography.Text>
          </Descriptions.Item>
          <Descriptions.Item label="Label">{edge.label}</Descriptions.Item>
          <Descriptions.Item label="Source">
            {sourceNode ? `${sourceNode.label} (${sourceNode.type})` : edge.source}
          </Descriptions.Item>
          <Descriptions.Item label="Source ID">{edge.source}</Descriptions.Item>
          <Descriptions.Item label="Target">
            {targetNode ? `${targetNode.label} (${targetNode.type})` : edge.target}
          </Descriptions.Item>
          <Descriptions.Item label="Target ID">{edge.target}</Descriptions.Item>
          {Object.entries(edge.properties).map(([k, v]) => (
            <Descriptions.Item key={k} label={k}>
              {String(v)}
            </Descriptions.Item>
          ))}
        </Descriptions>
      </div>
    );
  }

  if (!primarySelectedNode) {
    return (
      <div style={{ padding: 16 }}>
        <Empty description="Click a node or edge to see details" image={Empty.PRESENTED_IMAGE_SIMPLE} />
      </div>
    );
  }

  const node = viewGraph.nodes.get(primarySelectedNode);
  if (!node) {
    return (
      <div style={{ padding: 16 }}>
        <Empty description="Node not in current view" image={Empty.PRESENTED_IMAGE_SIMPLE} />
      </div>
    );
  }

  const connectedEdges = Array.from(viewGraph.edges.values()).filter(
    (e) => e.source === node.id || e.target === node.id
  );

  return (
    <div style={{ padding: 16 }}>
      <Typography.Title level={5} style={{ marginBottom: 8 }}>
        {node.label}
      </Typography.Title>
      <div style={{ marginBottom: 12 }}>
        <Tag color="blue">{node.type}</Tag>
        {node.hydrated ? <Tag color="green">Hydrated</Tag> : <Tag>Lightweight</Tag>}
      </div>

      <PropRow label="ID" value={node.id} />
      <PropRow label="Edges in view" value={String(connectedEdges.length)} />
      {Object.entries(node.properties).map(([k, v]) => (
        <PropRow key={k} label={k} value={String(v)} />
      ))}

      {connectedEdges.length > 0 && (
        <>
          <Typography.Title level={5} style={{ marginTop: 16, marginBottom: 8 }}>
            Connected Edges
          </Typography.Title>
          {connectedEdges.map((e) => {
            const other = e.source === node.id ? e.target : e.source;
            const otherNode = viewGraph.nodes.get(other);
            const direction = e.source === node.id ? "outgoing" : "incoming";
            return (
              <div
                key={e.id}
                style={{
                  padding: "6px 8px",
                  marginBottom: 4,
                  background: "#f5f5f5",
                  borderRadius: 4,
                  fontSize: 12,
                }}
              >
                <Tag color={direction === "outgoing" ? "blue" : "purple"} style={{ fontSize: 10 }}>
                  {direction}
                </Tag>
                <strong>{e.label}</strong>
                <span style={{ margin: "0 4px" }}>&rarr;</span>
                {otherNode ? otherNode.label : other}
                {e.properties.weight != null && (
                  <span style={{ color: "#888", marginLeft: 8 }}>
                    w: {String(e.properties.weight)}
                  </span>
                )}
              </div>
            );
          })}
        </>
      )}
    </div>
  );
}
