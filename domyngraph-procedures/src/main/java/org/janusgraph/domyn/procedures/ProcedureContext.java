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

package org.janusgraph.domyn.procedures;

import org.apache.tinkerpop.gremlin.process.traversal.dsl.graph.GraphTraversalSource;
import org.janusgraph.core.JanusGraph;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;
import org.slf4j.MDC;

import java.util.UUID;

public class ProcedureContext {

    private final String tenantId;
    private final JanusGraph graph;
    private final GraphTraversalSource g;
    private final String requestId;
    private final long startTimeMs;
    private final Logger logger;

    private ProcedureContext(String tenantId, JanusGraph graph, GraphTraversalSource g,
                            String requestId) {
        this.tenantId = tenantId;
        this.graph = graph;
        this.g = g;
        this.requestId = requestId;
        this.startTimeMs = System.currentTimeMillis();
        this.logger = LoggerFactory.getLogger(ProcedureContext.class);
    }

    public static ProcedureContext create(JanusGraph graph, String tenantId) {
        String requestId = UUID.randomUUID().toString();
        GraphTraversalSource g = graph.traversal();
        return new ProcedureContext(tenantId, graph, g, requestId);
    }

    public static ProcedureContext create(JanusGraph graph, GraphTraversalSource g,
                                          String tenantId) {
        String requestId = UUID.randomUUID().toString();
        return new ProcedureContext(tenantId, graph, g, requestId);
    }

    public String getTenantId() {
        return tenantId;
    }

    public JanusGraph getGraph() {
        return graph;
    }

    public GraphTraversalSource traversal() {
        return g;
    }

    public String getRequestId() {
        return requestId;
    }

    public long getStartTimeMs() {
        return startTimeMs;
    }

    public long getElapsedMs() {
        return System.currentTimeMillis() - startTimeMs;
    }

    public void logStart(String procedureName) {
        MDC.put("requestId", requestId);
        MDC.put("tenantId", tenantId);
        logger.info("PROC_START procedure={} tenant={} requestId={}",
                procedureName, tenantId, requestId);
    }

    public void logEnd(String procedureName, boolean success) {
        logger.info("PROC_END procedure={} tenant={} requestId={} elapsed={}ms success={}",
                procedureName, tenantId, requestId, getElapsedMs(), success);
        MDC.remove("requestId");
        MDC.remove("tenantId");
    }
}
