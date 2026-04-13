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
 * Connected Components via label propagation for DomynGraph.
 * Each vertex propagates its component ID (initially its own vertex ID as long)
 * to neighbors. Vertices adopt the minimum component ID they receive.
 * Converges when no vertex changes its component ID.
 */
public class ConnectedComponentsVertexProgram extends StaticVertexProgram<Long> {

    private static final Logger logger = LoggerFactory.getLogger(ConnectedComponentsVertexProgram.class);

    public static final String COMPONENT = "domyn.connectedComponents.component";
    private static final String MAX_ITERATIONS = "domyn.connectedComponents.maxIterations";
    private static final String VOTE_TO_HALT = "domyn.connectedComponents.voteToHalt";

    private int maxIterations;

    private final MessageScope.Local<Long> scope = MessageScope.Local.of(__::bothE);

    private static final Set<VertexComputeKey> COMPUTE_KEYS = ImmutableSet.of(
            VertexComputeKey.of(COMPONENT, false));

    @Override
    public void loadState(final Graph graph, final Configuration configuration) {
        maxIterations = configuration.getInt(MAX_ITERATIONS, 30);
    }

    @Override
    public void storeState(final Configuration configuration) {
        configuration.setProperty(VERTEX_PROGRAM, ConnectedComponentsVertexProgram.class.getName());
        configuration.setProperty(MAX_ITERATIONS, maxIterations);
    }

    @Override
    public Set<VertexComputeKey> getVertexComputeKeys() {
        return COMPUTE_KEYS;
    }

    @Override
    public Set<MemoryComputeKey> getMemoryComputeKeys() {
        return ImmutableSet.of(
                MemoryComputeKey.of(VOTE_TO_HALT, Operator.and, true, false));
    }

    @Override
    public Optional<MessageCombiner<Long>> getMessageCombiner() {
        return Optional.of(new MinCombiner());
    }

    @Override
    public void setup(final Memory memory) {
        memory.set(VOTE_TO_HALT, true);
    }

    @Override
    public void execute(final Vertex vertex, Messenger<Long> messenger, final Memory memory) {
        if (memory.isInitialIteration()) {
            long myId = ((Number) vertex.id()).longValue();
            vertex.property(VertexProperty.Cardinality.single, COMPONENT, myId);
            messenger.sendMessage(scope, myId);
            memory.add(VOTE_TO_HALT, false);
        } else {
            Long currentComponent = vertex.<Long>value(COMPONENT);
            Long minReceived = IteratorUtils.stream(messenger.receiveMessages())
                    .reduce(Math::min)
                    .orElse(currentComponent);

            long newComponent = Math.min(currentComponent, minReceived);

            if (newComponent < currentComponent) {
                vertex.property(VertexProperty.Cardinality.single, COMPONENT, newComponent);
                messenger.sendMessage(scope, newComponent);
                memory.add(VOTE_TO_HALT, false);
            }
        }
    }

    @Override
    public boolean terminate(final Memory memory) {
        boolean halt = memory.<Boolean>get(VOTE_TO_HALT);
        if (halt) {
            logger.info("ConnectedComponents converged at iteration {}", memory.getIteration());
            return true;
        }
        if (memory.getIteration() >= maxIterations) {
            logger.info("ConnectedComponents terminated: max iterations ({}) reached", maxIterations);
            return true;
        }
        memory.set(VOTE_TO_HALT, true);
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
            super(ConnectedComponentsVertexProgram.class);
        }

        public Builder maxIterations(final int maxIterations) {
            configuration.setProperty(MAX_ITERATIONS, maxIterations);
            return this;
        }
    }

    public static class MinCombiner implements MessageCombiner<Long> {
        @Override
        public Long combine(Long a, Long b) {
            return Math.min(a, b);
        }
    }
}
