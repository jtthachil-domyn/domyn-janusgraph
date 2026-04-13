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

import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import java.util.Collection;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.stream.Collectors;

public final class ProcedureRegistry {

    private static final Logger logger = LoggerFactory.getLogger(ProcedureRegistry.class);
    private static final ProcedureRegistry INSTANCE = new ProcedureRegistry();

    private final ConcurrentHashMap<String, DomynProcedure> procedures = new ConcurrentHashMap<>();

    private ProcedureRegistry() {}

    public static ProcedureRegistry getInstance() {
        return INSTANCE;
    }

    public void register(DomynProcedure procedure) {
        String name = procedure.name();
        DomynProcedure existing = procedures.putIfAbsent(name, procedure);
        if (existing != null) {
            logger.warn("Procedure '{}' already registered, skipping duplicate", name);
        } else {
            logger.info("Registered procedure: {} (v{})", name,
                    procedure.definition().getVersion());
        }
    }

    public DomynProcedure get(String name) {
        DomynProcedure proc = procedures.get(name);
        if (proc == null) {
            throw new IllegalArgumentException("Unknown procedure: " + name);
        }
        return proc;
    }

    public boolean has(String name) {
        return procedures.containsKey(name);
    }

    public Collection<DomynProcedure> listAll() {
        return Collections.unmodifiableCollection(procedures.values());
    }

    public List<String> listNames() {
        return procedures.keySet().stream().sorted().collect(Collectors.toList());
    }

    public Map<String, Object> introspect(String name) {
        return get(name).definition().toMap();
    }

    public List<Map<String, Object>> introspectAll() {
        return procedures.values().stream()
                .map(p -> p.definition().toMap())
                .collect(Collectors.toList());
    }

    public Object call(String name, ProcedureContext ctx, Map<String, Object> args) {
        DomynProcedure proc = get(name);
        ctx.logStart(name);
        boolean success = false;
        try {
            Object result = proc.execute(ctx, args);
            success = true;
            return result;
        } finally {
            ctx.logEnd(name, success);
        }
    }

    public int size() {
        return procedures.size();
    }

    /**
     * Convenience for Gremlin script usage:
     * DomynProcedures.list()
     * DomynProcedures.describe("kHop")
     * DomynProcedures.call("kHop", ctx, args)
     */
    public static final class DomynProcedures {

        private DomynProcedures() {}

        public static List<String> list() {
            return getInstance().listNames();
        }

        public static Map<String, Object> describe(String name) {
            return getInstance().introspect(name);
        }

        public static List<Map<String, Object>> describeAll() {
            return getInstance().introspectAll();
        }

        public static Object call(String name, ProcedureContext ctx, Map<String, Object> args) {
            return getInstance().call(name, ctx, args);
        }
    }
}
