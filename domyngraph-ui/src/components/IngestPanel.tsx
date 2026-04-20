import { useState, useCallback } from "react";
import {
  Upload,
  Button,
  Steps,
  Typography,
  Space,
  Tag,
  Alert,
  Spin,
  Statistic,
  Card,
  Row,
  Col,
  Input,
} from "antd";
import {
  UploadOutlined,
  BuildOutlined,
  DatabaseOutlined,
  NodeIndexOutlined,
  CheckCircleOutlined,
  LoadingOutlined,
} from "@ant-design/icons";
import type { UploadFile } from "antd";
import {
  uploadDocuments,
  buildKG,
  indexVector,
  indexGraph,
  getIngestStatus,
} from "../api/ragClient";
import { useGraphStore } from "../store/graphStore";

const { Text, Title } = Typography;

export default function IngestPanel() {
  const tenant = useGraphStore((s) => s.activeTenant);
  const effectiveTenant = tenant === "__ALL__" ? "default" : tenant;

  const [step, setStep] = useState(0);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [fileList, setFileList] = useState<UploadFile[]>([]);
  const [entityTypes, setEntityTypes] = useState("Entity, Concept");
  const [stats, setStats] = useState<{
    uploaded?: number;
    triplets?: number;
    chunks?: number;
    vectorIndexed?: number;
    graphIndexed?: number;
  }>({});

  const refreshStatus = useCallback(async () => {
    try {
      const s = await getIngestStatus(effectiveTenant);
      setStats((prev) => ({
        ...prev,
        uploaded: s.uploaded_pdfs,
        triplets: s.triplets,
        chunks: s.chunks,
      }));
    } catch {
      /* ignore */
    }
  }, [effectiveTenant]);

  const handleUpload = async () => {
    if (fileList.length === 0) return;
    setLoading(true);
    setError(null);
    try {
      const files = fileList
        .map((f) => f.originFileObj)
        .filter(Boolean) as File[];
      const res = await uploadDocuments(files, effectiveTenant);
      setStats((prev) => ({ ...prev, uploaded: res.count }));
      setStep(1);
    } catch (e: any) {
      setError(e.message);
    } finally {
      setLoading(false);
    }
  };

  const handleBuildKG = async () => {
    setLoading(true);
    setError(null);
    try {
      const res = await buildKG(effectiveTenant, entityTypes);
      setStats((prev) => ({
        ...prev,
        triplets: res.triplets_count,
        chunks: res.chunks_count,
      }));
      setStep(2);
    } catch (e: any) {
      setError(e.message);
    } finally {
      setLoading(false);
    }
  };

  const handleIndexVector = async () => {
    setLoading(true);
    setError(null);
    try {
      const res = await indexVector(effectiveTenant);
      setStats((prev) => ({
        ...prev,
        vectorIndexed: res.chunks_indexed + res.triplets_indexed,
      }));
      setStep(3);
    } catch (e: any) {
      setError(e.message);
    } finally {
      setLoading(false);
    }
  };

  const handleIndexGraph = async () => {
    setLoading(true);
    setError(null);
    try {
      const res = await indexGraph(effectiveTenant);
      setStats((prev) => ({ ...prev, graphIndexed: res.indexed }));
      setStep(4);
    } catch (e: any) {
      setError(e.message);
    } finally {
      setLoading(false);
    }
  };

  return (
    <div style={{ padding: 16, height: "100%", overflow: "auto" }}>
      <Title level={5} style={{ marginTop: 0 }}>
        Ingest Pipeline
      </Title>
      <Text type="secondary">Tenant: {effectiveTenant}</Text>

      {error && (
        <Alert
          type="error"
          message={error}
          closable
          onClose={() => setError(null)}
          style={{ marginTop: 8 }}
        />
      )}

      <Steps
        current={step}
        size="small"
        direction="vertical"
        style={{ marginTop: 16 }}
        items={[
          { title: "Upload PDFs", icon: <UploadOutlined /> },
          { title: "Build KG", icon: <BuildOutlined /> },
          { title: "Index Vectors", icon: <DatabaseOutlined /> },
          { title: "Index Graph", icon: <NodeIndexOutlined /> },
          { title: "Done", icon: <CheckCircleOutlined /> },
        ]}
      />

      <div style={{ marginTop: 16 }}>
        {step === 0 && (
          <Space direction="vertical" style={{ width: "100%" }}>
            <Upload.Dragger
              multiple
              accept=".pdf"
              fileList={fileList}
              beforeUpload={() => false}
              onChange={({ fileList: fl }) => setFileList(fl)}
            >
              <p>
                <UploadOutlined style={{ fontSize: 24 }} />
              </p>
              <p>Drop PDFs here or click to select</p>
            </Upload.Dragger>
            <Button
              type="primary"
              onClick={handleUpload}
              loading={loading}
              disabled={fileList.length === 0}
              block
            >
              Upload {fileList.length} file(s)
            </Button>
          </Space>
        )}

        {step === 1 && (
          <Space direction="vertical" style={{ width: "100%" }}>
            <Input
              value={entityTypes}
              onChange={(e) => setEntityTypes(e.target.value)}
              placeholder="Entity types (comma-separated)"
              addonBefore="Types"
            />
            <Button
              type="primary"
              onClick={handleBuildKG}
              loading={loading}
              icon={loading ? <LoadingOutlined /> : <BuildOutlined />}
              block
            >
              {loading ? "Extracting triplets..." : "Build Knowledge Graph"}
            </Button>
          </Space>
        )}

        {step === 2 && (
          <Button
            type="primary"
            onClick={handleIndexVector}
            loading={loading}
            icon={<DatabaseOutlined />}
            block
          >
            {loading ? "Indexing vectors..." : "Index into ChromaDB + BM25"}
          </Button>
        )}

        {step === 3 && (
          <Button
            type="primary"
            onClick={handleIndexGraph}
            loading={loading}
            icon={<NodeIndexOutlined />}
            block
          >
            {loading ? "Indexing into JanusGraph..." : "Index into JanusGraph"}
          </Button>
        )}

        {step === 4 && (
          <Alert
            type="success"
            message="Ingest pipeline complete!"
            description="All data has been indexed. You can now use RAG queries."
          />
        )}
      </div>

      {(stats.triplets || stats.chunks || stats.graphIndexed) && (
        <Row gutter={[8, 8]} style={{ marginTop: 16 }}>
          {stats.uploaded != null && (
            <Col span={12}>
              <Card size="small">
                <Statistic title="PDFs" value={stats.uploaded} />
              </Card>
            </Col>
          )}
          {stats.chunks != null && (
            <Col span={12}>
              <Card size="small">
                <Statistic title="Chunks" value={stats.chunks} />
              </Card>
            </Col>
          )}
          {stats.triplets != null && (
            <Col span={12}>
              <Card size="small">
                <Statistic title="Triplets" value={stats.triplets} />
              </Card>
            </Col>
          )}
          {stats.graphIndexed != null && (
            <Col span={12}>
              <Card size="small">
                <Statistic title="Graph" value={stats.graphIndexed} />
              </Card>
            </Col>
          )}
        </Row>
      )}
    </div>
  );
}
