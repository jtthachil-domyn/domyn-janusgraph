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

import java.util.LinkedHashMap;
import java.util.Map;

public class AlgorithmResult<T> {

    public enum Status {
        SUCCESS,
        TIMEOUT,
        ERROR
    }

    private final Status status;
    private final T result;
    private final long elapsedMs;
    private final String algorithmName;
    private final String errorMessage;

    private AlgorithmResult(Status status, T result, long elapsedMs,
                            String algorithmName, String errorMessage) {
        this.status = status;
        this.result = result;
        this.elapsedMs = elapsedMs;
        this.algorithmName = algorithmName;
        this.errorMessage = errorMessage;
    }

    public static <T> AlgorithmResult<T> success(T result, long elapsedMs, String algorithmName) {
        return new AlgorithmResult<>(Status.SUCCESS, result, elapsedMs, algorithmName, null);
    }

    public static <T> AlgorithmResult<T> timeout(long elapsedMs, String algorithmName) {
        return new AlgorithmResult<>(Status.TIMEOUT, null, elapsedMs, algorithmName,
                "Algorithm timed out after " + elapsedMs + "ms");
    }

    public static <T> AlgorithmResult<T> error(String message, long elapsedMs, String algorithmName) {
        return new AlgorithmResult<>(Status.ERROR, null, elapsedMs, algorithmName, message);
    }

    public Status getStatus() {
        return status;
    }

    public boolean isSuccess() {
        return status == Status.SUCCESS;
    }

    public T getResult() {
        if (!isSuccess()) {
            throw new IllegalStateException("Cannot get result from " + status + " result: " + errorMessage);
        }
        return result;
    }

    public T getResultOrNull() {
        return result;
    }

    public long getElapsedMs() {
        return elapsedMs;
    }

    public String getAlgorithmName() {
        return algorithmName;
    }

    public String getErrorMessage() {
        return errorMessage;
    }

    public Map<String, Object> toMap() {
        Map<String, Object> map = new LinkedHashMap<>();
        map.put("algorithm", algorithmName);
        map.put("status", status.name());
        map.put("elapsedMs", elapsedMs);
        if (errorMessage != null) {
            map.put("error", errorMessage);
        }
        return map;
    }

    @Override
    public String toString() {
        return String.format("AlgorithmResult{algo=%s, status=%s, elapsed=%dms}",
                algorithmName, status, elapsedMs);
    }
}
