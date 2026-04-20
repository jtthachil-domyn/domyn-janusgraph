//! Loads the real 10-K SEC filing KG triplet data into a Nexus graph.
//!
//! Same dataset used by domyngraph-benchmark: 18 tickers, ~47.5K vertices,
//! ~64.9K edges, 1,133 unique predicates.

use nexus_core::graph::Graph;
use nexus_core::properties::PropertyType;
use nexus_core::types::{Value, VertexId};
use serde_json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

const DEFAULT_TRIPLET_DIR: &str =
    "/Users/josephthomasthachil/Desktop/Domyn/uda/KG_VIEWS/Qwen2.5-72B-Instruct/reflection";

#[derive(Debug)]
pub struct LoadStats {
    pub vertices: usize,
    pub edges: usize,
    pub predicates: usize,
    pub tickers: usize,
    pub load_time_ms: f64,
    pub build_time_ms: f64,
}

pub struct RealDataGraph {
    pub graph: Graph,
    pub stats: LoadStats,
    pub seed_external_ids: Vec<String>,
    pub tickers: Vec<String>,
}

fn normalize_predicate(pred: &str) -> String {
    let trimmed = pred.trim();
    if trimmed.is_empty() {
        return "RELATED".to_string();
    }
    trimmed
        .split(|c: char| c.is_whitespace() || c == '_')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut chars = p.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => {
                    let upper: String = first.to_uppercase().collect();
                    upper + &chars.as_str().to_lowercase()
                }
            }
        })
        .collect::<Vec<_>>()
        .join("_")
}

fn make_external_id(tenant_id: &str, name: &str, entity_type: &str) -> String {
    format!("{tenant_id}:{name}:{entity_type}")
}

