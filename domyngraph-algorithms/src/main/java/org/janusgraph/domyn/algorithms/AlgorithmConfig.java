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

public class AlgorithmConfig {

    private int maxIterations;
    private long timeoutMs;
    private long memoryLimitMb;
    private int workerThreads;

    private AlgorithmConfig(Builder builder) {
        this.maxIterations = builder.maxIterations;
        this.timeoutMs = builder.timeoutMs;
        this.memoryLimitMb = builder.memoryLimitMb;
        this.workerThreads = builder.workerThreads;
    }

    public static AlgorithmConfig defaults() {
        return builder().build();
    }

    public int getMaxIterations() {
        return maxIterations;
    }

    public long getTimeoutMs() {
        return timeoutMs;
    }

    public long getMemoryLimitMb() {
        return memoryLimitMb;
    }

    public int getWorkerThreads() {
        return workerThreads;
    }

    public static Builder builder() {
        return new Builder();
    }

    @Override
    public String toString() {
        return String.format("AlgorithmConfig{maxIter=%d, timeout=%dms, memLimit=%dMB, threads=%d}",
                maxIterations, timeoutMs, memoryLimitMb, workerThreads);
    }

    public static class Builder {
        private int maxIterations = 100;
        private long timeoutMs = 300_000;       // 5 minutes
        private long memoryLimitMb = 2048;      // 2 GB
        private int workerThreads = 4;

        public Builder maxIterations(int maxIterations) {
            if (maxIterations < 1) throw new IllegalArgumentException("maxIterations must be >= 1");
            this.maxIterations = maxIterations;
            return this;
        }

        public Builder timeoutMs(long timeoutMs) {
            if (timeoutMs < 1000) throw new IllegalArgumentException("timeoutMs must be >= 1000");
            this.timeoutMs = timeoutMs;
            return this;
        }

        public Builder memoryLimitMb(long memoryLimitMb) {
            if (memoryLimitMb < 64) throw new IllegalArgumentException("memoryLimitMb must be >= 64");
            this.memoryLimitMb = memoryLimitMb;
            return this;
        }

        public Builder workerThreads(int workerThreads) {
            if (workerThreads < 1) throw new IllegalArgumentException("workerThreads must be >= 1");
            this.workerThreads = workerThreads;
            return this;
        }

        public AlgorithmConfig build() {
            return new AlgorithmConfig(this);
        }
    }
}
