// =============================================================================
// DomynGraph Engine — Schema Bootstrap Script
// Loaded by Gremlin Server on startup via ScriptFileGremlinPlugin
// =============================================================================

def initDomynGraphSchema(graph) {
    mgmt = graph.openManagement()

    // -- Vertex Labels --
    if (!mgmt.containsVertexLabel('Entity'))    mgmt.makeVertexLabel('Entity').make()
    if (!mgmt.containsVertexLabel('Chunk'))     mgmt.makeVertexLabel('Chunk').make()
    if (!mgmt.containsVertexLabel('Document'))  mgmt.makeVertexLabel('Document').make()
    if (!mgmt.containsVertexLabel('Concept'))   mgmt.makeVertexLabel('Concept').make()

    // -- Edge Labels --
    if (!mgmt.containsEdgeLabel('RELATION'))    mgmt.makeEdgeLabel('RELATION').multiplicity(MULTI).make()
    if (!mgmt.containsEdgeLabel('CONTAINS'))    mgmt.makeEdgeLabel('CONTAINS').multiplicity(MULTI).make()
    if (!mgmt.containsEdgeLabel('REFERENCES'))  mgmt.makeEdgeLabel('REFERENCES').multiplicity(MULTI).make()
    if (!mgmt.containsEdgeLabel('SIMILAR_TO'))  mgmt.makeEdgeLabel('SIMILAR_TO').multiplicity(MULTI).make()

    // -- Property Keys --
    name        = mgmt.containsPropertyKey('name')        ? mgmt.getPropertyKey('name')        : mgmt.makePropertyKey('name').dataType(String.class).make()
    type        = mgmt.containsPropertyKey('type')        ? mgmt.getPropertyKey('type')        : mgmt.makePropertyKey('type').dataType(String.class).make()
    tenantId    = mgmt.containsPropertyKey('tenant_id')   ? mgmt.getPropertyKey('tenant_id')   : mgmt.makePropertyKey('tenant_id').dataType(String.class).make()
    externalId  = mgmt.containsPropertyKey('external_id') ? mgmt.getPropertyKey('external_id') : mgmt.makePropertyKey('external_id').dataType(String.class).make()
    createdAt   = mgmt.containsPropertyKey('created_at')  ? mgmt.getPropertyKey('created_at')  : mgmt.makePropertyKey('created_at').dataType(Long.class).make()
    description = mgmt.containsPropertyKey('description') ? mgmt.getPropertyKey('description') : mgmt.makePropertyKey('description').dataType(String.class).make()
    weight      = mgmt.containsPropertyKey('weight')      ? mgmt.getPropertyKey('weight')      : mgmt.makePropertyKey('weight').dataType(Double.class).make()
    metaType    = mgmt.containsPropertyKey('__type')      ? mgmt.getPropertyKey('__type')       : mgmt.makePropertyKey('__type').dataType(String.class).make()

    if (!mgmt.containsPropertyKey('embedding'))      mgmt.makePropertyKey('embedding').dataType(byte[].class).make()
    if (!mgmt.containsPropertyKey('metadata'))        mgmt.makePropertyKey('metadata').dataType(String.class).make()
    if (!mgmt.containsPropertyKey('schema_version'))  mgmt.makePropertyKey('schema_version').dataType(Integer.class).make()

    // -- Composite Indexes (Cassandra, exact-match) --
    if (!mgmt.containsGraphIndex('byExternalId'))
        mgmt.buildIndex('byExternalId', Vertex.class).addKey(externalId).unique().buildCompositeIndex()

    if (!mgmt.containsGraphIndex('byTenantId'))
        mgmt.buildIndex('byTenantId', Vertex.class).addKey(tenantId).buildCompositeIndex()

    if (!mgmt.containsGraphIndex('byName'))
        mgmt.buildIndex('byName', Vertex.class).addKey(name).buildCompositeIndex()

    if (!mgmt.containsGraphIndex('byType'))
        mgmt.buildIndex('byType', Vertex.class).addKey(type).buildCompositeIndex()

    if (!mgmt.containsGraphIndex('byTenantAndType'))
        mgmt.buildIndex('byTenantAndType', Vertex.class).addKey(tenantId).addKey(type).buildCompositeIndex()

    if (!mgmt.containsGraphIndex('byMetaType'))
        mgmt.buildIndex('byMetaType', Vertex.class).addKey(metaType).buildCompositeIndex()

    // -- Mixed Index (Elasticsearch, full-text + filtering) --
    if (!mgmt.containsGraphIndex('search'))
        mgmt.buildIndex('search', Vertex.class)
            .addKey(name, Mapping.TEXT.asParameter())
            .addKey(type, Mapping.STRING.asParameter())
            .addKey(tenantId, Mapping.STRING.asParameter())
            .addKey(externalId, Mapping.STRING.asParameter())
            .addKey(description, Mapping.TEXT.asParameter())
            .addKey(createdAt, Mapping.DEFAULT.asParameter())
            .buildMixedIndex('search')

    mgmt.commit()

    // -- Schema Meta Vertex --
    if (!graph.traversal().V().has('__type', '__schema_meta').hasNext()) {
        v = graph.addVertex()
        v.property('__type', '__schema_meta')
        v.property('schema_version', 1)
        graph.tx().commit()
    }
}

// Execute on the default graph bound by Gremlin Server
initDomynGraphSchema(graph)
