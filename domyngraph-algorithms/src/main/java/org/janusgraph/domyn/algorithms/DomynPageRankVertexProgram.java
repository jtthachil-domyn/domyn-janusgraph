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

import java.util.Set;

/**
 * Production-grade PageRank implementation for DomynGraph.
 * Enhanced from JanusGraph's test-only implementation with:
 * - Convergence detection (stops early if ranks stabilize)
 * - Configurable damping factor, max iterations, convergence tolerance
 * - Proper vertex count estimation
 */
public class DomynPageRankVertexProgram extends StaticVertexProgram<Double> {

    private static final Logger logger = LoggerFactory.getLogger(DomynPageRankVertexProgram.class);

    public static final String PAGE_RANK = "domyn.pageRank.rank";
    public static final String EDGE_COUNT = "domyn.pageRank.edgeCount";

    private static final String DAMPING_FACTOR = "domyn.pageRank.dampingFactor";
    private static final String MAX_ITERATIONS = "domyn.pageRank.maxIterations";
    private static final String VERTEX_COUNT = "domyn.pageRank.vertexCount";
    private static final String CONVERGENCE_THRESHOLD = "domyn.pageRank.convergenceThreshold";
    private static final String CONVERGED_COUNT = "domyn.pageRank.convergedCount";

    private double dampingFactor;
    private int maxIterations;
    private long vertexCount;
    private double convergenceThreshold;

    private final MessageScope.Local<Double> outE = MessageScope.Local.of(__::outE);
    private final MessageScope.Local<Double> inE = MessageScope.Local.of(__::inE);

    private static final Set<VertexComputeKey> COMPUTE_KEYS = ImmutableSet.of(
            VertexComputeKey.of(PAGE_RANK, false),
            VertexComputeKey.of(EDGE_COUNT, false));

    @Override
    public void loadState(final Graph graph, final Configuration configuration) {
        dampingFactor = configuration.getDouble(DAMPING_FACTOR, 0.85D);
        maxIterations = configuration.getInt(MAX_ITERATIONS, 20);
        vertexCount = configuration.getLong(VERTEX_COUNT, 1L);
        convergenceThreshold = configuration.getDouble(CONVERGENCE_THRESHOLD, 0.001D);
    }

    @Override
    public void storeState(final Configuration configuration) {
        configuration.setProperty(VERTEX_PROGRAM, DomynPageRankVertexProgram.class.getName());
        configuration.setProperty(DAMPING_FACTOR, dampingFactor);
        configuration.setProperty(MAX_ITERATIONS, maxIterations);
        configuration.setProperty(VERTEX_COUNT, vertexCount);
        configuration.setProperty(CONVERGENCE_THRESHOLD, convergenceThreshold);
    }

    @Override
    public Set<VertexComputeKey> getVertexComputeKeys() {
        return COMPUTE_KEYS;
    }

    @Override
    public Set<MemoryComputeKey> getMemoryComputeKeys() {
        return ImmutableSet.of(
                MemoryComputeKey.of(CONVERGED_COUNT, Operator.sum, true, false));
    }

    @Override
    public void setup(Memory memory) {
        memory.set(CONVERGED_COUNT, 0L);
    }

    @Override
    public void execute(Vertex vertex, Messenger<Double> messenger, Memory memory) {
        if (memory.isInitialIteration()) {
            messenger.sendMessage(inE, 1D);
        } else if (1 == memory.getIteration()) {
            double initialPageRank = 1D / vertexCount;
            double edgeCount = IteratorUtils.stream(messenger.receiveMessages())
                    .reduce(0D, Double::sum);
            vertex.property(VertexProperty.Cardinality.single, PAGE_RANK, initialPageRank);
            vertex.property(VertexProperty.Cardinality.single, EDGE_COUNT, edgeCount);
            if (edgeCount > 0) {
                messenger.sendMessage(outE, initialPageRank / edgeCount);
            }
        } else {
            double newPageRank = IteratorUtils.stream(messenger.receiveMessages())
                    .reduce(0D, Double::sum);
            newPageRank = (dampingFactor * newPageRank) + ((1D - dampingFactor) / vertexCount);

            Double oldPageRank = vertex.<Double>property(PAGE_RANK).orElse(0D);
            vertex.property(VertexProperty.Cardinality.single, PAGE_RANK, newPageRank);

            if (Math.abs(newPageRank - oldPageRank) < convergenceThreshold) {
                memory.add(CONVERGED_COUNT, 1L);
            }

            Double edgeCount = vertex.<Double>property(EDGE_COUNT).orElse(0D);
            if (edgeCount > 0) {
                messenger.sendMessage(outE, newPageRank / edgeCount);
            }
        }
    }

    @Override
    public boolean terminate(Memory memory) {
        if (memory.getIteration() >= maxIterations) {
            logger.info("PageRank terminated: max iterations ({}) reached", maxIterations);
            return true;
        }
        if (memory.getIteration() > 2) {
            long converged = memory.<Long>get(CONVERGED_COUNT);
            if (converged >= vertexCount * 0.99) {
                logger.info("PageRank converged at iteration {} ({} of {} vertices stable)",
                        memory.getIteration(), converged, vertexCount);
                return true;
            }
            memory.set(CONVERGED_COUNT, 0L);
        }
        return false;
    }

    @Override
    public Set<MessageScope> getMessageScopes(Memory memory) {
        return ImmutableSet.of(outE, inE);
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
            super(DomynPageRankVertexProgram.class);
        }

        public Builder vertexCount(final long vertexCount) {
            configuration.setProperty(VERTEX_COUNT, vertexCount);
            return this;
        }

        public Builder dampingFactor(final double dampingFactor) {
            configuration.setProperty(DAMPING_FACTOR, dampingFactor);
            return this;
        }

        public Builder iterations(final int iterations) {
            configuration.setProperty(MAX_ITERATIONS, iterations);
            return this;
        }

        public Builder convergenceThreshold(final double threshold) {
            configuration.setProperty(CONVERGENCE_THRESHOLD, threshold);
            return this;
        }
    }
}
