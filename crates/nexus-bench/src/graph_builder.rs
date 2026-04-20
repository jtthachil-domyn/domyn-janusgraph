//! Builds a synthetic financial graph for benchmark workloads.
//!
//! Mirrors the structure used in domyngraph-benchmark:
//!   Companies -> Disclosures -> Metrics -> Components
//!   Companies -> Operates_In -> Geographies
//!   Documents -> Mentions -> Entities

use nexus_core::graph::Graph;
use nexus_core::properties::PropertyType;
use nexus_core::types::{Value, VertexId};

pub struct BenchGraph {
    pub graph: Graph,
    pub companies: Vec<VertexId>,
    pub metrics: Vec<VertexId>,
    pub segments: Vec<VertexId>,
    pub geographies: Vec<VertexId>,
    pub documents: Vec<VertexId>,
}

pub fn build_bench_graph(
    num_companies: usize,
    metrics_per_company: usize,
    segments_per_metric: usize,
    num_geos: usize,
    num_docs: usize,
) -> BenchGraph {
    let total_vertices = num_companies
        + num_companies * metrics_per_company
        + num_companies * metrics_per_company * segments_per_metric
        + num_geos
        + num_docs;
    let total_edges = num_companies * metrics_per_company
        + num_companies * metrics_per_company * segments_per_metric
        + num_companies * 2 // operates_in + document mentions
        + num_docs;

    let mut g = Graph::new(total_vertices, total_edges);

    g.register_vertex_property("name", PropertyType::String, true, false);
    g.register_vertex_property("entity_type", PropertyType::String, true, false);
    g.register_vertex_property("external_id", PropertyType::String, true, true);
    g.register_vertex_property("tenant_id", PropertyType::String, true, false);
    g.register_vertex_property("ticker", PropertyType::String, true, false);
    g.register_vertex_property("value", PropertyType::Float64, false, false);
    g.register_vertex_property("text", PropertyType::String, false, false);

    g.register_edge_property("weight", PropertyType::Float64, false, false);
    g.register_edge_property("confidence", PropertyType::Float64, false, false);

    let mut companies = Vec::with_capacity(num_companies);
    let mut metrics = Vec::new();
    let mut segments = Vec::new();

    let tickers = [
        "AAPL", "GOOGL", "MSFT", "AMZN", "META", "TSLA", "NVDA", "JPM", "V", "WMT", "PG", "JNJ",
        "UNH", "HD", "MA", "DIS", "PYPL", "NFLX", "INTC", "CSCO",
    ];

    for i in 0..num_companies {
        let ticker = tickers[i % tickers.len()];
        let name = format!("Company_{i}");
        let v = g.add_vertex("Entity");
        g.set_vertex_property(v, "name", Value::String(name.clone()));
        g.set_vertex_property(v, "entity_type", "ORG".into());
        g.set_vertex_property(
            v,
            "external_id",
            Value::String(format!("{ticker}:{name}:ORG")),
        );
        g.set_vertex_property(v, "tenant_id", Value::String(ticker.to_string()));
        g.set_vertex_property(v, "ticker", Value::String(ticker.to_string()));
        companies.push(v);

        for m in 0..metrics_per_company {
            let metric_name = match m % 5 {
                0 => "Revenue",
                1 => "Net_Income",
                2 => "EBITDA",
                3 => "Operating_Cash_Flow",
                _ => "Gross_Margin",
            };
            let mv = g.add_vertex("Metric");
            g.set_vertex_property(mv, "name", Value::String(format!("{ticker}_{metric_name}")));
            g.set_vertex_property(mv, "entity_type", "FIN_METRIC".into());
            g.set_vertex_property(mv, "value", Value::Float64(100.0 * (i as f64 + 1.0)));

            let eid = g.add_edge(v, mv, "DISCLOSES");
            g.set_edge_property(eid, "weight", Value::Float64(0.9));
            metrics.push(mv);

            for s in 0..segments_per_metric {
                let seg_name = format!("Segment_{s}_of_{metric_name}");
                let sv = g.add_vertex("Segment");
                g.set_vertex_property(sv, "name", Value::String(seg_name));
                g.add_edge(mv, sv, "HAS_COMPONENT");
                segments.push(sv);
            }
        }
    }

    let geo_names = [
        "United_States",
        "China",
        "Europe",
        "Japan",
        "India",
        "Brazil",
        "UK",
        "Germany",
        "France",
        "Canada",
    ];
    let mut geographies = Vec::with_capacity(num_geos);
    for i in 0..num_geos {
        let gv = g.add_vertex("Geography");
        g.set_vertex_property(
            gv,
            "name",
            Value::String(geo_names[i % geo_names.len()].to_string()),
        );
        g.set_vertex_property(gv, "entity_type", "GEO".into());
        geographies.push(gv);
    }

    for (i, &company) in companies.iter().enumerate() {
        if !geographies.is_empty() {
            let geo = geographies[i % geographies.len()];
            g.add_edge(company, geo, "OPERATES_IN");
        }
    }

    let mut documents = Vec::with_capacity(num_docs);
    for i in 0..num_docs {
        let dv = g.add_vertex("Document");
        g.set_vertex_property(dv, "name", Value::String(format!("10K_Filing_{i}")));
        g.set_vertex_property(
            dv,
            "text",
            Value::String(format!("Annual report for fiscal year {}", 2020 + i % 5)),
        );
        documents.push(dv);

        if !companies.is_empty() {
            let company = companies[i % companies.len()];
            g.add_edge(dv, company, "MENTIONS");
        }
    }

    g.build();

    BenchGraph {
        graph: g,
        companies,
        metrics,
        segments,
        geographies,
        documents,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_small_bench_graph() {
        let bg = build_bench_graph(5, 3, 2, 3, 5);
        assert!(bg.graph.num_vertices() > 0);
        assert!(bg.graph.num_edges() > 0);
        assert_eq!(bg.companies.len(), 5);
    }
}
