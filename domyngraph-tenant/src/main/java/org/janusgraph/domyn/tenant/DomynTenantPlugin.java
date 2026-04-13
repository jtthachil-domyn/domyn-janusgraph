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

import org.apache.tinkerpop.gremlin.jsr223.AbstractGremlinPlugin;
import org.apache.tinkerpop.gremlin.jsr223.DefaultImportCustomizer;

public class DomynTenantPlugin extends AbstractGremlinPlugin {

    private static final String NAME = "org.janusgraph.domyn.tenant";

    private static final DomynTenantPlugin INSTANCE = new DomynTenantPlugin();

    public DomynTenantPlugin() {
        super(NAME, DefaultImportCustomizer.build()
                .addClassImports(
                        TenantManager.class,
                        TenantIsolationStrategy.class,
                        TenantSchemaInitializer.class,
                        TenantAwareTraversalSource.class,
                        SchemaMigration.class,
                        SchemaMigrationManager.class
                )
                .create());
    }

    public static DomynTenantPlugin instance() {
        return INSTANCE;
    }
}
