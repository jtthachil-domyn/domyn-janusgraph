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

public enum TenantIsolationStrategy {

    /**
     * Full isolation: each tenant gets its own Cassandra keyspace and
     * Elasticsearch index. Strongest guarantees, higher resource cost.
     */
    KEYSPACE_PER_TENANT,

    /**
     * Lightweight isolation: all tenants share one graph, isolated by
     * tenant_id property filtering on every traversal. Lower cost,
     * requires disciplined query patterns.
     */
    SHARED_GRAPH
}
