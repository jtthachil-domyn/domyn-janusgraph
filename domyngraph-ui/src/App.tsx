import { useEffect } from "react";
import { Layout, Tabs, Typography, Space, theme } from "antd";
import {
  NodeIndexOutlined,
  ExperimentOutlined,
  DatabaseOutlined,
  TeamOutlined,
  HeartOutlined,
  ThunderboltOutlined,
} from "@ant-design/icons";
import GraphCanvas from "./components/GraphCanvas";
import Toolbar from "./components/Toolbar";
import SearchBar from "./components/SearchBar";
import TenantSelector from "./components/TenantSelector";
import DetailPanel from "./components/DetailPanel";
import AlgorithmPanel from "./components/AlgorithmPanel";
import ProcedurePanel from "./components/ProcedurePanel";
import SchemaPanel from "./components/SchemaPanel";
import TenantPanel from "./components/TenantPanel";
import HealthPanel from "./components/HealthPanel";
import ContextMenu from "./components/ContextMenu";
import { useGraphStore } from "./store/graphStore";
import { loadInitialGraph } from "./api/client";

const { Header, Sider, Content } = Layout;

export default function App() {
  const { token } = theme.useToken();
  const addGraphData = useGraphStore((s) => s.addGraphData);
  const activeTenant = useGraphStore((s) => s.activeTenant);
  const setFocus = useGraphStore((s) => s.setFocus);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const resp = await loadInitialGraph(activeTenant);
        if (!cancelled && resp.nodes.length > 0) {
          addGraphData(resp);
          setFocus(resp.nodes[0].id);
        }
      } catch (err) {
        console.warn("Initial graph load failed:", err);
      }
    })();
    return () => { cancelled = true; };
  }, [activeTenant, addGraphData, setFocus]);

  return (
    <Layout style={{ height: "100vh", overflow: "hidden" }}>
      <Header
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          background: "#141414",
          padding: "0 20px",
          height: 48,
        }}
      >
        <Space>
          <NodeIndexOutlined style={{ color: "#a78bfa", fontSize: 20 }} />
          <Typography.Text
            strong
            style={{ color: "#fff", fontSize: 15, letterSpacing: 0.5 }}
          >
            DomynGraph Lens
          </Typography.Text>
        </Space>
        <Space size="middle">
          <SearchBar />
          <TenantSelector />
        </Space>
      </Header>

      <Layout>
        <Content
          style={{
            display: "flex",
            flexDirection: "column",
            overflow: "hidden",
          }}
        >
          <Toolbar />
          <div style={{ flex: 1, position: "relative" }}>
            <GraphCanvas />
            <ContextMenu />
          </div>
        </Content>

        <Sider
          width={340}
          style={{
            background: token.colorBgContainer,
            borderLeft: `1px solid ${token.colorBorderSecondary}`,
            overflow: "auto",
          }}
        >
          <Tabs
            defaultActiveKey="detail"
            size="small"
            style={{ height: "100%" }}
            tabBarStyle={{ padding: "0 12px", margin: 0 }}
            items={[
              {
                key: "detail",
                label: "Detail",
                icon: <NodeIndexOutlined />,
                children: <DetailPanel />,
              },
              {
                key: "procedures",
                label: "Procedures",
                icon: <ThunderboltOutlined />,
                children: <ProcedurePanel />,
              },
              {
                key: "algorithms",
                label: "Algorithms",
                icon: <ExperimentOutlined />,
                children: <AlgorithmPanel />,
              },
              {
                key: "schema",
                label: "Schema",
                icon: <DatabaseOutlined />,
                children: <SchemaPanel />,
              },
              {
                key: "tenants",
                label: "Tenants",
                icon: <TeamOutlined />,
                children: <TenantPanel />,
              },
              {
                key: "health",
                label: "Health",
                icon: <HeartOutlined />,
                children: <HealthPanel />,
              },
            ]}
          />
        </Sider>
      </Layout>
    </Layout>
  );
}
