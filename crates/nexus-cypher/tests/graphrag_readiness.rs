//! GraphRAG benchmark readiness tests.
//!
//! Translates the actual Gremlin query patterns from domyn-janusgraph's
//! RAG pipeline into Cypher and tests whether Nexus can parse, plan,
//! and execute each one on a small FinReflectKG-shaped test graph.

use nexus_core::graph::Graph;
use nexus_core::types::Value;
use nexus_cypher::executor::{run_cypher, run_cypher_mut_in_memory};

fn build_finreflect_mini() -> Graph {
    let mut g = Graph::new(20, 20);

    let nvidia = g.add_vertex("Entity");
    let revenue = g.add_vertex("Entity");
    let q4_2024 = g.add_vertex("Entity");
    let datacenter = g.add_vertex("Entity");
    let gaming = g.add_vertex("Entity");
    let chunk1 = g.add_vertex("Chunk");
    let doc1 = g.add_vertex("Document");
    let concept1 = g.add_vertex("Concept");

    g.set_vertex_property(nvidia, "name", Value::String("nvidia".into()));
    g.set_vertex_property(nvidia, "type", Value::String("company".into()));
    g.set_vertex_property(
        nvidia,
        "external_id",
        Value::String("ext_nvidia_001".into()),
    );
    g.set_vertex_property(nvidia, "tenant_id", Value::String("NVDA".into()));

    g.set_vertex_property(revenue, "name", Value::String("revenue".into()));
    g.set_vertex_property(revenue, "type", Value::String("metric".into()));
    g.set_vertex_property(
        revenue,
        "external_id",
        Value::String("ext_revenue_001".into()),
    );
    g.set_vertex_property(revenue, "tenant_id", Value::String("NVDA".into()));

    g.set_vertex_property(q4_2024, "name", Value::String("q4 2024".into()));
    g.set_vertex_property(q4_2024, "type", Value::String("period".into()));
    g.set_vertex_property(q4_2024, "external_id", Value::String("ext_q4_001".into()));
    g.set_vertex_property(q4_2024, "tenant_id", Value::String("NVDA".into()));

    g.set_vertex_property(datacenter, "name", Value::String("datacenter".into()));
    g.set_vertex_property(datacenter, "type", Value::String("segment".into()));
    g.set_vertex_property(
        datacenter,
        "external_id",
        Value::String("ext_dc_001".into()),
    );
    g.set_vertex_property(datacenter, "tenant_id", Value::String("NVDA".into()));

    g.set_vertex_property(gaming, "name", Value::String("gaming".into()));
    g.set_vertex_property(gaming, "type", Value::String("segment".into()));
    g.set_vertex_property(gaming, "external_id", Value::String("ext_gm_001".into()));
    g.set_vertex_property(gaming, "tenant_id", Value::String("NVDA".into()));

    g.set_vertex_property(chunk1, "name", Value::String("chunk_001".into()));
    g.set_vertex_property(chunk1, "tenant_id", Value::String("NVDA".into()));

    g.set_vertex_property(doc1, "name", Value::String("nvidia_10k_2024".into()));
    g.set_vertex_property(doc1, "tenant_id", Value::String("NVDA".into()));

    g.set_vertex_property(concept1, "name", Value::String("gpu revenue growth".into()));
    g.set_vertex_property(concept1, "tenant_id", Value::String("NVDA".into()));

    let e0 = g.add_edge(nvidia, revenue, "RELATION");
    g.set_edge_property(e0, "name", Value::String("discloses".into()));
    g.set_edge_property(e0, "weight", Value::Float64(1.0));

    let e1 = g.add_edge(revenue, q4_2024, "RELATION");
    g.set_edge_property(e1, "name", Value::String("reported_in".into()));
    g.set_edge_property(e1, "weight", Value::Float64(1.0));

    let e2 = g.add_edge(nvidia, datacenter, "RELATION");
    g.set_edge_property(e2, "name", Value::String("has_segment".into()));
    g.set_edge_property(e2, "weight", Value::Float64(1.0));

    let e3 = g.add_edge(nvidia, gaming, "RELATION");
    g.set_edge_property(e3, "name", Value::String("has_segment".into()));
    g.set_edge_property(e3, "weight", Value::Float64(1.0));

    let e4 = g.add_edge(doc1, chunk1, "CONTAINS");
    g.set_edge_property(e4, "name", Value::String("contains".into()));

    let e5 = g.add_edge(chunk1, nvidia, "REFERENCES");
    g.set_edge_property(e5, "name", Value::String("references".into()));

    let e6 = g.add_edge(concept1, nvidia, "SIMILAR_TO");
    g.set_edge_property(e6, "name", Value::String("similar_to".into()));

    g.build();
    g
}

