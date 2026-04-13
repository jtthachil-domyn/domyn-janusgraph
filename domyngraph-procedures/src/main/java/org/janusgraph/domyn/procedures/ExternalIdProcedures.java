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

import org.apache.tinkerpop.gremlin.process.traversal.dsl.graph.GraphTraversalSource;
import org.apache.tinkerpop.gremlin.structure.Vertex;

import java.util.Map;
import java.util.Optional;
import java.util.UUID;

public class ExternalIdProcedures implements DomynProcedure {

    public static final String NAME = "getByExternalId";
    public static final String EXTERNAL_ID_PROPERTY = "external_id";

    private static final ProcedureDefinition DEFINITION = ProcedureDefinition.builder(NAME)
            .version("1.0")
            .description("Look up a vertex by its external UUID")
            .addInput(ParameterDef.required("externalId", "String", "The external UUID"))
            .outputType("vertex")
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
        String externalId = (String) args.get("externalId");
        if (externalId == null || externalId.isEmpty()) {
            throw new IllegalArgumentException("'externalId' parameter is required");
        }
        return getByExternalId(ctx.traversal(), externalId)
                .orElseThrow(() -> new IllegalArgumentException(
                        "No vertex found with external_id: " + externalId));
    }

    public static Optional<Vertex> getByExternalId(GraphTraversalSource g, String externalId) {
        return g.V().has(EXTERNAL_ID_PROPERTY, externalId).tryNext();
    }

    public static String assignExternalId(Vertex vertex) {
        String id = UUID.randomUUID().toString();
        vertex.property(EXTERNAL_ID_PROPERTY, id);
        return id;
    }

    public static String getOrAssignExternalId(Vertex vertex) {
        if (vertex.property(EXTERNAL_ID_PROPERTY).isPresent()) {
            return vertex.value(EXTERNAL_ID_PROPERTY);
        }
        return assignExternalId(vertex);
    }
}
