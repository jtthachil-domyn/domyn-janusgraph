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

import org.apache.tinkerpop.gremlin.structure.Vertex;
import org.janusgraph.core.JanusGraph;
import org.janusgraph.core.Multiplicity;
import org.janusgraph.core.PropertyKey;
import org.janusgraph.core.VertexLabel;
import org.janusgraph.core.schema.JanusGraphManagement;
import org.janusgraph.core.schema.Mapping;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

public class TenantSchemaInitializer {

    private static final Logger logger = LoggerFactory.getLogger(TenantSchemaInitializer.class);
    public static final int SCHEMA_VERSION = 1;
    public static final String SCHEMA_META_TYPE = "__schema_meta";

    public static void initialize(JanusGraph graph) {
        logger.info("Initializing DomynGraph schema v{}", SCHEMA_VERSION);
        JanusGraphManagement mgmt = graph.openManagement();

        try {
            // -- Vertex Labels --
            makeVertexLabelIfAbsent(mgmt, "Entity");
            makeVertexLabelIfAbsent(mgmt, "Chunk");
            makeVertexLabelIfAbsent(mgmt, "Document");
            makeVertexLabelIfAbsent(mgmt, "Concept");

            // -- Edge Labels --
            makeEdgeLabelIfAbsent(mgmt, "RELATION");
            makeEdgeLabelIfAbsent(mgmt, "CONTAINS");
            makeEdgeLabelIfAbsent(mgmt, "REFERENCES");
            makeEdgeLabelIfAbsent(mgmt, "SIMILAR_TO");

            // -- Property Keys --
            PropertyKey name = makePropertyKeyIfAbsent(mgmt, "name", String.class);
            PropertyKey type = makePropertyKeyIfAbsent(mgmt, "type", String.class);
            PropertyKey tenantId = makePropertyKeyIfAbsent(mgmt, "tenant_id", String.class);
            PropertyKey externalId = makePropertyKeyIfAbsent(mgmt, "external_id", String.class);
            PropertyKey createdAt = makePropertyKeyIfAbsent(mgmt, "created_at", Long.class);
            PropertyKey description = makePropertyKeyIfAbsent(mgmt, "description", String.class);
            PropertyKey weight = makePropertyKeyIfAbsent(mgmt, "weight", Double.class);
            makePropertyKeyIfAbsent(mgmt, "embedding", byte[].class);
            makePropertyKeyIfAbsent(mgmt, "metadata", String.class);
            makePropertyKeyIfAbsent(mgmt, "schema_version", Integer.class);
            PropertyKey metaType = makePropertyKeyIfAbsent(mgmt, "__type", String.class);

            // -- Composite Indexes (Cassandra, exact-match) --
            if (!mgmt.containsGraphIndex("byExternalId")) {
                mgmt.buildIndex("byExternalId", Vertex.class)
                        .addKey(externalId)
                        .unique()
                        .buildCompositeIndex();
            }
            if (!mgmt.containsGraphIndex("byTenantId")) {
                mgmt.buildIndex("byTenantId", Vertex.class)
                        .addKey(tenantId)
                        .buildCompositeIndex();
            }
            if (!mgmt.containsGraphIndex("byName")) {
                mgmt.buildIndex("byName", Vertex.class)
                        .addKey(name)
                        .buildCompositeIndex();
            }
            if (!mgmt.containsGraphIndex("byType")) {
                mgmt.buildIndex("byType", Vertex.class)
                        .addKey(type)
                        .buildCompositeIndex();
            }
            if (!mgmt.containsGraphIndex("byTenantAndType")) {
                mgmt.buildIndex("byTenantAndType", Vertex.class)
                        .addKey(tenantId)
                        .addKey(type)
                        .buildCompositeIndex();
            }
            if (!mgmt.containsGraphIndex("byMetaType")) {
                mgmt.buildIndex("byMetaType", Vertex.class)
                        .addKey(metaType)
                        .buildCompositeIndex();
            }

            // -- Mixed Index (Elasticsearch, full-text + filtering) --
            if (!mgmt.containsGraphIndex("search")) {
                mgmt.buildIndex("search", Vertex.class)
                        .addKey(name, Mapping.TEXT.asParameter())
                        .addKey(type, Mapping.STRING.asParameter())
                        .addKey(tenantId, Mapping.STRING.asParameter())
                        .addKey(externalId, Mapping.STRING.asParameter())
                        .addKey(description, Mapping.TEXT.asParameter())
                        .addKey(createdAt, Mapping.DEFAULT.asParameter())
                        .buildMixedIndex("search");
            }

            mgmt.commit();
            logger.info("Schema v{} committed successfully", SCHEMA_VERSION);
        } catch (Exception e) {
            mgmt.rollback();
            throw new RuntimeException("Failed to initialize DomynGraph schema", e);
        }

        // Create schema meta vertex
        createSchemaMetaVertex(graph);
    }

    private static void createSchemaMetaVertex(JanusGraph graph) {
        boolean exists = graph.traversal().V()
                .has("__type", SCHEMA_META_TYPE)
                .hasNext();

        if (!exists) {
            Vertex meta = graph.addVertex();
            meta.property("__type", SCHEMA_META_TYPE);
            meta.property("schema_version", SCHEMA_VERSION);
            graph.tx().commit();
            logger.info("Created __schema_meta vertex with version {}", SCHEMA_VERSION);
        }
    }

    private static VertexLabel makeVertexLabelIfAbsent(JanusGraphManagement mgmt, String label) {
        if (mgmt.containsVertexLabel(label)) {
            return mgmt.getVertexLabel(label);
        }
        return mgmt.makeVertexLabel(label).make();
    }

    private static void makeEdgeLabelIfAbsent(JanusGraphManagement mgmt, String label) {
        if (!mgmt.containsEdgeLabel(label)) {
            mgmt.makeEdgeLabel(label).multiplicity(Multiplicity.MULTI).make();
        }
    }

    private static PropertyKey makePropertyKeyIfAbsent(JanusGraphManagement mgmt,
                                                        String name, Class<?> dataType) {
        if (mgmt.containsPropertyKey(name)) {
            return mgmt.getPropertyKey(name);
        }
        return mgmt.makePropertyKey(name).dataType(dataType).make();
    }
}
