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

import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.stream.Collectors;

public class ProcedureDefinition {

    private final String name;
    private final String version;
    private final List<ParameterDef> inputs;
    private final String outputType;
    private final boolean deterministic;
    private final String description;

    private ProcedureDefinition(Builder builder) {
        this.name = builder.name;
        this.version = builder.version;
        this.inputs = Collections.unmodifiableList(new ArrayList<>(builder.inputs));
        this.outputType = builder.outputType;
        this.deterministic = builder.deterministic;
        this.description = builder.description;
    }

    public String getName() {
        return name;
    }

    public String getVersion() {
        return version;
    }

    public List<ParameterDef> getInputs() {
        return inputs;
    }

    public String getOutputType() {
        return outputType;
    }

    public boolean isDeterministic() {
        return deterministic;
    }

    public String getDescription() {
        return description;
    }

    public Map<String, Object> toMap() {
        Map<String, Object> map = new LinkedHashMap<>();
        map.put("name", name);
        map.put("version", version);
        map.put("description", description);
        map.put("deterministic", deterministic);
        map.put("outputType", outputType);
        map.put("inputs", inputs.stream().map(ParameterDef::toMap).collect(Collectors.toList()));
        return map;
    }

    public static Builder builder(String name) {
        return new Builder(name);
    }

    public static class Builder {
        private final String name;
        private String version = "1.0";
        private final List<ParameterDef> inputs = new ArrayList<>();
        private String outputType = "object";
        private boolean deterministic = true;
        private String description = "";

        private Builder(String name) {
            this.name = name;
        }

        public Builder version(String version) {
            this.version = version;
            return this;
        }

        public Builder addInput(ParameterDef param) {
            this.inputs.add(param);
            return this;
        }

        public Builder outputType(String outputType) {
            this.outputType = outputType;
            return this;
        }

        public Builder deterministic(boolean deterministic) {
            this.deterministic = deterministic;
            return this;
        }

        public Builder description(String description) {
            this.description = description;
            return this;
        }

        public ProcedureDefinition build() {
            return new ProcedureDefinition(this);
        }
    }
}
