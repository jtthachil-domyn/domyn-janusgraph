import { useState, useRef, useEffect } from "react";
import {
  Input,
  Button,
  Typography,
  Space,
  Tag,
  Segmented,
  Collapse,
  Spin,
} from "antd";
import {
  SendOutlined,
  ThunderboltOutlined,
  NodeIndexOutlined,
  FileTextOutlined,
  MergeCellsOutlined,
} from "@ant-design/icons";
import { streamRAGQuery, type RAGEvent, type RAGMode } from "../api/ragClient";

const { Text, Paragraph } = Typography;

interface Message {
  role: "user" | "assistant" | "system";
  content: string;
  events?: RAGEvent[];
  gremlinQuery?: string;
  mode?: RAGMode;
}

export default function RAGPanel() {
  const [messages, setMessages] = useState<Message[]>([]);
  const [input, setInput] = useState("");
  const [mode, setMode] = useState<RAGMode>("graph");
  const [loading, setLoading] = useState(false);
  const [currentStatus, setCurrentStatus] = useState("");
  const scrollRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight });
  }, [messages, currentStatus]);

  const handleSend = async () => {
    const query = input.trim();
    if (!query || loading) return;

    setInput("");
    setLoading(true);
    setCurrentStatus("Starting...");

    const userMsg: Message = { role: "user", content: query };
    setMessages((prev) => [...prev, userMsg]);

    const events: RAGEvent[] = [];
    let answer = "";
    let gremlinQuery = "";

    try {
      for await (const event of streamRAGQuery(query, mode, {
        n_triplets: 10,
        n_chunks: 5,
      })) {
        events.push(event);

        if (event.event === "status") {
          setCurrentStatus(event.data);
        } else if (event.event === "gremlin_query") {
          gremlinQuery = event.data;
        } else if (event.event === "gremlin_query_retry") {
          gremlinQuery = `[retry] ${event.data}`;
        } else if (event.event === "answer") {
          answer = event.data;
        }
      }
    } catch (e: any) {
      answer = `Error: ${e.message}`;
    }

    const assistantMsg: Message = {
      role: "assistant",
      content: answer || "No answer generated.",
      events,
      gremlinQuery,
      mode,
    };
    setMessages((prev) => [...prev, assistantMsg]);
    setLoading(false);
    setCurrentStatus("");
  };

  return (
    <div
      style={{
        display: "flex",
        flexDirection: "column",
        height: "calc(100vh - 120px)",
        padding: 12,
      }}
    >
      {/* Mode Selector */}
      <Segmented
        value={mode}
        onChange={(v) => setMode(v as RAGMode)}
        options={[
          {
            label: "Graph",
            value: "graph",
            icon: <NodeIndexOutlined />,
          },
          {
            label: "Vector",
            value: "vector",
            icon: <FileTextOutlined />,
          },
          {
            label: "Hybrid",
            value: "hybrid",
            icon: <MergeCellsOutlined />,
          },
        ]}
        block
        style={{ marginBottom: 8 }}
      />

      {/* Messages */}
      <div
        ref={scrollRef}
        style={{
          flex: 1,
          overflow: "auto",
          display: "flex",
          flexDirection: "column",
          gap: 8,
        }}
      >
        {messages.length === 0 && (
          <div
            style={{
              textAlign: "center",
              padding: 40,
              color: "#999",
            }}
          >
            <ThunderboltOutlined style={{ fontSize: 32, marginBottom: 8 }} />
            <br />
            <Text type="secondary">
              Ask a question about your knowledge graph
            </Text>
          </div>
        )}

        {messages.map((msg, i) => (
          <div
            key={i}
            style={{
              padding: 10,
              borderRadius: 8,
              background:
                msg.role === "user"
                  ? "#e6f4ff"
                  : msg.role === "system"
                    ? "#fff7e6"
                    : "#f6ffed",
              border:
                msg.role === "user"
                  ? "1px solid #91caff"
                  : msg.role === "system"
                    ? "1px solid #ffd591"
                    : "1px solid #b7eb8f",
            }}
          >
            <Space size={4} style={{ marginBottom: 4 }}>
              <Tag
                color={
                  msg.role === "user"
                    ? "blue"
                    : msg.role === "system"
                      ? "orange"
                      : "green"
                }
              >
                {msg.role}
              </Tag>
              {msg.mode && <Tag>{msg.mode}</Tag>}
            </Space>

            <Paragraph
              style={{ margin: 0, whiteSpace: "pre-wrap", fontSize: 13 }}
            >
              {msg.content}
            </Paragraph>

            {msg.gremlinQuery && (
              <Collapse
                size="small"
                style={{ marginTop: 6 }}
                items={[
                  {
                    key: "gremlin",
                    label: "Gremlin Query",
                    children: (
                      <pre
                        style={{
                          fontSize: 11,
                          background: "#1e1e1e",
                          color: "#d4d4d4",
                          padding: 8,
                          borderRadius: 4,
                          overflow: "auto",
                          maxHeight: 120,
                          margin: 0,
                        }}
                      >
                        {msg.gremlinQuery}
                      </pre>
                    ),
                  },
                ]}
              />
            )}

            {msg.events && msg.events.length > 0 && (
              <Collapse
                size="small"
                style={{ marginTop: 4 }}
                items={[
                  {
                    key: "events",
                    label: `${msg.events.length} events`,
                    children: (
                      <div
                        style={{
                          maxHeight: 120,
                          overflow: "auto",
                          fontSize: 11,
                        }}
                      >
                        {msg.events.map((ev, j) => (
                          <div key={j}>
                            <Tag color="default" style={{ fontSize: 10 }}>
                              {ev.event}
                            </Tag>
                            <Text type="secondary" style={{ fontSize: 10 }}>
                              {ev.data.slice(0, 100)}
                            </Text>
                          </div>
                        ))}
                      </div>
                    ),
                  },
                ]}
              />
            )}
          </div>
        ))}

        {loading && (
          <div
            style={{
              padding: 10,
              borderRadius: 8,
              background: "#fafafa",
              border: "1px solid #d9d9d9",
            }}
          >
            <Space>
              <Spin size="small" />
              <Text type="secondary">{currentStatus}</Text>
            </Space>
          </div>
        )}
      </div>

      {/* Input */}
      <div style={{ display: "flex", gap: 8, marginTop: 8 }}>
        <Input
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onPressEnter={handleSend}
          placeholder="Ask about your knowledge graph..."
          disabled={loading}
          autoFocus
        />
        <Button
          type="primary"
          icon={<SendOutlined />}
          onClick={handleSend}
          loading={loading}
        />
      </div>
    </div>
  );
}
