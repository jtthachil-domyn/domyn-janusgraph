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

import org.junit.jupiter.api.Test;

import static org.junit.jupiter.api.Assertions.*;

class TenantManagerValidationTest {

    @Test
    void constructorShouldAcceptBothStrategies() {
        TenantManager shared = new TenantManager(TenantIsolationStrategy.SHARED_GRAPH);
        assertEquals(TenantIsolationStrategy.SHARED_GRAPH, shared.getStrategy());

        TenantManager keyspace = new TenantManager(TenantIsolationStrategy.KEYSPACE_PER_TENANT);
        assertEquals(TenantIsolationStrategy.KEYSPACE_PER_TENANT, keyspace.getStrategy());
    }

    @Test
    void listTenantsShouldStartEmpty() {
        TenantManager mgr = new TenantManager(TenantIsolationStrategy.SHARED_GRAPH);
        assertTrue(mgr.listTenants().isEmpty());
    }

    @Test
    void createTenantWithNullIdShouldThrow() {
        TenantManager mgr = new TenantManager(TenantIsolationStrategy.SHARED_GRAPH);
        assertThrows(IllegalArgumentException.class, () -> mgr.createTenant(null));
    }

    @Test
    void createTenantWithEmptyIdShouldThrow() {
        TenantManager mgr = new TenantManager(TenantIsolationStrategy.SHARED_GRAPH);
        assertThrows(IllegalArgumentException.class, () -> mgr.createTenant(""));
    }

    @Test
    void createTenantWithInvalidCharsShouldThrow() {
        TenantManager mgr = new TenantManager(TenantIsolationStrategy.SHARED_GRAPH);
        assertThrows(IllegalArgumentException.class, () -> mgr.createTenant("tenant with spaces"));
        assertThrows(IllegalArgumentException.class, () -> mgr.createTenant("tenant@bad"));
    }

    @Test
    void createTenantWithTooLongIdShouldThrow() {
        TenantManager mgr = new TenantManager(TenantIsolationStrategy.SHARED_GRAPH);
        String longId = "a".repeat(65);
        assertThrows(IllegalArgumentException.class, () -> mgr.createTenant(longId));
    }

    @Test
    void validTenantIdsShouldPassValidation() {
        TenantManager mgr = new TenantManager(TenantIsolationStrategy.SHARED_GRAPH);
        // These should not throw on validation (they'll fail on graph creation
        // since there's no graph backend, but validation itself is the test)
        assertDoesNotThrow(() -> {
            try { mgr.createTenant("valid_tenant"); } catch (RuntimeException e) {
                if (e.getMessage().contains("must not be null") ||
                    e.getMessage().contains("must only contain")) {
                    throw e;
                }
            }
        });
    }
}
