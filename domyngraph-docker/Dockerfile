# Copyright 2024 DomynGraph Authors
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#      http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

# --- Build stage: compile DomynGraph modules from source ---
FROM maven:3.9-eclipse-temurin-11 AS build
WORKDIR /src
COPY . .
RUN mvn package -pl domyngraph-procedures,domyngraph-algorithms,domyngraph-tenant -am \
    -DskipTests -Drat.skip=true -Dcheckstyle.skip=true \
    -Denforcer.skip=true -Dcyclonedx.skip=true -q

# --- Runtime: pinned JanusGraph 1.1.0 ---
FROM janusgraph/janusgraph:1.1.0

COPY --from=build /src/domyngraph-procedures/target/domyngraph-procedures-*.jar /opt/janusgraph/lib/
COPY --from=build /src/domyngraph-algorithms/target/domyngraph-algorithms-*.jar /opt/janusgraph/lib/
COPY --from=build /src/domyngraph-tenant/target/domyngraph-tenant-*.jar /opt/janusgraph/lib/