pub fn load_real_data(triplet_dir: Option<&str>) -> RealDataGraph {
    let root = PathBuf::from(triplet_dir.unwrap_or(DEFAULT_TRIPLET_DIR));
    eprintln!("Loading triplet data from: {}", root.display());

    let load_start = Instant::now();

    let mut vertex_map: HashMap<String, (String, String, String, String)> = HashMap::new();
    struct RawEdge {
        source_id: String,
        target_id: String,
        predicate: String,
    }
    let mut raw_edges: Vec<RawEdge> = Vec::new();
    let mut predicates = std::collections::HashSet::new();
    let mut tickers: Vec<String> = Vec::new();

    let mut ticker_dirs: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("cannot read triplet directory")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.path())
        .collect();
    ticker_dirs.sort();

    for td_path in &ticker_dirs {
        let ticker = td_path.file_name().unwrap().to_str().unwrap().to_string();

        let json_files: Vec<PathBuf> = walkdir(td_path, "_triplets_");
        if json_files.is_empty() {
            continue;
        }

        tickers.push(ticker.clone());
        let json_file = &json_files[0];
        let data: Vec<serde_json::Value> =
            serde_json::from_reader(std::fs::File::open(json_file).unwrap()).unwrap();

        for chunk in &data {
            let chunk_ticker = chunk
                .get("ticker")
                .and_then(|v| v.as_str())
                .unwrap_or(&ticker);
            let chunk_triplet = match chunk.get("chunk_triplet") {
                Some(ct) => ct,
                None => continue,
            };
            let triplet_map = match chunk_triplet.as_object() {
                Some(m) => m,
                None => continue,
            };

            for (_key, triplet) in triplet_map {
                let arr = match triplet.as_array() {
                    Some(a) if a.len() >= 5 => a,
                    _ => continue,
                };

                let subj_name = val_to_string(&arr[0]);
                let subj_type = val_to_string(&arr[1]);
                let predicate_raw = val_to_string(&arr[2]);
                let obj_name = val_to_string(&arr[3]);
                let obj_type = val_to_string(&arr[4]);

                if subj_name.is_empty() || obj_name.is_empty() || predicate_raw.is_empty() {
                    continue;
                }

                let pred = normalize_predicate(&predicate_raw);
                predicates.insert(pred.clone());

                let subj_ext_id = make_external_id(chunk_ticker, &subj_name, &subj_type);
                let obj_ext_id = make_external_id(chunk_ticker, &obj_name, &obj_type);

                vertex_map.entry(subj_ext_id.clone()).or_insert_with(|| {
                    (
                        subj_name.clone(),
                        subj_type.clone(),
                        chunk_ticker.to_string(),
                        chunk_ticker.to_string(),
                    )
                });
                vertex_map.entry(obj_ext_id.clone()).or_insert_with(|| {
                    (
                        obj_name.clone(),
                        obj_type.clone(),
                        chunk_ticker.to_string(),
                        chunk_ticker.to_string(),
                    )
                });

                raw_edges.push(RawEdge {
                    source_id: subj_ext_id,
                    target_id: obj_ext_id,
                    predicate: pred,
                });
            }
        }
    }

    let load_parse_time = load_start.elapsed();
    eprintln!(
        "Parsed: {} vertices, {} edges, {} predicates, {} tickers in {:.1}ms",
        vertex_map.len(),
        raw_edges.len(),
        predicates.len(),
        tickers.len(),
        load_parse_time.as_secs_f64() * 1000.0,
    );

    let build_start = Instant::now();
    let mut g = Graph::new(vertex_map.len(), raw_edges.len());

    g.register_vertex_property("name", PropertyType::String, true, false);
    g.register_vertex_property("entity_type", PropertyType::String, true, false);
    g.register_vertex_property("external_id", PropertyType::String, true, true);
    g.register_vertex_property("tenant_id", PropertyType::String, true, false);
    g.register_vertex_property("ticker", PropertyType::String, true, false);

    let mut ext_id_to_vid: HashMap<String, VertexId> = HashMap::with_capacity(vertex_map.len());

    for (ext_id, (name, entity_type, ticker_val, tenant_id)) in &vertex_map {
        let v = g.add_vertex("Entity");
        g.set_vertex_property(v, "name", Value::String(name.clone()));
        g.set_vertex_property(v, "entity_type", Value::String(entity_type.clone()));
        g.set_vertex_property(v, "external_id", Value::String(ext_id.clone()));
        g.set_vertex_property(v, "tenant_id", Value::String(tenant_id.clone()));
        g.set_vertex_property(v, "ticker", Value::String(ticker_val.clone()));
        ext_id_to_vid.insert(ext_id.clone(), v);
    }

    for edge in &raw_edges {
        let src = ext_id_to_vid[&edge.source_id];
        let dst = ext_id_to_vid[&edge.target_id];
        g.add_edge(src, dst, &edge.predicate);
    }

    g.build();

    let build_time = build_start.elapsed();
    eprintln!(
        "Graph built: {} V, {} E in {:.1}ms",
        g.num_vertices(),
        g.num_edges(),
        build_time.as_secs_f64() * 1000.0,
    );

    let seed_external_ids: Vec<String> = {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut ids: Vec<String> = ext_id_to_vid.keys().cloned().collect();
        ids.sort_by(|a, b| {
            let mut ha = DefaultHasher::new();
            a.hash(&mut ha);
            let mut hb = DefaultHasher::new();
            b.hash(&mut hb);
            ha.finish().cmp(&hb.finish())
        });
        ids.truncate(50);
        ids
    };

    RealDataGraph {
        stats: LoadStats {
            vertices: g.num_vertices(),
            edges: g.num_edges() as usize,
            predicates: predicates.len(),
            tickers: tickers.len(),
            load_time_ms: load_parse_time.as_secs_f64() * 1000.0,
            build_time_ms: build_time.as_secs_f64() * 1000.0,
        },
        graph: g,
        seed_external_ids,
        tickers,
    }
}

/// Parsed ticker data ready for graph ingestion.
pub struct ParsedTicker {
    pub ticker: String,
    pub vertices: Vec<(String, String, String, String, String)>, // (ext_id, name, entity_type, ticker, tenant_id)
    pub edges: Vec<(String, String, String)>, // (source_ext_id, target_ext_id, predicate)
}

