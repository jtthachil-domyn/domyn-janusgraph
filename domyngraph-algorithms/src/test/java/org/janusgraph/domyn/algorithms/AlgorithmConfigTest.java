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

import org.junit.jupiter.api.Test;

import static org.junit.jupiter.api.Assertions.*;

class AlgorithmConfigTest {

    @Test
    void defaultsShouldHaveSaneValues() {
        AlgorithmConfig config = AlgorithmConfig.defaults();
        assertEquals(100, config.getMaxIterations());
        assertEquals(300_000, config.getTimeoutMs());
        assertEquals(2048, config.getMemoryLimitMb());
        assertEquals(4, config.getWorkerThreads());
    }

    @Test
    void builderShouldOverrideDefaults() {
        AlgorithmConfig config = AlgorithmConfig.builder()
                .maxIterations(50)
                .timeoutMs(60000)
                .memoryLimitMb(1024)
                .workerThreads(8)
                .build();

        assertEquals(50, config.getMaxIterations());
        assertEquals(60000, config.getTimeoutMs());
        assertEquals(1024, config.getMemoryLimitMb());
        assertEquals(8, config.getWorkerThreads());
    }

    @Test
    void builderShouldRejectInvalidMaxIterations() {
        assertThrows(IllegalArgumentException.class, () ->
                AlgorithmConfig.builder().maxIterations(0).build());
    }

    @Test
    void builderShouldRejectInvalidTimeout() {
        assertThrows(IllegalArgumentException.class, () ->
                AlgorithmConfig.builder().timeoutMs(500).build());
    }

    @Test
    void builderShouldRejectInvalidMemoryLimit() {
        assertThrows(IllegalArgumentException.class, () ->
                AlgorithmConfig.builder().memoryLimitMb(32).build());
    }

    @Test
    void builderShouldRejectInvalidWorkerThreads() {
        assertThrows(IllegalArgumentException.class, () ->
                AlgorithmConfig.builder().workerThreads(0).build());
    }

    @Test
    void toStringShouldContainAllFields() {
        AlgorithmConfig config = AlgorithmConfig.builder()
                .maxIterations(20)
                .timeoutMs(5000)
                .memoryLimitMb(512)
                .workerThreads(2)
                .build();
        String str = config.toString();
        assertTrue(str.contains("20"));
        assertTrue(str.contains("5000"));
        assertTrue(str.contains("512"));
        assertTrue(str.contains("2"));
    }
}
