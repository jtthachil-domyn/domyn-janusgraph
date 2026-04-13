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

/**
 * DomynGraph Algorithm Engine — GDS-equivalent for JanusGraph.
 *
 * Provides resource-controlled graph algorithms (PageRank, shortest distance,
 * connected components, BFS) running on FulgoraGraphComputer with enforced
 * timeout, memory, and iteration limits.
 */
package org.janusgraph.domyn.algorithms;
