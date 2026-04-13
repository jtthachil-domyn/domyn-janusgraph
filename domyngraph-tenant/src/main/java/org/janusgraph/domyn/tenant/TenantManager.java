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

import org.apache.tinkerpop.gremlin.process.traversal.dsl.graph.GraphTraversalSource;
import org.janusgraph.core.ConfiguredGraphFactory;
import org.janusgraph.core.JanusGraph;
import org.janusgraph.core.JanusGraphFactory;
import org.janusgraph.graphdb.management.JanusGraphManager;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import java.util.Collections;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;

public class TenantManager {

    private static final Logger logger = LoggerFactory.getLogger(TenantManager.class);
    private static final String TENANT_PREFIX = "domyn_";

    private final TenantIsolationStrategy strategy;
    private final SchemaMigrationManager migrationManager;

    private final ConcurrentHashMap<String, JanusGraph> openGraphs = new ConcurrentHashMap<>();

    // For SHARED_GRAPH mode, this is the single shared graph instance
    private volatile JanusGraph sharedGraph;

    public TenantManager(TenantIsolationStrategy strategy) {
        this(strategy, new SchemaMigrationManager());
    }

    public TenantManager(TenantIsolationStrategy strategy,
                         SchemaMigrationManager migrationManager) {
        this.strategy = strategy;
        this.migrationManager = migrationManager;
    }

    public JanusGraph createTenant(String tenantId) {
        return createTenant(tenantId, Collections.emptyMap());
    }

    public JanusGraph createTenant(String tenantId, Map<String, Object> overrides) {
        validateTenantId(tenantId);
        String graphName = toGraphName(tenantId);
        logger.info("Creating tenant '{}' (graph: {}, strategy: {})",
                tenantId, graphName, strategy);

        JanusGraph graph;

        if (strategy == TenantIsolationStrategy.KEYSPACE_PER_TENANT) {
            graph = createIsolatedTenantGraph(graphName, overrides);
        } else {
            graph = getOrCreateSharedGraph(overrides);
        }

        TenantSchemaInitializer.initialize(graph);
        migrationManager.migrateToLatest(graph);

        openGraphs.put(tenantId, graph);
        logger.info("Tenant '{}' created successfully", tenantId);
        return graph;
    }

    public JanusGraph openTenant(String tenantId) {
        validateTenantId(tenantId);

        JanusGraph cached = openGraphs.get(tenantId);
        if (cached != null && cached.isOpen()) {
            migrationManager.migrateToLatest(cached);
            return cached;
        }

        String graphName = toGraphName(tenantId);
        logger.info("Opening tenant '{}' (graph: {})", tenantId, graphName);

        JanusGraph graph;
        if (strategy == TenantIsolationStrategy.KEYSPACE_PER_TENANT) {
            try {
                graph = ConfiguredGraphFactory.open(graphName);
            } catch (Exception e) {
                throw new IllegalStateException(
                        "Tenant '" + tenantId + "' does not exist or cannot be opened", e);
            }
        } else {
            graph = getOrCreateSharedGraph(Collections.emptyMap());
        }

        migrationManager.migrateToLatest(graph);
        openGraphs.put(tenantId, graph);
        return graph;
    }

    public GraphTraversalSource traversal(String tenantId) {
        JanusGraph graph = openTenant(tenantId);

        if (strategy == TenantIsolationStrategy.SHARED_GRAPH) {
            return TenantAwareTraversalSource.create(graph, tenantId);
        }
        return graph.traversal();
    }

    public void closeTenant(String tenantId) {
        JanusGraph graph = openGraphs.remove(tenantId);
        if (graph != null && graph.isOpen()
                && strategy == TenantIsolationStrategy.KEYSPACE_PER_TENANT) {
            try {
                graph.close();
                logger.info("Closed tenant '{}'", tenantId);
            } catch (Exception e) {
                logger.warn("Error closing tenant '{}': {}", tenantId, e.getMessage());
            }
        }
    }

    public void dropTenant(String tenantId) {
        validateTenantId(tenantId);
        String graphName = toGraphName(tenantId);
        logger.warn("Dropping tenant '{}' (graph: {}) — THIS IS DESTRUCTIVE",
                tenantId, graphName);

        closeTenant(tenantId);

        if (strategy == TenantIsolationStrategy.KEYSPACE_PER_TENANT) {
            try {
                ConfiguredGraphFactory.drop(graphName);
                logger.info("Tenant '{}' dropped completely", tenantId);
            } catch (Exception e) {
                throw new RuntimeException("Failed to drop tenant " + tenantId, e);
            }
        } else {
            logger.warn("Cannot drop tenant in SHARED_GRAPH mode — " +
                    "data must be deleted manually via tenant_id filter");
        }
    }

    public Set<String> listTenants() {
        return Collections.unmodifiableSet(openGraphs.keySet());
    }

    public TenantIsolationStrategy getStrategy() {
        return strategy;
    }

    public int getSchemaVersion(String tenantId) {
        JanusGraph graph = openTenant(tenantId);
        return migrationManager.getCurrentVersion(graph);
    }

    private JanusGraph createIsolatedTenantGraph(String graphName, Map<String, Object> overrides) {
        try {
            return ConfiguredGraphFactory.create(graphName);
        } catch (Exception e) {
            throw new RuntimeException("Failed to create isolated graph for " + graphName, e);
        }
    }

    private JanusGraph getOrCreateSharedGraph(Map<String, Object> overrides) {
        if (sharedGraph != null && sharedGraph.isOpen()) {
            return sharedGraph;
        }
        synchronized (this) {
            if (sharedGraph != null && sharedGraph.isOpen()) {
                return sharedGraph;
            }
            try {
                sharedGraph = ConfiguredGraphFactory.open(TENANT_PREFIX + "shared");
            } catch (Exception e) {
                try {
                    sharedGraph = ConfiguredGraphFactory.create(TENANT_PREFIX + "shared");
                } catch (Exception e2) {
                    throw new RuntimeException("Failed to open/create shared graph", e2);
                }
            }
            return sharedGraph;
        }
    }

    private static String toGraphName(String tenantId) {
        return TENANT_PREFIX + tenantId.toLowerCase().replaceAll("[^a-z0-9_]", "_");
    }

    private static void validateTenantId(String tenantId) {
        if (tenantId == null || tenantId.trim().isEmpty()) {
            throw new IllegalArgumentException("tenantId must not be null or empty");
        }
        if (tenantId.length() > 64) {
            throw new IllegalArgumentException("tenantId must be 64 characters or fewer");
        }
        if (!tenantId.matches("^[a-zA-Z0-9_-]+$")) {
            throw new IllegalArgumentException(
                    "tenantId must only contain alphanumeric characters, hyphens, or underscores");
        }
    }
}
