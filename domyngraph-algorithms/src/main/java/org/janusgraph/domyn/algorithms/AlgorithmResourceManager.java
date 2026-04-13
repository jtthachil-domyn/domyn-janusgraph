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

import org.apache.tinkerpop.gremlin.process.computer.ComputerResult;
import org.apache.tinkerpop.gremlin.process.computer.GraphComputer;
import org.apache.tinkerpop.gremlin.process.computer.VertexProgram;
import org.janusgraph.core.JanusGraph;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import java.util.concurrent.ExecutionException;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;

public class AlgorithmResourceManager {

    private static final Logger logger = LoggerFactory.getLogger(AlgorithmResourceManager.class);

    private static final AlgorithmResourceManager INSTANCE = new AlgorithmResourceManager();

    private AlgorithmResourceManager() {}

    public static AlgorithmResourceManager getInstance() {
        return INSTANCE;
    }

    public AlgorithmResult<ComputerResult> execute(JanusGraph graph,
                                                    VertexProgram<?> program,
                                                    AlgorithmConfig config) {
        String algorithmName = program.getClass().getSimpleName();
        logger.info("ALGO_START algorithm={} config={}", algorithmName, config);

        validateResources(config);

        long startTime = System.currentTimeMillis();
        Future<ComputerResult> future = null;

        try {
            GraphComputer computer = graph.compute()
                    .workers(config.getWorkerThreads())
                    .program(program);

            future = computer.submit();

            ComputerResult result = future.get(config.getTimeoutMs(), TimeUnit.MILLISECONDS);

            long elapsed = System.currentTimeMillis() - startTime;
            logger.info("ALGO_END algorithm={} status=SUCCESS elapsed={}ms", algorithmName, elapsed);
            return AlgorithmResult.success(result, elapsed, algorithmName);

        } catch (TimeoutException e) {
            long elapsed = System.currentTimeMillis() - startTime;
            logger.warn("ALGO_END algorithm={} status=TIMEOUT elapsed={}ms limit={}ms",
                    algorithmName, elapsed, config.getTimeoutMs());

            if (future != null) {
                future.cancel(true);
                logger.info("Cancelled timed-out algorithm: {}", algorithmName);
            }

            return AlgorithmResult.timeout(elapsed, algorithmName);

        } catch (ExecutionException e) {
            long elapsed = System.currentTimeMillis() - startTime;
            String msg = e.getCause() != null ? e.getCause().getMessage() : e.getMessage();
            logger.error("ALGO_END algorithm={} status=ERROR elapsed={}ms error={}",
                    algorithmName, elapsed, msg);
            return AlgorithmResult.error(msg, elapsed, algorithmName);

        } catch (InterruptedException e) {
            long elapsed = System.currentTimeMillis() - startTime;
            Thread.currentThread().interrupt();
            logger.error("ALGO_END algorithm={} status=INTERRUPTED elapsed={}ms",
                    algorithmName, elapsed);
            return AlgorithmResult.error("Algorithm interrupted", elapsed, algorithmName);

        } catch (Exception e) {
            long elapsed = System.currentTimeMillis() - startTime;
            logger.error("ALGO_END algorithm={} status=ERROR elapsed={}ms error={}",
                    algorithmName, elapsed, e.getMessage(), e);
            return AlgorithmResult.error(e.getMessage(), elapsed, algorithmName);
        }
    }

    public AlgorithmResult<ComputerResult> execute(JanusGraph graph,
                                                    VertexProgram<?> program) {
        return execute(graph, program, AlgorithmConfig.defaults());
    }

    private void validateResources(AlgorithmConfig config) {
        Runtime runtime = Runtime.getRuntime();
        long freeMemoryMb = runtime.freeMemory() / (1024 * 1024);
        long maxMemoryMb = runtime.maxMemory() / (1024 * 1024);

        if (config.getMemoryLimitMb() > maxMemoryMb) {
            logger.warn("Requested memory limit {}MB exceeds JVM max {}MB — execution may fail",
                    config.getMemoryLimitMb(), maxMemoryMb);
        }

        if (freeMemoryMb < config.getMemoryLimitMb() / 4) {
            logger.warn("Low free memory: {}MB free, algorithm requests {}MB limit",
                    freeMemoryMb, config.getMemoryLimitMb());
        }
    }
}
