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

import java.util.LinkedHashMap;
import java.util.Map;

public class ParameterDef {

    private final String name;
    private final String type;
    private final boolean required;
    private final String description;

    public ParameterDef(String name, String type, boolean required, String description) {
        this.name = name;
        this.type = type;
        this.required = required;
        this.description = description;
    }

    public static ParameterDef required(String name, String type, String description) {
        return new ParameterDef(name, type, true, description);
    }

    public static ParameterDef optional(String name, String type, String description) {
        return new ParameterDef(name, type, false, description);
    }

    public String getName() {
        return name;
    }

    public String getType() {
        return type;
    }

    public boolean isRequired() {
        return required;
    }

    public String getDescription() {
        return description;
    }

    public Map<String, Object> toMap() {
        Map<String, Object> map = new LinkedHashMap<>();
        map.put("name", name);
        map.put("type", type);
        map.put("required", required);
        map.put("description", description);
        return map;
    }
}
