# DomynGraph Lens

A semantic graph interaction system for the DomynGraph Engine. Not a graph viewer — a production-grade exploration tool built on G6 v5.

## Quick Start

**Prerequisites:** The DomynGraph cluster must be running (Cassandra + Elasticsearch + JanusGraph).

```bash
# Start the API (from domyngraph-api/)
cd domyngraph-api
python3 -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt
uvicorn app.main:app --host 0.0.0.0 --port 8080 --reload

# Start the UI (from domyngraph-ui/)
cd domyngraph-ui
npm install
npm run dev -- --port 3000
```

Open **http://localhost:3000** in your browser.

For Docker deployment (all-in-one):

```bash
cd domyngraph-docker
docker compose up --build
```

This starts: Cassandra :9042 → Elasticsearch :9200 → JanusGraph :8182 → API :8080 → UI :3000

---

## What You See On Load

The UI auto-loads all vertices and edges for the active tenant. The demo dataset includes:

| Node | Type | Description |
|------|------|-------------|
| NVIDIA | Company | GPU manufacturer |
| Google | Company | Technology company |
| Apple | Company | Technology company |
| Jensen Huang | Person | CEO of NVIDIA |
| Sundar Pichai | Person | CEO of Google |
| Tim Cook | Person | CEO of Apple |
| Artificial Intelligence | Technology | AI concept |
| Semiconductors | Technology | Chip manufacturing |

Edges connect people to companies (RELATION), companies to technologies (REFERENCES), and companies to each other (SIMILAR_TO).

---

## How to Use Each Feature

### Graph Canvas (center area)

- **Click** a node — selects it, shows its details in the right panel
- **Click** an edge — selects it, shows the edge details (label, source, target, weight) in the right panel
- **Double-click** a node — expands its neighbors (fetches connected nodes from the database)
- **Double-click** an already-expanded node — collapses it (removes neighbors that are not connected to other visible nodes)
- **Drag** a node — repositions it on the canvas
- **Scroll wheel** — zoom in/out
- **Click + drag canvas** — pan the view

Expanded nodes get a green border to indicate they have been expanded. Double-click again to collapse.

Nodes are **color-coded by type**:
- Blue = Company
- Purple = Person
- Cyan = Technology
- Green = Concept
- Orange = Entity

Node **size scales with edge count** — more connected nodes appear larger.

**Edge labels** are hidden by default to reduce clutter. Hover over an edge to see its label and weight in a tooltip.

### Toolbar (top bar)

| Control | What it does |
|---------|-------------|
| Layout buttons (3 icons) | Switch between **Force** (organic), **Dagre** (hierarchical), **Radial** (circular) layouts |
| Arrow toggle | Show/hide directional arrows on edges (useful for OWNS, WORKS_AT, REPORTS_TO) |
| Perspective dropdown | Filter what's visible: "Full Graph" shows everything, "People & Companies" hides Technology/Concept nodes |
| Reset (circular arrow) | Clears hidden nodes and selection, keeps graph data |
| Clear (red trash) | Clears everything and reloads from scratch |

### Search Bar (top right)

Type a name (e.g., "Apple") and press Enter. Matching nodes are added to the canvas and the camera centers on the first result.

Search uses JanusGraph's Elasticsearch full-text index on the `name` property.

### Tenant Selector (top right corner)

Switch between tenants. Each tenant has isolated data. Switching tenants clears the canvas and reloads.

---

## Right Panel Tabs

### Detail Tab

Shows properties of the selected element:

**When a node is selected:**
- Node label, type, and ID
- Hydration status (lightweight = only basic properties, hydrated = full properties fetched)
- All stored properties (tenant_id, external_id, algorithm outputs, etc.)
- List of connected edges with direction (incoming/outgoing), label, and target node

**When an edge is selected:**
- Edge label and direction
- Source and target nodes (name and type)
- Edge properties (weight, etc.)

### Procedures Tab

Run DomynGraph custom procedures:
1. Select a procedure from the dropdown
2. Enter parameters as JSON (e.g., `{"key": "value"}`)
3. Click "Run Procedure"
4. Results appear as JSON below

### Algorithms Tab

Run graph algorithms asynchronously. The algorithm runs server-side and results are polled every 2 seconds.

#### PageRank

Ranks every vertex by importance. Vertices with many incoming connections from other important vertices score higher.

- **What it does:** Computes a rank score for every vertex in the graph
- **Parameters:** Max iterations (default 20), timeout
- **Result:** Shows the PageRank program name, execution metadata
- **Use case:** Find the most important/central entities in your graph

#### BFS (Breadth-First Search)

Explores the graph level-by-level from a seed node.

- **What it does:** Computes the shortest hop distance from a seed node to every reachable vertex
- **Parameters:** Click a node first to set it as the seed, then run
- **Result:** Each vertex gets a `domyn.bfs.depth` property showing its distance from the seed
- **Use case:** "How far is entity X from entity Y?"

#### Connected Components

Finds isolated clusters in the graph.

- **What it does:** Assigns a component ID to every vertex. Vertices in the same connected cluster share the same ID
- **Parameters:** Max iterations, timeout
- **Result:** Each vertex gets a `domyn.connectedComponents.component` property
- **Use case:** "Are there disconnected groups in my data?"

**After running an algorithm**, the results are persisted to vertex properties in JanusGraph. You can see them in the Detail panel when you click a node (look for `domyn.pageRank.rank`, `domyn.bfs.depth`, `domyn.connectedComponents.component` properties).

### Schema Tab

Shows the current state of JanusGraph indexes and schema version. Indexes should be in ENABLED state for queries to work.

### Tenants Tab

Lists all tenants with their vertex and edge counts. Click a tenant row to switch to it.

### Health Tab

Shows system health:
- **API:** Is the FastAPI server running?
- **Gremlin:** Is JanusGraph reachable?
- **Vertex Count:** Total vertices in the database
- **Cache:** Current cache size, max size, TTL

---

## Architecture

```
Browser :3000          API :8080              JanusGraph :8182
┌──────────────┐     ┌──────────────┐      ┌──────────────┐
│ React + G6   │────▶│ FastAPI      │─────▶│ Gremlin WS   │
│ Zustand      │ HTTP│ GraphTransf. │  WS  │ Cassandra    │
│ Ant Design   │◀────│ LRU Cache    │◀─────│ Elasticsearch│
└──────────────┘     │ Async Jobs   │      └──────────────┘
                     └──────────────┘
```

The API layer is mandatory because browsers cannot talk to Gremlin Server directly (binary WebSocket protocol, no CORS).

---

## Troubleshooting

| Problem | Fix |
|---------|-----|
| Empty canvas on load | Check that JanusGraph is running (`curl http://localhost:8080/api/health`) and has data |
| "Expand failed" on double-click | The vertex might not have neighbors, or the Gremlin query timed out |
| Search returns nothing | Search uses `textContains` on the `name` property — check that the ES index is ENABLED in the Schema tab |
| Algorithm stays "running" forever | Check API logs for errors. Algorithms run via JanusGraph's GraphComputer (OLAP) which can be slow on large graphs |
| Nodes overlap | Switch to Dagre layout (hierarchical) or zoom out. Force layout needs time to settle |
