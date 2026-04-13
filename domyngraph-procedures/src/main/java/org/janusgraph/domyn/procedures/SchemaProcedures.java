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

import org.janusgraph.core.JanusGraph;
import org.janusgraph.core.schema.JanusGraphIndex;
import org.janusgraph.core.schema.JanusGraphManagement;
import org.janusgraph.core.schema.SchemaAction;
import org.janusgraph.core.schema.SchemaStatus;
import org.janusgraph.graphdb.database.management.ManagementSystem;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import java.time.temporal.ChronoUnit;
import java.util.LinkedHashMap;
import java.util.Map;

public class SchemaProcedures implements DomynProcedure {

    private static final Logger logger = LoggerFactory.getLogger(SchemaProcedures.class);

    public static final String NAME = "indexStatus";

    private static final ProcedureDefinition DEFINITION = ProcedureDefinition.builder(NAME)
            .version("1.0")
            .description("Returns the status of all graph indexes")
            .outputType("map")
            .deterministic(false)
            .build();

    @Override
    public String name() {
        return NAME;
    }

    @Override
    public ProcedureDefinition definition() {
        return DEFINITION;
    }

    @Override
    public Object execute(ProcedureContext ctx, Map<String, Object> args) {
        return indexStatus(ctx.getGraph());
    }

    public static Map<String, String> indexStatus(JanusGraph graph) {
        Map<String, String> statuses = new LinkedHashMap<>();
        JanusGraphManagement mgmt = graph.openManagement();
        try {
            for (JanusGraphIndex index : mgmt.getGraphIndexes(org.apache.tinkerpop.gremlin.structure.Vertex.class)) {
                statuses.put(index.name(), index.getIndexStatus(
                        index.getFieldKeys()[0]).name());
            }
            for (JanusGraphIndex index : mgmt.getGraphIndexes(org.apache.tinkerpop.gremlin.structure.Edge.class)) {
                statuses.put(index.name(), index.getIndexStatus(
                        index.getFieldKeys()[0]).name());
            }
        } finally {
            mgmt.rollback();
        }
        return statuses;
    }

    public static void awaitIndex(JanusGraph graph, String indexName, long timeoutMs)
            throws InterruptedException {
        logger.info("Awaiting index '{}' to reach REGISTERED status...", indexName);
        ManagementSystem.awaitGraphIndexStatus(graph, indexName)
                .status(SchemaStatus.REGISTERED)
                .timeout(timeoutMs, ChronoUnit.MILLIS)
                .call();

        logger.info("Index '{}' is REGISTERED, triggering REINDEX...", indexName);
        JanusGraphManagement mgmt = graph.openManagement();
        try {
            JanusGraphIndex index = mgmt.getGraphIndex(indexName);
            mgmt.updateIndex(index, SchemaAction.REINDEX).get();
            mgmt.commit();
        } catch (Exception e) {
            mgmt.rollback();
            throw new RuntimeException("Failed to reindex " + indexName, e);
        }

        logger.info("Awaiting index '{}' to reach ENABLED status...", indexName);
        ManagementSystem.awaitGraphIndexStatus(graph, indexName)
                .status(SchemaStatus.ENABLED)
                .timeout(timeoutMs, ChronoUnit.MILLIS)
                .call();

        logger.info("Index '{}' is ENABLED and ready for queries", indexName);
    }
}
