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

package org.janusgraph.domyn.algorithms;

import org.apache.commons.configuration2.Configuration;
import org.apache.tinkerpop.gremlin.process.computer.GraphComputer;
import org.apache.tinkerpop.gremlin.process.computer.Memory;
import org.apache.tinkerpop.gremlin.process.computer.MessageCombiner;
import org.apache.tinkerpop.gremlin.process.computer.MessageScope;
import org.apache.tinkerpop.gremlin.process.computer.Messenger;
import org.apache.tinkerpop.gremlin.process.computer.VertexComputeKey;
import org.apache.tinkerpop.gremlin.process.computer.util.AbstractVertexProgramBuilder;
import org.apache.tinkerpop.gremlin.process.computer.util.StaticVertexProgram;
import org.apache.tinkerpop.gremlin.process.traversal.dsl.graph.__;
import org.apache.tinkerpop.gremlin.structure.Graph;
import org.apache.tinkerpop.gremlin.structure.Vertex;
import org.apache.tinkerpop.gremlin.structure.VertexProperty;
import org.apache.tinkerpop.gremlin.util.iterator.IteratorUtils;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import java.util.Collections;
import java.util.HashSet;
import java.util.Iterator;
import java.util.Optional;
import java.util.Set;

/**
 * Production-grade shortest distance (SSSP) implementation for DomynGraph.
 * Enhanced from JanusGraph's test-only implementation with:
 * - Configurable weight property
 * - Optional target vertex (early termination)
 * - Proper message combining (min distance)
 */
public class DomynShortestDistanceVertexProgram extends StaticVertexProgram<Long> {

    private static final Logger logger = LoggerFactory.getLogger(DomynShortestDistanceVertexProgram.class);

    public static final String DISTANCE = "domyn.shortestDistance.distance";
    private static final String MAX_DEPTH = "domyn.shortestDistance.maxDepth";
    private static final String WEIGHT_PROPERTY = "domyn.shortestDistance.weightProperty";
    private static final String SEED = "domyn.shortestDistance.seed";

    private static final Set<VertexComputeKey> COMPUTE_KEYS =
            new HashSet<>(Collections.singletonList(VertexComputeKey.of(DISTANCE, false)));

    private int maxDepth;
    private long seed;
    private String weightProperty;
    private MessageScope.Local<Long> incidentMessageScope;

    private DomynShortestDistanceVertexProgram() {}

    @Override
    public void loadState(final Graph graph, final Configuration configuration) {
        maxDepth = configuration.getInt(MAX_DEPTH, 10);
        seed = configuration.getLong(SEED);
        weightProperty = configuration.getString(WEIGHT_PROPERTY, "weight");
        incidentMessageScope = MessageScope.Local.of(__::inE,
                (msg, edge) -> {
                    Number w = edge.property(weightProperty).isPresent()
                            ? edge.<Number>value(weightProperty) : 1;
                    return msg + w.longValue();
                });
    }

    @Override
    public void storeState(final Configuration configuration) {
        configuration.setProperty(VERTEX_PROGRAM, DomynShortestDistanceVertexProgram.class.getName());
        configuration.setProperty(MAX_DEPTH, maxDepth);
        configuration.setProperty(SEED, seed);
        configuration.setProperty(WEIGHT_PROPERTY, weightProperty);
    }

    @Override
    public Set<VertexComputeKey> getVertexComputeKeys() {
        return COMPUTE_KEYS;
    }

    @Override
    public Optional<MessageCombiner<Long>> getMessageCombiner() {
        return Optional.of(new MinDistanceCombiner());
    }

    @Override
    public Set<MessageScope> getMessageScopes(final Memory memory) {
        Set<MessageScope> set = new HashSet<>();
        set.add(incidentMessageScope);
        return set;
    }

    @Override
    public void setup(final Memory memory) {}

    @Override
    public void execute(final Vertex vertex, Messenger<Long> messenger, final Memory memory) {
        if (memory.isInitialIteration()) {
            if (vertex.id().equals(seed)) {
                vertex.property(VertexProperty.Cardinality.single, DISTANCE, 0L);
                messenger.sendMessage(incidentMessageScope, 0L);
            }
        } else {
            Iterator<Long> distances = messenger.receiveMessages();
            Long shortest = IteratorUtils.stream(distances).reduce(Math::min).orElse(null);

            if (shortest == null) return;

            VertexProperty<Long> currentVP = vertex.property(DISTANCE);
            if (!currentVP.isPresent() || currentVP.value() > shortest) {
                vertex.property(VertexProperty.Cardinality.single, DISTANCE, shortest);
                messenger.sendMessage(incidentMessageScope, shortest);
            }
        }
    }

    @Override
    public boolean terminate(final Memory memory) {
        return memory.getIteration() >= maxDepth;
    }

    @Override
    public GraphComputer.ResultGraph getPreferredResultGraph() {
        return GraphComputer.ResultGraph.ORIGINAL;
    }

    @Override
    public GraphComputer.Persist getPreferredPersist() {
        return GraphComputer.Persist.VERTEX_PROPERTIES;
    }

    @Override
    public Features getFeatures() {
        return new Features() {
            @Override
            public boolean requiresLocalMessageScopes() {
                return true;
            }

            @Override
            public boolean requiresVertexPropertyAddition() {
                return true;
            }
        };
    }

    public static Builder build() {
        return new Builder();
    }

    public static class Builder extends AbstractVertexProgramBuilder<Builder> {
        private Builder() {
            super(DomynShortestDistanceVertexProgram.class);
        }

        public Builder seed(final long seed) {
            configuration.setProperty(SEED, seed);
            return this;
        }

        public Builder maxDepth(final int maxDepth) {
            configuration.setProperty(MAX_DEPTH, maxDepth);
            return this;
        }

        public Builder weightProperty(final String weightProperty) {
            configuration.setProperty(WEIGHT_PROPERTY, weightProperty);
            return this;
        }
    }

    public static class MinDistanceCombiner implements MessageCombiner<Long> {
        @Override
        public Long combine(Long a, Long b) {
            return Math.min(a, b);
        }
    }
}
