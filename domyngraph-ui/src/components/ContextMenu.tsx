import { useEffect, useRef, useState, useCallback } from "react";
import { Menu } from "antd";
import {
  ExpandOutlined,
  EyeInvisibleOutlined,
  PushpinOutlined,
  InfoCircleOutlined,
  BranchesOutlined,
} from "@ant-design/icons";
import { useGraphStore } from "../store/graphStore";
import { expandVertex } from "../api/client";
import { message } from "antd";

interface Position {
  x: number;
  y: number;
  nodeId: string;
}

export default function ContextMenu() {
  const [pos, setPos] = useState<Position | null>(null);
  const menuRef = useRef<HTMLDivElement>(null);

  const activeTenant = useGraphStore((s) => s.activeTenant);
  const addGraphData = useGraphStore((s) => s.addGraphData);
  const setFocus = useGraphStore((s) => s.setFocus);
  const hideNode = useGraphStore((s) => s.hideNode);
  const pinNode = useGraphStore((s) => s.pinNode);
  const selectNode = useGraphStore((s) => s.selectNode);
  const recordExpand = useGraphStore((s) => s.recordExpand);

  useEffect(() => {
    const handleContextMenu = (e: MouseEvent) => {
      const target = e.target as HTMLElement;
      const nodeId = target?.closest?.("[data-node-id]")?.getAttribute("data-node-id");
      if (nodeId) {
        e.preventDefault();
        setPos({ x: e.clientX, y: e.clientY, nodeId });
      }
    };
    const handleClick = () => setPos(null);

    document.addEventListener("contextmenu", handleContextMenu);
    document.addEventListener("click", handleClick);
    return () => {
      document.removeEventListener("contextmenu", handleContextMenu);
      document.removeEventListener("click", handleClick);
    };
  }, []);

  const handleExpand = useCallback(async () => {
    if (!pos) return;
    try {
      const resp = await expandVertex(pos.nodeId, activeTenant, 1, 50);
      addGraphData(resp);
      recordExpand(pos.nodeId, { depth: 1, edgeTypes: null });
      setFocus(pos.nodeId);
    } catch (err: unknown) {
      message.error(`Expand failed: ${err instanceof Error ? err.message : "unknown"}`);
    }
    setPos(null);
  }, [pos, activeTenant, addGraphData, recordExpand, setFocus]);

  const handleHide = useCallback(() => {
    if (pos) {
      hideNode(pos.nodeId);
      setPos(null);
    }
  }, [pos, hideNode]);

  const handlePin = useCallback(() => {
    if (pos) {
      pinNode(pos.nodeId);
      setPos(null);
    }
  }, [pos, pinNode]);

  const handleDetail = useCallback(() => {
    if (pos) {
      selectNode(pos.nodeId);
      setPos(null);
    }
  }, [pos, selectNode]);

  if (!pos) return null;

  return (
    <div
      ref={menuRef}
      style={{
        position: "fixed",
        left: pos.x,
        top: pos.y,
        zIndex: 1000,
        boxShadow: "0 2px 8px rgba(0,0,0,0.15)",
        borderRadius: 6,
        background: "#fff",
      }}
    >
      <Menu
        items={[
          {
            key: "expand",
            icon: <ExpandOutlined />,
            label: "Expand Neighbors",
            onClick: handleExpand,
          },
          {
            key: "hide",
            icon: <EyeInvisibleOutlined />,
            label: "Hide Node",
            onClick: handleHide,
          },
          {
            key: "pin",
            icon: <PushpinOutlined />,
            label: "Pin/Unpin Node",
            onClick: handlePin,
          },
          {
            key: "detail",
            icon: <InfoCircleOutlined />,
            label: "Show Details",
            onClick: handleDetail,
          },
        ]}
        style={{ border: "none" }}
      />
    </div>
  );
}
