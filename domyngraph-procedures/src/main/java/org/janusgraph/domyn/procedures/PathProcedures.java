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

import org.apache.tinkerpop.gremlin.process.traversal.Path;
import org.apache.tinkerpop.gremlin.process.traversal.dsl.graph.GraphTraversal;
import org.apache.tinkerpop.gremlin.process.traversal.dsl.graph.GraphTraversalSource;
import org.apache.tinkerpop.gremlin.process.traversal.dsl.graph.__;
import org.apache.tinkerpop.gremlin.structure.Vertex;

import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.stream.Collectors;

public class PathProcedures implements DomynProcedure {

    public static final String NAME = "kHop";

    private static final ProcedureDefinition DEFINITION = ProcedureDefinition.builder(NAME)
            .version("1.0")
            .description("Traverse k hops from a vertex identified by name, returning all paths")
            .addInput(ParameterDef.required("name", "String", "Name property of the start vertex"))
            .addInput(ParameterDef.required("hops", "Integer", "Number of hops to traverse"))
            .addInput(ParameterDef.optional("edgeLabel", "String",
                    "Edge label to traverse (all edges if omitted)"))
            .addInput(ParameterDef.optional("maxPaths", "Integer",
                    "Maximum number of paths to return (default 100)"))
            .outputType("path[]")
            .deterministic(true)
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
        String name = (String) args.get("name");
        int hops = ((Number) args.get("hops")).intValue();
        String edgeLabel = (String) args.get("edgeLabel");
        int maxPaths = args.containsKey("maxPaths")
                ? ((Number) args.get("maxPaths")).intValue() : 100;

        if (name == null || name.isEmpty()) {
            throw new IllegalArgumentException("'name' parameter is required");
        }
        if (hops < 1 || hops > 10) {
            throw new IllegalArgumentException("'hops' must be between 1 and 10");
        }

        return kHop(ctx.traversal(), name, hops, edgeLabel, maxPaths, ctx.getTenantId());
    }

    public static List<Path> kHop(GraphTraversalSource g, String name, int hops,
                                   String edgeLabel, int maxPaths, String tenantId) {
        GraphTraversal<Vertex, Vertex> start = tenantFilteredV(g, tenantId).has("name", name);
        if (edgeLabel != null && !edgeLabel.isEmpty()) {
            return start
                    .repeat(__.both(edgeLabel).simplePath())
                    .times(hops)
                    .path()
                    .limit(maxPaths)
                    .toList();
        } else {
            return start
                    .repeat(__.both().simplePath())
                    .times(hops)
                    .path()
                    .limit(maxPaths)
                    .toList();
        }
    }

    public static List<Path> kHop(GraphTraversalSource g, String name, int hops,
                                   String edgeLabel, int maxPaths) {
        return kHop(g, name, hops, edgeLabel, maxPaths, null);
    }

    public static List<Path> shortestPath(GraphTraversalSource g, String fromName,
                                           String toName, int maxDepth, String tenantId) {
        return tenantFilteredV(g, tenantId).has("name", fromName)
                .repeat(__.both().simplePath())
                .until(__.has("name", toName).or().loops().is(maxDepth))
                .has("name", toName)
                .path()
                .limit(10)
                .toList();
    }

    public static List<Path> shortestPath(GraphTraversalSource g, String fromName,
                                           String toName, int maxDepth) {
        return shortestPath(g, fromName, toName, maxDepth, null);
    }

    public static List<Map<String, Object>> neighbors(GraphTraversalSource g, String name,
                                                        int depth, String tenantId) {
        return tenantFilteredV(g, tenantId).has("name", name)
                .repeat(__.both().simplePath())
                .times(depth)
                .dedup()
                .valueMap(true)
                .toList()
                .stream()
                .map(PathProcedures::flattenValueMap)
                .collect(Collectors.toList());
    }

    public static List<Map<String, Object>> neighbors(GraphTraversalSource g, String name,
                                                        int depth) {
        return neighbors(g, name, depth, null);
    }

    private static GraphTraversal<Vertex, Vertex> tenantFilteredV(
            GraphTraversalSource g, String tenantId) {
        if (tenantId != null && !tenantId.isEmpty()) {
            return g.V().has("tenant_id", tenantId);
        }
        return g.V();
    }

    @SuppressWarnings("unchecked")
    private static Map<String, Object> flattenValueMap(Object raw) {
        Map<String, Object> result = new HashMap<>();
        if (raw instanceof Map) {
            Map<Object, Object> map = (Map<Object, Object>) raw;
            for (Map.Entry<Object, Object> entry : map.entrySet()) {
                Object value = entry.getValue();
                if (value instanceof List && ((List<?>) value).size() == 1) {
                    value = ((List<?>) value).get(0);
                }
                result.put(entry.getKey().toString(), value);
            }
        }
        return result;
    }
}
