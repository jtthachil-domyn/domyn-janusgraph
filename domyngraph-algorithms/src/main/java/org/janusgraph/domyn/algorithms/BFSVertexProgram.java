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

import com.google.common.collect.ImmutableSet;
import org.apache.commons.configuration2.Configuration;
import org.apache.tinkerpop.gremlin.process.computer.GraphComputer;
import org.apache.tinkerpop.gremlin.process.computer.Memory;
import org.apache.tinkerpop.gremlin.process.computer.MemoryComputeKey;
import org.apache.tinkerpop.gremlin.process.computer.MessageCombiner;
import org.apache.tinkerpop.gremlin.process.computer.MessageScope;
import org.apache.tinkerpop.gremlin.process.computer.Messenger;
import org.apache.tinkerpop.gremlin.process.computer.VertexComputeKey;
import org.apache.tinkerpop.gremlin.process.computer.util.AbstractVertexProgramBuilder;
import org.apache.tinkerpop.gremlin.process.computer.util.StaticVertexProgram;
import org.apache.tinkerpop.gremlin.process.traversal.Operator;
import org.apache.tinkerpop.gremlin.process.traversal.dsl.graph.__;
import org.apache.tinkerpop.gremlin.structure.Graph;
import org.apache.tinkerpop.gremlin.structure.Vertex;
import org.apache.tinkerpop.gremlin.structure.VertexProperty;
import org.apache.tinkerpop.gremlin.util.iterator.IteratorUtils;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import java.util.Optional;
import java.util.Set;

/**
 * Breadth-First Search from a seed vertex for DomynGraph.
 * Assigns each reachable vertex its BFS depth (hop distance from seed).
 * Vertices not reachable from seed will not have the depth property set.
 */
public class BFSVertexProgram extends StaticVertexProgram<Integer> {

    private static final Logger logger = LoggerFactory.getLogger(BFSVertexProgram.class);

    public static final String DEPTH = "domyn.bfs.depth";
    private static final String MAX_DEPTH = "domyn.bfs.maxDepth";
    private static final String SEED = "domyn.bfs.seed";
    private static final String ACTIVE_COUNT = "domyn.bfs.activeCount";

    private int maxDepth;
    private long seed;

    private final MessageScope.Local<Integer> scope = MessageScope.Local.of(__::bothE);

    private static final Set<VertexComputeKey> COMPUTE_KEYS = ImmutableSet.of(
            VertexComputeKey.of(DEPTH, false));

    @Override
    public void loadState(final Graph graph, final Configuration configuration) {
        maxDepth = configuration.getInt(MAX_DEPTH, 10);
        seed = configuration.getLong(SEED);
    }

    @Override
    public void storeState(final Configuration configuration) {
        configuration.setProperty(VERTEX_PROGRAM, BFSVertexProgram.class.getName());
        configuration.setProperty(MAX_DEPTH, maxDepth);
        configuration.setProperty(SEED, seed);
    }

    @Override
    public Set<VertexComputeKey> getVertexComputeKeys() {
        return COMPUTE_KEYS;
    }

    @Override
    public Set<MemoryComputeKey> getMemoryComputeKeys() {
        return ImmutableSet.of(
                MemoryComputeKey.of(ACTIVE_COUNT, Operator.sum, true, false));
    }

    @Override
    public Optional<MessageCombiner<Integer>> getMessageCombiner() {
        return Optional.of(new MinIntCombiner());
    }

    @Override
    public void setup(final Memory memory) {
        memory.set(ACTIVE_COUNT, 0L);
    }

    @Override
    public void execute(final Vertex vertex, Messenger<Integer> messenger, final Memory memory) {
        if (memory.isInitialIteration()) {
            if (vertex.id().equals(seed)) {
                vertex.property(VertexProperty.Cardinality.single, DEPTH, 0);
                messenger.sendMessage(scope, 1);
                memory.add(ACTIVE_COUNT, 1L);
            }
        } else {
            Integer minDepth = IteratorUtils.stream(messenger.receiveMessages())
                    .reduce(Math::min)
                    .orElse(null);

            if (minDepth == null) return;

            VertexProperty<Integer> currentDepth = vertex.property(DEPTH);
            if (!currentDepth.isPresent()) {
                vertex.property(VertexProperty.Cardinality.single, DEPTH, minDepth);
                if (minDepth < maxDepth) {
                    messenger.sendMessage(scope, minDepth + 1);
                    memory.add(ACTIVE_COUNT, 1L);
                }
            }
        }
    }

    @Override
    public boolean terminate(final Memory memory) {
        long active = memory.<Long>get(ACTIVE_COUNT);
        if (active == 0 && memory.getIteration() > 0) {
            logger.info("BFS completed at iteration {} (no more active vertices)",
                    memory.getIteration());
            return true;
        }
        if (memory.getIteration() >= maxDepth) {
            logger.info("BFS terminated: max depth ({}) reached", maxDepth);
            return true;
        }
        memory.set(ACTIVE_COUNT, 0L);
        return false;
    }

    @Override
    public Set<MessageScope> getMessageScopes(final Memory memory) {
        return ImmutableSet.of(scope);
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
            super(BFSVertexProgram.class);
        }

        public Builder seed(final long seed) {
            configuration.setProperty(SEED, seed);
            return this;
        }

        public Builder maxDepth(final int maxDepth) {
            configuration.setProperty(MAX_DEPTH, maxDepth);
            return this;
        }
    }

    public static class MinIntCombiner implements MessageCombiner<Integer> {
        @Override
        public Integer combine(Integer a, Integer b) {
            return Math.min(a, b);
        }
    }
}
