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

import java.util.Map;

import static org.junit.jupiter.api.Assertions.*;

class AlgorithmResultTest {

    @Test
    void successResultShouldHaveCorrectState() {
        AlgorithmResult<String> result = AlgorithmResult.success("data", 100L, "TestAlgo");
        assertTrue(result.isSuccess());
        assertEquals("data", result.getResult());
        assertEquals(100L, result.getElapsedMs());
        assertEquals("TestAlgo", result.getAlgorithmName());
        assertNull(result.getErrorMessage());
    }

    @Test
    void timeoutResultShouldHaveCorrectState() {
        AlgorithmResult<String> result = AlgorithmResult.timeout(5000L, "SlowAlgo");
        assertFalse(result.isSuccess());
        assertEquals(AlgorithmResult.Status.TIMEOUT, result.getStatus());
        assertEquals(5000L, result.getElapsedMs());
        assertNotNull(result.getErrorMessage());
    }

    @Test
    void errorResultShouldHaveCorrectState() {
        AlgorithmResult<String> result = AlgorithmResult.error("OOM", 200L, "BigAlgo");
        assertFalse(result.isSuccess());
        assertEquals(AlgorithmResult.Status.ERROR, result.getStatus());
        assertEquals("OOM", result.getErrorMessage());
    }

    @Test
    void getResultOnNonSuccessShouldThrow() {
        AlgorithmResult<String> result = AlgorithmResult.error("fail", 100L, "Algo");
        assertThrows(IllegalStateException.class, result::getResult);
    }

    @Test
    void getResultOrNullOnErrorShouldReturnNull() {
        AlgorithmResult<String> result = AlgorithmResult.error("fail", 100L, "Algo");
        assertNull(result.getResultOrNull());
    }

    @Test
    void toMapShouldContainRequiredKeys() {
        AlgorithmResult<String> success = AlgorithmResult.success("ok", 50L, "PageRank");
        Map<String, Object> map = success.toMap();
        assertEquals("PageRank", map.get("algorithm"));
        assertEquals("SUCCESS", map.get("status"));
        assertEquals(50L, map.get("elapsedMs"));
        assertFalse(map.containsKey("error"));

        AlgorithmResult<String> error = AlgorithmResult.error("bad", 100L, "BFS");
        Map<String, Object> errMap = error.toMap();
        assertEquals("bad", errMap.get("error"));
    }
}