/// Parse a single ticker directory without building a graph.
pub fn parse_single_ticker(triplet_dir: Option<&str>, ticker_name: &str) -> Option<ParsedTicker> {
    let root = PathBuf::from(triplet_dir.unwrap_or(DEFAULT_TRIPLET_DIR));
    let td_path = root.join(ticker_name);
    if !td_path.is_dir() {
        return None;
    }

    let json_files = walkdir(&td_path, "_triplets_");
    if json_files.is_empty() {
        return None;
    }

    let data: Vec<serde_json::Value> =
        serde_json::from_reader(std::fs::File::open(&json_files[0]).unwrap()).unwrap();

    let mut vertex_map: HashMap<String, (String, String, String, String)> = HashMap::new();
    let mut raw_edges: Vec<(String, String, String)> = Vec::new();

    for chunk in &data {
        let chunk_ticker = chunk
            .get("ticker")
            .and_then(|v| v.as_str())
            .unwrap_or(ticker_name);
        let triplet_map = match chunk.get("chunk_triplet").and_then(|ct| ct.as_object()) {
            Some(m) => m,
            None => continue,
        };

        for (_key, triplet) in triplet_map {
            let arr = match triplet.as_array() {
                Some(a) if a.len() >= 5 => a,
                _ => continue,
            };

            let subj_name = val_to_string(&arr[0]);
            let subj_type = val_to_string(&arr[1]);
            let predicate_raw = val_to_string(&arr[2]);
            let obj_name = val_to_string(&arr[3]);
            let obj_type = val_to_string(&arr[4]);

            if subj_name.is_empty() || obj_name.is_empty() || predicate_raw.is_empty() {
                continue;
            }

            let pred = normalize_predicate(&predicate_raw);
            let subj_ext_id = make_external_id(chunk_ticker, &subj_name, &subj_type);
            let obj_ext_id = make_external_id(chunk_ticker, &obj_name, &obj_type);

            vertex_map.entry(subj_ext_id.clone()).or_insert_with(|| {
                (
                    subj_name.clone(),
                    subj_type.clone(),
                    chunk_ticker.to_string(),
                    chunk_ticker.to_string(),
                )
            });
            vertex_map.entry(obj_ext_id.clone()).or_insert_with(|| {
                (
                    obj_name.clone(),
                    obj_type.clone(),
                    chunk_ticker.to_string(),
                    chunk_ticker.to_string(),
                )
            });

            raw_edges.push((subj_ext_id, obj_ext_id, pred));
        }
    }

    let vertices: Vec<_> = vertex_map
        .into_iter()
        .map(|(ext_id, (name, etype, ticker, tenant))| (ext_id, name, etype, ticker, tenant))
        .collect();

    Some(ParsedTicker {
        ticker: ticker_name.to_string(),
        vertices,
        edges: raw_edges,
    })
}

/// Build a graph from pre-parsed vertex/edge data.
/// Returns the graph and the build time in ms.
pub fn build_graph_from_parsed(
    vertices: &[(String, String, String, String, String)],
    edges: &[(String, String, String)],
) -> (Graph, f64) {
    let build_start = Instant::now();
    let mut g = Graph::new(vertices.len(), edges.len());

    g.register_vertex_property("name", PropertyType::String, true, false);
    g.register_vertex_property("entity_type", PropertyType::String, true, false);
    g.register_vertex_property("external_id", PropertyType::String, true, true);
    g.register_vertex_property("tenant_id", PropertyType::String, true, false);
    g.register_vertex_property("ticker", PropertyType::String, true, false);

    let mut ext_id_to_vid: HashMap<String, VertexId> = HashMap::with_capacity(vertices.len());

    for (ext_id, name, entity_type, ticker_val, tenant_id) in vertices {
        let v = g.add_vertex("Entity");
        g.set_vertex_property(v, "name", Value::String(name.clone()));
        g.set_vertex_property(v, "entity_type", Value::String(entity_type.clone()));
        g.set_vertex_property(v, "external_id", Value::String(ext_id.clone()));
        g.set_vertex_property(v, "tenant_id", Value::String(tenant_id.clone()));
        g.set_vertex_property(v, "ticker", Value::String(ticker_val.clone()));
        ext_id_to_vid.insert(ext_id.clone(), v);
    }

    for (src_id, dst_id, pred) in edges {
        if let (Some(&src), Some(&dst)) = (ext_id_to_vid.get(src_id), ext_id_to_vid.get(dst_id)) {
            g.add_edge(src, dst, pred);
        }
    }

    g.build();
    let build_ms = build_start.elapsed().as_secs_f64() * 1000.0;
    (g, build_ms)
}

/// Parse all tickers and return a flat list of ParsedTicker.
pub fn parse_all_tickers(triplet_dir: Option<&str>) -> Vec<ParsedTicker> {
    let root = PathBuf::from(triplet_dir.unwrap_or(DEFAULT_TRIPLET_DIR));
    let mut ticker_dirs: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("cannot read triplet directory")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.path())
        .collect();
    ticker_dirs.sort();

    let mut result = Vec::new();
    for td_path in &ticker_dirs {
        let ticker = td_path.file_name().unwrap().to_str().unwrap();
        if let Some(parsed) = parse_single_ticker(triplet_dir, ticker) {
            result.push(parsed);
        }
    }
    result
}

fn val_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.trim().to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn walkdir(dir: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut results = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                results.extend(walkdir(&path, pattern));
            } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.contains(pattern) && name.ends_with(".json") {
                    results.push(path);
                }
            }
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_predicate_basic() {
        assert_eq!(normalize_predicate("discloses"), "Discloses");
        assert_eq!(normalize_predicate("has component"), "Has_Component");
        assert_eq!(normalize_predicate("OPERATES_IN"), "Operates_In");
        assert_eq!(normalize_predicate(""), "RELATED");
    }
}
