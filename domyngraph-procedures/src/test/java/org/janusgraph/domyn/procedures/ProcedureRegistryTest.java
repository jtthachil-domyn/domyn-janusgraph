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

import org.junit.jupiter.api.Test;

import java.util.Collections;
import java.util.List;
import java.util.Map;

import static org.junit.jupiter.api.Assertions.*;

class ProcedureRegistryTest {

    @Test
    void registryShouldBeASingleton() {
        ProcedureRegistry a = ProcedureRegistry.getInstance();
        ProcedureRegistry b = ProcedureRegistry.getInstance();
        assertSame(a, b);
    }

    @Test
    void registerAndRetrieveProcedure() {
        ProcedureRegistry registry = ProcedureRegistry.getInstance();
        DomynProcedure stub = new StubProcedure("test_register");
        registry.register(stub);

        assertTrue(registry.has("test_register"));
        assertSame(stub, registry.get("test_register"));
    }

    @Test
    void getUnknownProcedureShouldThrow() {
        ProcedureRegistry registry = ProcedureRegistry.getInstance();
        assertThrows(IllegalArgumentException.class, () -> registry.get("no_such_proc"));
    }

    @Test
    void listNamesShouldReturnSorted() {
        ProcedureRegistry registry = ProcedureRegistry.getInstance();
        registry.register(new StubProcedure("zz_proc"));
        registry.register(new StubProcedure("aa_proc"));

        List<String> names = registry.listNames();
        int aaIdx = names.indexOf("aa_proc");
        int zzIdx = names.indexOf("zz_proc");
        assertTrue(aaIdx >= 0);
        assertTrue(zzIdx >= 0);
        assertTrue(aaIdx < zzIdx, "listNames() should be sorted");
    }

    @Test
    void introspectShouldReturnMap() {
        ProcedureRegistry registry = ProcedureRegistry.getInstance();
        registry.register(new StubProcedure("test_introspect"));

        Map<String, Object> meta = registry.introspect("test_introspect");
        assertNotNull(meta);
        assertEquals("test_introspect", meta.get("name"));
    }

    @Test
    void duplicateRegisterShouldNotReplace() {
        ProcedureRegistry registry = ProcedureRegistry.getInstance();
        StubProcedure first = new StubProcedure("test_dup");
        StubProcedure second = new StubProcedure("test_dup");
        registry.register(first);
        registry.register(second);

        assertSame(first, registry.get("test_dup"));
    }

    private static class StubProcedure implements DomynProcedure {
        private final String name;

        StubProcedure(String name) {
            this.name = name;
        }

        @Override
        public String name() {
            return name;
        }

        @Override
        public ProcedureDefinition definition() {
            return ProcedureDefinition.builder(name)
                    .version("1.0")
                    .description("stub")
                    .outputType("void")
                    .build();
        }

        @Override
        public Object execute(ProcedureContext ctx, Map<String, Object> args) {
            return Collections.emptyMap();
        }
    }
}
