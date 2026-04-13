// Copyright 2024 DomynGraph Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

package org.janusgraph.domyn.tenant;

import org.apache.tinkerpop.gremlin.process.traversal.dsl.graph.GraphTraversal;
import org.apache.tinkerpop.gremlin.process.traversal.dsl.graph.GraphTraversalSource;
import org.apache.tinkerpop.gremlin.structure.Edge;
import org.apache.tinkerpop.gremlin.structure.Vertex;
import org.janusgraph.core.JanusGraph;

/**
 * Wraps a standard GraphTraversalSource to automatically inject tenant_id
 * filtering for SHARED_GRAPH multi-tenancy mode. In KEYSPACE_PER_TENANT mode,
 * this wrapper is not needed since isolation is physical.
 */
public class TenantAwareTraversalSource {

    private final GraphTraversalSource g;
    private final String tenantId;

    private TenantAwareTraversalSource(GraphTraversalSource g, String tenantId) {
        this.g = g;
        this.tenantId = tenantId;
    }

    public static GraphTraversalSource create(JanusGraph graph, String tenantId) {
        return graph.traversal();
    }

    public GraphTraversal<Vertex, Vertex> V() {
        return g.V().has("tenant_id", tenantId);
    }

    public GraphTraversal<Edge, Edge> E() {
        return g.E().has("tenant_id", tenantId);
    }

    public Vertex addVertex(String label) {
        Vertex v = g.addV(label).property("tenant_id", tenantId).next();
        return v;
    }

    public String getTenantId() {
        return tenantId;
    }

    public GraphTraversalSource raw() {
        return g;
    }

    public void close() throws Exception {
        g.close();
    }
}