// Q1: Entity search by name (CONTAINS)
#[test]
fn q1_entity_search_by_name() {
    let g = build_finreflect_mini();
    let cypher = "MATCH (n:Entity) WHERE n.tenant_id = 'NVDA' AND n.name CONTAINS 'revenue' RETURN n.name, n.type LIMIT 20";
    let result = run_cypher(cypher, &g);
    assert!(result.is_ok(), "Q1 FAILED: {:?}", result.err());
    let qr = result.unwrap();
    assert!(qr.num_rows() > 0, "Q1 returned 0 rows");
}

// Q2: 1-hop outgoing neighbors
#[test]
fn q2_one_hop_outgoing_neighbors() {
    let g = build_finreflect_mini();
    let cypher = "MATCH (n:Entity)-[r]->(m) WHERE n.name = 'nvidia' RETURN m.name, m.type";
    let result = run_cypher(cypher, &g);
    assert!(result.is_ok(), "Q2 FAILED: {:?}", result.err());
    let qr = result.unwrap();
    assert!(
        qr.num_rows() >= 3,
        "Q2 expected >=3 neighbors, got {}",
        qr.num_rows()
    );
}

// Q3: Neighbors filtered by edge label
#[test]
fn q3_neighbors_filtered_by_edge_type() {
    let g = build_finreflect_mini();
    let cypher = "MATCH (n:Entity)-[:RELATION]->(m) WHERE n.name = 'nvidia' RETURN m.name";
    let result = run_cypher(cypher, &g);
    assert!(result.is_ok(), "Q3 FAILED: {:?}", result.err());
    let qr = result.unwrap();
    assert!(qr.num_rows() >= 3, "Q3 expected >=3, got {}", qr.num_rows());
}

// Q4: 2-hop directed traversal
#[test]
fn q4_two_hop_directed_traversal() {
    let g = build_finreflect_mini();
    let cypher = "MATCH (a:Entity)-[:RELATION]->(b)-[:RELATION]->(c) WHERE a.name = 'nvidia' RETURN c.name, c.type";
    let result = run_cypher(cypher, &g);
    assert!(result.is_ok(), "Q4 FAILED: {:?}", result.err());
    let qr = result.unwrap();
    assert!(qr.num_rows() > 0, "Q4 expected results for 2-hop traversal");
}

// Q5: Edge enrichment (source->edge->target with type(r))
#[test]
fn q5_edge_enrichment_pattern() {
    let g = build_finreflect_mini();
    let cypher = "MATCH (source:Entity)-[edge]->(target) WHERE source.name = 'nvidia' RETURN source.name, type(edge) AS edge_type, target.name";
    let result = run_cypher(cypher, &g);
    assert!(result.is_ok(), "Q5 FAILED: {:?}", result.err());
    let qr = result.unwrap();
    assert!(qr.num_rows() >= 3, "Q5 expected >=3, got {}", qr.num_rows());
}

// Q6: Lookup by external_id
#[test]
fn q6_lookup_by_external_id() {
    let g = build_finreflect_mini();
    let cypher = "MATCH (n) WHERE n.external_id = 'ext_nvidia_001' RETURN n.name, n.type";
    let result = run_cypher(cypher, &g);
    assert!(result.is_ok(), "Q6 FAILED: {:?}", result.err());
    let qr = result.unwrap();
    assert_eq!(
        qr.num_rows(),
        1,
        "Q6 expected 1 result, got {}",
        qr.num_rows()
    );
}

