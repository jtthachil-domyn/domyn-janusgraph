//! Semiring traits for graph algorithms expressed as linear algebra.
//!
//! A semiring (S, +, *, 0, 1) defines how values combine during matrix operations:
//! - `add` ("+") combines values from multiple paths (e.g., min, or, plus)
//! - `multiply` ("*") combines values along a path (e.g., plus, and, times)
//! - `zero` is the additive identity
//! - `one` is the multiplicative identity
//!
//! Different algorithms use different semirings:
//! - BFS: (min, plus) over integers -- tropical semiring
//! - PageRank: (plus, times) over floats -- standard arithmetic
//! - Connected Components: (min, min) over integers
//! - Reachability: (or, and) over booleans

/// A semiring defines the algebraic operations for graph traversal.
pub trait Semiring: Send + Sync {
    type Value: Clone + Send + Sync + PartialEq;

    fn zero(&self) -> Self::Value;
    fn one(&self) -> Self::Value;
    fn add(&self, a: &Self::Value, b: &Self::Value) -> Self::Value;
    fn multiply(&self, a: &Self::Value, b: &Self::Value) -> Self::Value;
}

/// Boolean semiring (OR, AND) for reachability queries.
/// "Can vertex A reach vertex B?"
#[derive(Debug, Clone)]
pub struct BooleanSemiring;

impl Semiring for BooleanSemiring {
    type Value = bool;

    fn zero(&self) -> bool {
        false
    }
    fn one(&self) -> bool {
        true
    }
    fn add(&self, a: &bool, b: &bool) -> bool {
        *a || *b
    }
    fn multiply(&self, a: &bool, b: &bool) -> bool {
        *a && *b
    }
}

/// Tropical semiring (min, plus) for shortest path / BFS.
/// Distance accumulates along paths; minimum wins across paths.
#[derive(Debug, Clone)]
pub struct TropicalSemiring;

impl Semiring for TropicalSemiring {
    type Value = f64;

    fn zero(&self) -> f64 {
        f64::INFINITY
    }
    fn one(&self) -> f64 {
        0.0
    }
    fn add(&self, a: &f64, b: &f64) -> f64 {
        a.min(*b)
    }
    fn multiply(&self, a: &f64, b: &f64) -> f64 {
        a + b
    }
}

/// Arithmetic semiring (plus, times) for PageRank.
#[derive(Debug, Clone)]
pub struct ArithmeticSemiring;

impl Semiring for ArithmeticSemiring {
    type Value = f64;

    fn zero(&self) -> f64 {
        0.0
    }
    fn one(&self) -> f64 {
        1.0
    }
    fn add(&self, a: &f64, b: &f64) -> f64 {
        a + b
    }
    fn multiply(&self, a: &f64, b: &f64) -> f64 {
        a * b
    }
}

/// Min-select semiring (min, min) for connected components.
#[derive(Debug, Clone)]
pub struct MinMinSemiring;

impl Semiring for MinMinSemiring {
    type Value = u64;

    fn zero(&self) -> u64 {
        u64::MAX
    }
    fn one(&self) -> u64 {
        0
    }
    fn add(&self, a: &u64, b: &u64) -> u64 {
        (*a).min(*b)
    }
    fn multiply(&self, a: &u64, b: &u64) -> u64 {
        (*a).min(*b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boolean_semiring() {
        let sr = BooleanSemiring;
        assert!(!sr.zero());
        assert!(sr.one());
        assert!(sr.add(&true, &false));
        assert!(!sr.multiply(&true, &false));
        assert!(sr.multiply(&true, &true));
    }

    #[test]
    fn tropical_semiring_shortest_path() {
        let sr = TropicalSemiring;
        // Two paths to same node: cost 3 vs cost 5 -> min gives 3
        assert_eq!(sr.add(&3.0, &5.0), 3.0);
        // Path through two edges: cost 2 + cost 3 = cost 5
        assert_eq!(sr.multiply(&2.0, &3.0), 5.0);
    }

    #[test]
    fn arithmetic_semiring_pagerank() {
        let sr = ArithmeticSemiring;
        assert_eq!(sr.add(&0.3, &0.2), 0.5);
        assert_eq!(sr.multiply(&0.5, &0.8), 0.4);
    }
}
