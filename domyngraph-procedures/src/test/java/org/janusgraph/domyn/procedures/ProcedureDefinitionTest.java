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

import org.junit.jupiter.api.Test;

import java.util.Map;

import static org.junit.jupiter.api.Assertions.*;

class ProcedureDefinitionTest {

    @Test
    void builderShouldSetAllFields() {
        ProcedureDefinition def = ProcedureDefinition.builder("myProc")
                .version("2.0")
                .description("A test procedure")
                .addInput(ParameterDef.required("arg1", "String", "First argument"))
                .addInput(ParameterDef.optional("arg2", "Integer", "Second argument"))
                .outputType("vertex[]")
                .deterministic(true)
                .build();

        assertEquals("myProc", def.getName());
        assertEquals("2.0", def.getVersion());
        assertEquals("A test procedure", def.getDescription());
        assertEquals("vertex[]", def.getOutputType());
        assertTrue(def.isDeterministic());
        assertEquals(2, def.getInputs().size());
    }

    @Test
    void toMapShouldContainAllKeys() {
        ProcedureDefinition def = ProcedureDefinition.builder("test")
                .version("1.0")
                .description("desc")
                .outputType("map")
                .build();

        Map<String, Object> map = def.toMap();
        assertEquals("test", map.get("name"));
        assertEquals("1.0", map.get("version"));
        assertEquals("desc", map.get("description"));
        assertEquals("map", map.get("outputType"));
    }

    @Test
    void parameterDefRequiredFlag() {
        ParameterDef req = ParameterDef.required("x", "String", "desc");
        ParameterDef opt = ParameterDef.optional("y", "Integer", "desc");

        assertTrue(req.isRequired());
        assertFalse(opt.isRequired());
    }
}