// Q7: Count vertices by tenant
#[test]
fn q7_count_vertices_by_tenant() {
    let g = build_finreflect_mini();
    let cypher = "MATCH (n) WHERE n.tenant_id = 'NVDA' RETURN count(n) AS cnt";
    let result = run_cypher(cypher, &g);
    assert!(result.is_ok(), "Q7 FAILED: {:?}", result.err());
    let qr = result.unwrap();
    assert!(qr.num_rows() > 0, "Q7 returned 0 rows");
}

// Q8: Parameterized entity lookup (parse only -- $params without binding)
#[test]
fn q8_parameterized_entity_lookup() {
    let g = build_finreflect_mini();
    let cypher = "MATCH (n:Entity) WHERE n.name = $entity_name AND n.tenant_id = $tenant RETURN n.name, n.type, n.external_id";
    let result = run_cypher(cypher, &g);
    // Params unbound -> may return 0 rows or null-match, but must not crash
    assert!(
        result.is_ok(),
        "Q8 FAILED (should parse+exec even with unbound params): {:?}",
        result.err()
    );
}

// Q9: Chunk -> Document traversal
#[test]
fn q9_chunk_document_traversal() {
    let g = build_finreflect_mini();
    let cypher = "MATCH (d:Document)-[:CONTAINS]->(c:Chunk) RETURN d.name, c.name";
    let result = run_cypher(cypher, &g);
    assert!(result.is_ok(), "Q9 FAILED: {:?}", result.err());
    let qr = result.unwrap();
    assert!(qr.num_rows() > 0, "Q9 expected doc->chunk results");
}

// Q10: CREATE vertex (write path)
#[test]
fn q10_create_vertex() {
    let mut g = build_finreflect_mini();
    let cypher = "CREATE (n:Entity {name: 'new_entity', external_id: 'ext_new_001', tenant_id: 'TEST'}) RETURN n.name";
    let result = run_cypher_mut_in_memory(cypher, &mut g);
    assert!(result.is_ok(), "Q10 CREATE FAILED: {:?}", result.err());
}

// Q11: MATCH + CREATE edge (write path)
#[test]
fn q11_create_edge_between_matched() {
    let mut g = build_finreflect_mini();
    let cypher = "MATCH (a:Entity), (b:Entity) WHERE a.name = 'nvidia' AND b.name = 'gaming' CREATE (a)-[:INVESTS_IN]->(b)";
    let result = run_cypher_mut_in_memory(cypher, &mut g);
    assert!(
        result.is_ok(),
        "Q11 MATCH+CREATE FAILED: {:?}",
        result.err()
    );
}

// Q12: Scan with labels() function
#[test]
fn q12_scan_multiple_labels() {
    let g = build_finreflect_mini();
    let cypher = "MATCH (n) WHERE n.tenant_id = 'NVDA' RETURN n.name, labels(n) AS label LIMIT 50";
    let result = run_cypher(cypher, &g);
    assert!(result.is_ok(), "Q12 FAILED: {:?}", result.err());
    let qr = result.unwrap();
    assert!(
        qr.num_rows() >= 5,
        "Q12 expected >=5, got {}",
        qr.num_rows()
    );
}

// Q13: type(r) on relationships
#[test]
fn q13_relationship_type_function() {
    let g = build_finreflect_mini();
    let cypher =
        "MATCH (a:Entity)-[r]->(b) WHERE a.name = 'nvidia' RETURN type(r) AS rel_type, b.name";
    let result = run_cypher(cypher, &g);
    assert!(result.is_ok(), "Q13 type(r) FAILED: {:?}", result.err());
}

// Q14: DISTINCT dedup
#[test]
fn q14_distinct_neighbors() {
    let g = build_finreflect_mini();
    let cypher = "MATCH (n:Entity)-[]->(m) WHERE n.name = 'nvidia' RETURN DISTINCT m.name";
    let result = run_cypher(cypher, &g);
    assert!(result.is_ok(), "Q14 DISTINCT FAILED: {:?}", result.err());
}
