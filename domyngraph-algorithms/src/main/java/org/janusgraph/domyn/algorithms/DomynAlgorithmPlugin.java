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

import org.apache.tinkerpop.gremlin.jsr223.AbstractGremlinPlugin;
import org.apache.tinkerpop.gremlin.jsr223.DefaultImportCustomizer;

public class DomynAlgorithmPlugin extends AbstractGremlinPlugin {

    private static final String NAME = "org.janusgraph.domyn.algorithms";

    private static final DomynAlgorithmPlugin INSTANCE = new DomynAlgorithmPlugin();

    public DomynAlgorithmPlugin() {
        super(NAME, DefaultImportCustomizer.build()
                .addClassImports(
                        AlgorithmConfig.class,
                        AlgorithmConfig.Builder.class,
                        AlgorithmResult.class,
                        AlgorithmResult.Status.class,
                        AlgorithmResourceManager.class,
                        DomynPageRankVertexProgram.class,
                        DomynPageRankVertexProgram.Builder.class,
                        DomynShortestDistanceVertexProgram.class,
                        DomynShortestDistanceVertexProgram.Builder.class,
                        ConnectedComponentsVertexProgram.class,
                        ConnectedComponentsVertexProgram.Builder.class,
                        BFSVertexProgram.class,
                        BFSVertexProgram.Builder.class
                )
                .create());
    }

    public static DomynAlgorithmPlugin instance() {
        return INSTANCE;
    }
}
