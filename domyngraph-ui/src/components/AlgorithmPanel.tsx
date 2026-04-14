import { Button, Select, InputNumber, Space, Typography, Alert, Progress, Descriptions, Tag, Table } from "antd";
import { useState, useRef, useCallback } from "react";
import { useGraphStore } from "../store/graphStore";
import { runAlgorithm, getAlgorithmJob } from "../api/client";

interface AlgoResult {
  algorithm?: string;
  iterations?: number;
  converged?: boolean;
  executionTimeMs?: number;
  verticesProcessed?: number;
  [key: string]: unknown;
}

export default function AlgorithmPanel() {
  const activeTenant = useGraphStore((s) => s.activeTenant);
  const focusedNodeId = useGraphStore((s) => s.focusedNodeId);
  const [algorithm, setAlgorithm] = useState("pagerank");
  const [maxIter, setMaxIter] = useState(20);
  const [timeout, setTimeoutMs] = useState(60000);
  const [running, setRunning] = useState(false);
  const [status, setStatus] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<AlgoResult | null>(null);
  const [elapsedMs, setElapsedMs] = useState(0);
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const algoDescriptions: Record<string, string> = {
    bfs: "Explores the graph level-by-level from a seed node. Computes the shortest distance (in hops) from the seed to every reachable vertex. Useful for neighborhood analysis.",
    pagerank: "Ranks every vertex by importance based on incoming connections. Vertices linked from many important vertices score higher. Useful for finding influential nodes.",
    connected_components: "Finds clusters of vertices that are connected to each other. Each cluster gets a component ID. Useful for identifying isolated groups.",
  };

  const handleRun = useCallback(async () => {
    setRunning(true);
    setError(null);
    setResult(null);
    setElapsedMs(0);
    setStatus("submitting");

    try {
      const params: Record<string, unknown> = {};
      if (algorithm === "bfs") {
        if (!focusedNodeId) {
          setError("Click a node first to set it as the BFS seed node");
          setRunning(false);
          return;
        }
        params.vertex_id = focusedNodeId;
      }

      const { job_id } = await runAlgorithm({
        algorithm,
        params,
        tenant: activeTenant,
        config: { timeout_ms: timeout, max_iterations: maxIter },
      });

      setStatus("running");

      pollRef.current = setInterval(async () => {
        try {
          const job = await getAlgorithmJob(job_id);
          setStatus(job.status);
          setElapsedMs(job.elapsed_ms);

          if (job.status === "completed") {
            setResult(job.result as AlgoResult);
            setRunning(false);
            if (pollRef.current) clearInterval(pollRef.current);
          } else if (job.status === "failed" || job.status === "timeout") {
            setError(job.error || "Algorithm failed");
            setRunning(false);
            if (pollRef.current) clearInterval(pollRef.current);
          }
        } catch {
          setError("Failed to poll job status");
          setRunning(false);
          if (pollRef.current) clearInterval(pollRef.current);
        }
      }, 2000);
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : "Failed to start algorithm");
      setRunning(false);
    }
  }, [algorithm, activeTenant, focusedNodeId, maxIter, timeout]);

  const formatResult = (res: AlgoResult) => {
    const known = ["algorithm", "iterations", "converged", "executionTimeMs", "verticesProcessed"];
    const extra = Object.entries(res).filter(([k]) => !known.includes(k));

    return (
      <div>
        <Descriptions column={1} size="small" bordered style={{ marginBottom: 8 }}>
          {res.algorithm && (
            <Descriptions.Item label="Algorithm">
              <Tag color="purple">{String(res.algorithm)}</Tag>
            </Descriptions.Item>
          )}
          {res.verticesProcessed != null && (
            <Descriptions.Item label="Vertices Processed">
              {String(res.verticesProcessed)}
            </Descriptions.Item>
          )}
          {res.iterations != null && (
            <Descriptions.Item label="Iterations">{String(res.iterations)}</Descriptions.Item>
          )}
          {res.converged != null && (
            <Descriptions.Item label="Converged">
              <Tag color={res.converged ? "green" : "orange"}>{res.converged ? "Yes" : "No"}</Tag>
            </Descriptions.Item>
          )}
          {res.executionTimeMs != null && (
            <Descriptions.Item label="Execution Time">
              {String(res.executionTimeMs)}ms
            </Descriptions.Item>
          )}
        </Descriptions>
        {extra.length > 0 && (
          <Table
            dataSource={extra.map(([k, v]) => ({ key: k, property: k, value: String(v) }))}
            columns={[
              { title: "Property", dataIndex: "property", width: 140 },
              { title: "Value", dataIndex: "value" },
            ]}
            size="small"
            pagination={false}
            style={{ fontSize: 11 }}
          />
        )}
      </div>
    );
  };

  return (
    <div style={{ padding: 16 }}>
      <Typography.Title level={5}>Algorithm Runner</Typography.Title>
      <Space direction="vertical" style={{ width: "100%" }} size="middle">
        <Select
          value={algorithm}
          onChange={setAlgorithm}
          style={{ width: "100%" }}
          options={[
            { label: "PageRank", value: "pagerank" },
            { label: "BFS (Breadth-First Search)", value: "bfs" },
            { label: "Connected Components", value: "connected_components" },
          ]}
        />
        <Typography.Paragraph
          type="secondary"
          style={{ fontSize: 12, margin: 0 }}
        >
          {algoDescriptions[algorithm]}
        </Typography.Paragraph>

        {algorithm === "bfs" && (
          <Alert
            type={focusedNodeId ? "info" : "warning"}
            message={
              focusedNodeId
                ? `Seed node: ${focusedNodeId} (click a different node to change)`
                : "Click a node on the canvas to set it as the BFS seed"
            }
            showIcon
          />
        )}

        <Space>
          <InputNumber
            size="small"
            addonBefore="Iterations"
            value={maxIter}
            onChange={(v) => v && setMaxIter(v)}
            min={1}
            max={100}
            style={{ width: 150 }}
          />
          <InputNumber
            size="small"
            addonBefore="Timeout"
            addonAfter="ms"
            value={timeout}
            onChange={(v) => v && setTimeoutMs(v)}
            min={5000}
            max={300000}
            step={5000}
            style={{ width: 190 }}
          />
        </Space>

        <Button type="primary" onClick={handleRun} loading={running} block>
          Run {algorithm === "bfs" ? "BFS" : algorithm === "pagerank" ? "PageRank" : "Connected Components"}
        </Button>

        {running && (
          <div>
            <Progress percent={-1} status="active" showInfo={false} />
            <Typography.Text type="secondary" style={{ fontSize: 11 }}>
              Status: {status} {elapsedMs > 0 && `(${elapsedMs}ms)`}
            </Typography.Text>
          </div>
        )}

        {error && <Alert type="error" message={error} showIcon closable />}

        {status === "completed" && result && (
          <div>
            <Alert
              type="success"
              message={`${algorithm.replace("_", " ")} completed in ${elapsedMs}ms`}
              showIcon
              style={{ marginBottom: 8 }}
            />
            {formatResult(result)}
          </div>
        )}
      </Space>
    </div>
  );
}
