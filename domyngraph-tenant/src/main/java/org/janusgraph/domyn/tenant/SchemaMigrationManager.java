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
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import java.util.ArrayList;
import java.util.Collections;
import java.util.Comparator;
import java.util.List;
import java.util.Optional;

public class SchemaMigrationManager {

    private static final Logger logger = LoggerFactory.getLogger(SchemaMigrationManager.class);
    private final List<SchemaMigration> migrations;

    public SchemaMigrationManager() {
        this.migrations = new ArrayList<>();
    }

    public SchemaMigrationManager(List<SchemaMigration> migrations) {
        this.migrations = new ArrayList<>(migrations);
        this.migrations.sort(Comparator.comparingInt(SchemaMigration::version));
    }

    public void addMigration(SchemaMigration migration) {
        migrations.add(migration);
        migrations.sort(Comparator.comparingInt(SchemaMigration::version));
    }

    public int getCurrentVersion(JanusGraph graph) {
        Optional<Object> version = graph.traversal().V()
                .has("__type", TenantSchemaInitializer.SCHEMA_META_TYPE)
                .values("schema_version")
                .tryNext();
        return version.map(v -> ((Number) v).intValue()).orElse(0);
    }

    public void migrateTo(JanusGraph graph, int targetVersion) {
        int current = getCurrentVersion(graph);
        if (current >= targetVersion) {
            logger.info("Schema already at version {} (target: {}), no migration needed",
                    current, targetVersion);
            return;
        }

        logger.info("Migrating schema from v{} to v{}", current, targetVersion);
        for (SchemaMigration migration : migrations) {
            if (migration.version() > current && migration.version() <= targetVersion) {
                logger.info("Applying migration v{}: {}", migration.version(),
                        migration.description());
                migration.apply(graph);
                updateVersion(graph, migration.version());
                logger.info("Migration v{} applied successfully", migration.version());
            }
        }
    }

    public void migrateToLatest(JanusGraph graph) {
        if (migrations.isEmpty()) {
            return;
        }
        int latest = migrations.get(migrations.size() - 1).version();
        migrateTo(graph, latest);
    }

    public List<SchemaMigration> getMigrations() {
        return Collections.unmodifiableList(migrations);
    }

    private void updateVersion(JanusGraph graph, int version) {
        Vertex meta = graph.traversal().V()
                .has("__type", TenantSchemaInitializer.SCHEMA_META_TYPE)
                .next();
        meta.property("schema_version", version);
        graph.tx().commit();
    }
}
