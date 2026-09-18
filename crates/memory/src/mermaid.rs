//! ADR-69 slice 3 — human-readable memory graph rendering + read-side loading.
//!
//! Pure, deterministic rendering of memory chunks and links into Mermaid
//! `flowchart TD` source, plus read-only loaders for the CLI and desktop
//! views. No write-side DDL is executed by any function in this module.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use concerto_core::error::MemoryError;
use concerto_core::memory::{MemoryLink, MemoryLinkKind, ProjectId};
use concerto_core::CancellationToken;
use concerto_core::VectorStore as _;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;

use crate::links::LinkStore;
use crate::scoring::{chunk_link_score, kind_weight, DECAY_FLOOR_DAYS};
use crate::vector_store::SqliteVectorStore;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Limits applied when rendering a graph. A value of `0` means "unlimited".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphCap {
    /// Maximum nodes. Default 100.
    pub max_nodes: usize,
    /// Maximum edges. Default 400.
    pub max_edges: usize,
}

impl Default for GraphCap {
    fn default() -> Self {
        Self { max_nodes: 100, max_edges: 400 }
    }
}

/// A graph node: a memory chunk with its evidence score.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphNode {
    pub id: String,
    pub label: String,
    pub score: f64,
}

/// A graph edge: a memory link between two chunks.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    pub kind: MemoryLinkKind,
    pub weight: f64,
    pub score: f64,
}

/// A rendered memory graph (before Mermaid serialization).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemoryGraph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

/// Detail view for a single chunk's links — used by `explain`.
#[derive(Debug, Clone)]
pub struct ChunkDetail {
    pub chunk_id: String,
    pub content: String,
    pub out_links: Vec<MemoryLink>,
    pub in_links: Vec<MemoryLink>,
    pub score: f64,
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Escape special characters for a Mermaid label string.
fn escape_label(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("&#amp;"),
            '"' => out.push_str("&#quot;"),
            '#' => out.push_str("&#35;"),
            '\n' => out.push_str("<br/>"),
            _ => out.push(ch),
        }
    }
    out
}

/// Sanitize a chunk id for use as a Mermaid node identifier.
///
/// Only `[A-Za-z0-9_-]` survive; everything else becomes `_`. An empty or
/// digit-starting result gets a `n_` prefix.
fn sanitize_node_id(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    if out.is_empty() || out.as_bytes()[0].is_ascii_digit() {
        out.insert_str(0, "n_");
    }
    out
}

/// Truncate content to ~`LABEL_MAX_CHARS` for a graph node label.
fn preview(content: &str) -> String {
    let trimmed = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.chars().count() <= LABEL_MAX_CHARS {
        trimmed
    } else {
        let mut s: String = trimmed.chars().take(LABEL_MAX_CHARS).collect();
        s.push('…');
        s
    }
}

/// Maximum label length (chars) in the rendered graph.
const LABEL_MAX_CHARS: usize = 120;

// ---------------------------------------------------------------------------
// Renderer (pure, no I/O)
// ---------------------------------------------------------------------------

/// Render a [`MemoryGraph`] into Mermaid `flowchart TD` source.
///
/// Deterministic: same input always produces the same output regardless of
/// node or edge insertion order. The algorithm:
///
/// 1. Dedupe nodes by id (keep first occurrence).
/// 2. Sort nodes by score descending, then id ascending.
/// 3. Cap nodes; sanitize ids with collision suffixes `_1`, `_2`, … in
///    sorted-id order.
/// 4. Keep edges whose both endpoints are in the capped node set; dedupe by
///    `(from, to, kind)`.
/// 5. Sort edges by score descending, from asc, to asc, kind asc; cap.
/// 6. Emit `flowchart TD` lines.
pub fn render_mermaid(graph: &MemoryGraph, cap: GraphCap) -> String {
    // 1. Dedupe nodes by id.
    let mut unique: Vec<&GraphNode> = Vec::with_capacity(graph.nodes.len());
    let mut seen = HashSet::new();
    for node in &graph.nodes {
        if seen.insert(node.id.as_str()) {
            unique.push(node);
        }
    }

    // 2. Sort by score desc, then id asc (stable, deterministic).
    unique.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));

    // 3. Cap.
    let omitted_nodes = if cap.max_nodes > 0 && unique.len() > cap.max_nodes {
        let extra = unique.len() - cap.max_nodes;
        unique.truncate(cap.max_nodes);
        extra
    } else {
        0
    };

    // 4. Sanitize + collision-resolve in sorted order.
    let mut used: HashSet<String> = HashSet::new();
    let mut orig_to_sanitize: Vec<(&str, String)> = Vec::with_capacity(unique.len());
    for node in &unique {
        let base = sanitize_node_id(&node.id);
        let mut candidate = base.clone();
        let mut n: usize = 1;
        while used.contains(&candidate) {
            candidate = format!("{base}_{n}");
            n += 1;
        }
        used.insert(candidate.clone());
        orig_to_sanitize.push((node.id.as_str(), candidate));
    }
    let lookup: HashMap<&str, &str> =
        orig_to_sanitize.iter().map(|(o, s)| (*o, s.as_str())).collect();

    // 5. Filter + dedupe + sort + cap edges.
    let node_ids: HashSet<&str> = unique.iter().map(|n| n.id.as_str()).collect();
    let mut edge_seen: HashSet<(&str, &str, &str)> = HashSet::new();
    let mut edges: Vec<&GraphEdge> = Vec::new();
    for edge in &graph.edges {
        let kind_str = edge.kind.as_str();
        if node_ids.contains(edge.from.as_str())
            && node_ids.contains(edge.to.as_str())
            && edge_seen.insert((edge.from.as_str(), edge.to.as_str(), kind_str))
        {
            edges.push(edge);
        }
    }
    edges.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.from.cmp(&b.from))
            .then_with(|| a.to.cmp(&b.to))
            .then_with(|| a.kind.as_str().cmp(b.kind.as_str()))
    });
    let omitted_edges = if cap.max_edges > 0 && edges.len() > cap.max_edges {
        let extra = edges.len() - cap.max_edges;
        edges.truncate(cap.max_edges);
        extra
    } else {
        0
    };

    // 6. Emit.
    let mut out = String::from("flowchart TD\n");
    for node in &unique {
        let sid = lookup[node.id.as_str()];
        let label = escape_label(&node.label);
        out.push_str(&format!("  {sid}[\"{label}\"]\n"));
    }
    for edge in &edges {
        let from = lookup[edge.from.as_str()];
        let to = lookup[edge.to.as_str()];
        let kind_label = escape_label(edge.kind.as_str());
        out.push_str(&format!("  {from} -->|\"{kind_label} {:.2}\"| {to}\n", edge.score,));
    }
    if omitted_nodes > 0 || omitted_edges > 0 {
        out.push_str(&format!(
            "%% truncated: {omitted_nodes} nodes, {omitted_edges} edges omitted\n",
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Read-side loader (I/O)
// ---------------------------------------------------------------------------

/// Open a read-only-ish pool: no migrations, no DDL.
async fn open_read_pool(db_path: &Path) -> Result<SqlitePool, MemoryError> {
    let options = SqliteConnectOptions::new().filename(db_path).create_if_missing(false);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .map_err(|e| MemoryError::Persistence(format!("failed to open memory db for reading: {e}")))
}

/// Now in Unix seconds (UTC).
fn now_unix_secs() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Load a memory graph from the database for the given project.
///
/// Returns an empty graph if the database file does not exist. No write-side
/// DDL is executed — uses `pub(crate)` DDL-free constructors on the vector
/// store and link store. The number of nodes is bounded by `limit` (from
/// `VectorStore::list`); edges are enumerated via `LinkStore::incoming_links`
/// over the node set.
pub async fn load_memory_graph(
    db_path: &Path,
    project_id: &ProjectId,
    limit: usize,
    cancel: CancellationToken,
) -> Result<MemoryGraph, MemoryError> {
    if !db_path.is_file() {
        return Ok(MemoryGraph::default());
    }
    let pool = open_read_pool(db_path).await?;
    let vector_store = SqliteVectorStore::from_pool(pool.clone());
    let link_store = LinkStore::from_pool(pool);

    let chunk_rows = vector_store.list(project_id, limit, cancel.clone()).await?;
    if chunk_rows.is_empty() {
        return Ok(MemoryGraph::default());
    }

    let ids: Vec<String> = chunk_rows.iter().map(|c| c.chunk_id.clone()).collect();
    let incoming = link_store.incoming_links(&ids, cancel.clone()).await?;
    let now = now_unix_secs();

    let mut nodes = Vec::with_capacity(chunk_rows.len());
    for c in &chunk_rows {
        let links_for = incoming.get(&c.chunk_id).map(|v| v.as_slice()).unwrap_or(&[]);
        let base = chunk_link_score(links_for, Some(DECAY_FLOOR_DAYS as u16), now);
        nodes.push(GraphNode { id: c.chunk_id.clone(), label: preview(&c.content), score: base });
    }

    let id_set: HashSet<&str> = ids.iter().map(|s| s.as_str()).collect();
    let mut edges = Vec::new();
    for (target, incoming_list) in &incoming {
        for tl in incoming_list {
            let source = tl.link.from.as_str();
            if id_set.contains(source) {
                edges.push(GraphEdge {
                    from: source.to_owned(),
                    to: target.clone(),
                    kind: tl.link.kind,
                    weight: tl.link.weight,
                    score: kind_weight(tl.link.kind) * tl.link.weight,
                });
            }
        }
    }

    Ok(MemoryGraph { nodes, edges })
}

/// Load detail for a single chunk: content, outgoing links, incoming links,
/// and evidence score. Returns `Ok(None)` when the chunk is not found or the
/// database file does not exist.
pub async fn load_chunk_detail(
    db_path: &Path,
    project_id: &ProjectId,
    chunk_id: &str,
    cancel: CancellationToken,
) -> Result<Option<ChunkDetail>, MemoryError> {
    if !db_path.is_file() {
        return Ok(None);
    }
    let pool = open_read_pool(db_path).await?;
    let vector_store = SqliteVectorStore::from_pool(pool.clone());
    let link_store = LinkStore::from_pool(pool);

    let chunks =
        vector_store.get_chunks(project_id, &[chunk_id.to_owned()], cancel.clone()).await?;
    let chunk = match chunks.into_iter().next() {
        Some(c) => c,
        None => return Ok(None),
    };

    let out_links = link_store.links_from(chunk_id, cancel.clone()).await?;
    let mut incoming_map =
        link_store.incoming_links(&[chunk_id.to_owned()], cancel.clone()).await?;
    let timed_in = incoming_map.remove(chunk_id).unwrap_or_default();
    let in_links: Vec<MemoryLink> = timed_in.iter().map(|tl| tl.link.clone()).collect();
    let score = chunk_link_score(&timed_in, Some(DECAY_FLOOR_DAYS as u16), now_unix_secs());

    Ok(Some(ChunkDetail { chunk_id: chunk.id, content: chunk.content, out_links, in_links, score }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::memory::ProjectId;
    use concerto_core::CancellationToken;
    use sqlx::sqlite::SqlitePoolOptions;
    use time::OffsetDateTime;

    fn test_project() -> ProjectId {
        ProjectId("test-graph".into())
    }

    async fn seed_db(db_path: &Path) -> (SqliteVectorStore, LinkStore) {
        let options = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new().max_connections(1).connect_with(options).await.unwrap();
        let vector_store = SqliteVectorStore::new(pool.clone()).await.unwrap();
        let link_store = LinkStore::new(pool).await.unwrap();
        (vector_store, link_store)
    }

    fn chunk_record(
        project_id: &ProjectId,
        id: &str,
        content: &str,
    ) -> concerto_core::memory::EmbeddingRecord {
        concerto_core::memory::EmbeddingRecord {
            id: id.into(),
            project_id: project_id.clone(),
            chunk_hash: blake3::hash(content.as_bytes()).to_string(),
            content: content.into(),
            file_path: "src/test.rs".into(),
            start_line: Some(1),
            end_line: Some(5),
            chunk_type: concerto_core::memory::ChunkType::SlidingWindow,
            vector: vec![0.1, 0.2, 0.3],
            model_id: "test".into(),
            model_version: "1".into(),
            stale: false,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    // -----------------------------------------------------------------------
    // escape_label
    // -----------------------------------------------------------------------

    #[test]
    fn escape_label_plain() {
        assert_eq!(escape_label("hello world"), "hello world");
    }

    #[test]
    fn escape_label_specials() {
        assert_eq!(escape_label("a&b"), "a&#amp;b");
        assert_eq!(escape_label("a\"b"), "a&#quot;b");
        assert_eq!(escape_label("a#b"), "a&#35;b");
        assert_eq!(escape_label("a\nb"), "a<br/>b");
        assert_eq!(escape_label("a&\"#newline\n"), "a&#amp;&#quot;&#35;newline<br/>");
    }

    // -----------------------------------------------------------------------
    // sanitize_node_id
    // -----------------------------------------------------------------------

    #[test]
    fn sanitize_preserves_good_chars() {
        assert_eq!(sanitize_node_id("abc-123_xyz"), "abc-123_xyz");
    }

    #[test]
    fn sanitize_replaces_bad_chars() {
        assert_eq!(sanitize_node_id("a b/c!"), "a_b_c_");
    }

    #[test]
    fn sanitize_prefixes_empty() {
        assert_eq!(sanitize_node_id(""), "n_");
    }

    #[test]
    fn sanitize_prefixes_digit_start() {
        assert_eq!(sanitize_node_id("1abc"), "n_1abc");
    }

    // -----------------------------------------------------------------------
    // render_mermaid
    // -----------------------------------------------------------------------

    #[test]
    fn render_empty() {
        let out = render_mermaid(&MemoryGraph::default(), GraphCap::default());
        assert_eq!(out, "flowchart TD\n");
    }

    #[test]
    fn render_basic_two_nodes_one_edge() {
        let graph = MemoryGraph {
            nodes: vec![
                GraphNode { id: "a".into(), label: "Alpha".into(), score: 5.0 },
                GraphNode { id: "b".into(), label: "Beta".into(), score: 3.0 },
            ],
            edges: vec![GraphEdge {
                from: "a".into(),
                to: "b".into(),
                kind: MemoryLinkKind::Supports,
                weight: 1.0,
                score: 1.0,
            }],
        };
        let out = render_mermaid(&graph, GraphCap { max_nodes: 10, max_edges: 10 });
        assert!(out.starts_with("flowchart TD\n"));
        assert!(out.contains("a[\"Alpha\"]"));
        assert!(out.contains("b[\"Beta\"]"));
        assert!(out.contains("a -->|\"supports 1.00\"| b"));
    }

    #[test]
    fn render_node_sorted_by_score_desc() {
        let graph = MemoryGraph {
            nodes: vec![
                GraphNode { id: "lo".into(), label: "low".into(), score: 1.0 },
                GraphNode { id: "hi".into(), label: "high".into(), score: 9.0 },
            ],
            edges: vec![],
        };
        let out = render_mermaid(&graph, GraphCap::default());
        let hi_pos = out.find("hi[\"high\"]").unwrap();
        let lo_pos = out.find("lo[\"low\"]").unwrap();
        assert!(hi_pos < lo_pos, "higher score must appear first");
    }

    #[test]
    fn render_dedupes_nodes() {
        let graph = MemoryGraph {
            nodes: vec![
                GraphNode { id: "dup".into(), label: "first".into(), score: 1.0 },
                GraphNode { id: "dup".into(), label: "second".into(), score: 2.0 },
            ],
            edges: vec![],
        };
        let out = render_mermaid(&graph, GraphCap::default());
        let node_lines: Vec<_> = out.lines().filter(|l| l.contains("[\"")).collect();
        assert_eq!(node_lines.len(), 1);
    }

    #[test]
    fn render_collision_resolution() {
        // "a b" and "a_b" both sanitize to "a_b".
        let graph = MemoryGraph {
            nodes: vec![
                GraphNode { id: "a b".into(), label: "X".into(), score: 1.0 },
                GraphNode { id: "a_b".into(), label: "Y".into(), score: 2.0 },
            ],
            edges: vec![],
        };
        let out = render_mermaid(&graph, GraphCap::default());
        assert!(out.contains("a_b[\""));
        assert!(out.contains("a_b_1[\""));
    }

    #[test]
    fn render_caps_nodes_and_reports_omission() {
        let nodes: Vec<GraphNode> = (0..5)
            .map(|i| GraphNode { id: format!("n{i}"), label: format!("L{i}"), score: i as f64 })
            .collect();
        let graph = MemoryGraph { nodes, edges: vec![] };
        let out = render_mermaid(&graph, GraphCap { max_nodes: 2, max_edges: 10 });
        let node_lines: Vec<_> = out.lines().filter(|l| l.contains("[\"")).collect();
        assert_eq!(node_lines.len(), 2);
        assert!(out.contains("%% truncated: 3 nodes, 0 edges omitted"));
    }

    #[test]
    fn render_caps_edges_and_reports_omission() {
        let graph = MemoryGraph {
            nodes: vec![
                GraphNode { id: "a".into(), label: "A".into(), score: 1.0 },
                GraphNode { id: "b".into(), label: "B".into(), score: 2.0 },
            ],
            edges: vec![
                GraphEdge {
                    from: "a".into(),
                    to: "b".into(),
                    kind: MemoryLinkKind::Supports,
                    weight: 1.0,
                    score: 1.0,
                },
                GraphEdge {
                    from: "a".into(),
                    to: "b".into(),
                    kind: MemoryLinkKind::References,
                    weight: 1.0,
                    score: 0.6,
                },
            ],
        };
        let out = render_mermaid(&graph, GraphCap { max_nodes: 10, max_edges: 1 });
        let edge_lines: Vec<_> = out.lines().filter(|l| l.contains("-->|")).collect();
        assert_eq!(edge_lines.len(), 1);
        assert!(out.contains("%% truncated: 0 nodes, 1 edges omitted"));
    }

    #[test]
    fn render_edges_filtered_when_endpoint_missing() {
        let graph = MemoryGraph {
            nodes: vec![GraphNode { id: "a".into(), label: "A".into(), score: 1.0 }],
            edges: vec![GraphEdge {
                from: "a".into(),
                to: "missing".into(),
                kind: MemoryLinkKind::Supports,
                weight: 1.0,
                score: 1.0,
            }],
        };
        let out = render_mermaid(&graph, GraphCap::default());
        assert!(!out.contains("-->|"));
    }

    #[test]
    fn render_determinism_and_shuffle_invariance() {
        let graph = MemoryGraph {
            nodes: vec![
                GraphNode { id: "z".into(), label: "Z".into(), score: 2.0 },
                GraphNode { id: "a".into(), label: "A".into(), score: 1.0 },
            ],
            edges: vec![GraphEdge {
                from: "a".into(),
                to: "z".into(),
                kind: MemoryLinkKind::Supports,
                weight: 1.0,
                score: 1.0,
            }],
        };
        let out1 = render_mermaid(&graph, GraphCap::default());
        let reversed = MemoryGraph {
            nodes: vec![
                GraphNode { id: "a".into(), label: "A".into(), score: 1.0 },
                GraphNode { id: "z".into(), label: "Z".into(), score: 2.0 },
            ],
            edges: vec![GraphEdge {
                from: "a".into(),
                to: "z".into(),
                kind: MemoryLinkKind::Supports,
                weight: 1.0,
                score: 1.0,
            }],
        };
        let out2 = render_mermaid(&reversed, GraphCap::default());
        assert_eq!(out1, out2);
    }

    #[test]
    fn render_cycles_fine() {
        let graph = MemoryGraph {
            nodes: vec![
                GraphNode { id: "a".into(), label: "A".into(), score: 1.0 },
                GraphNode { id: "b".into(), label: "B".into(), score: 2.0 },
            ],
            edges: vec![
                GraphEdge {
                    from: "a".into(),
                    to: "b".into(),
                    kind: MemoryLinkKind::Supports,
                    weight: 1.0,
                    score: 1.0,
                },
                GraphEdge {
                    from: "b".into(),
                    to: "a".into(),
                    kind: MemoryLinkKind::Supports,
                    weight: 1.0,
                    score: 1.0,
                },
            ],
        };
        let out = render_mermaid(&graph, GraphCap::default());
        let edge_lines: Vec<_> = out.lines().filter(|l| l.contains("-->|")).collect();
        assert_eq!(edge_lines.len(), 2);
    }

    // -----------------------------------------------------------------------
    // load_memory_graph
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn load_missing_file_returns_empty() {
        let out = load_memory_graph(
            Path::new("/nonexistent/path/memory.db"),
            &test_project(),
            100,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(out, MemoryGraph::default());
    }

    #[tokio::test]
    async fn load_empty_db_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("empty.sqlite");
        let out = load_memory_graph(&db_path, &test_project(), 100, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out, MemoryGraph::default());
    }

    #[tokio::test]
    async fn load_graph_with_chunks_and_links() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("memory.db");
        let project_id = test_project();
        let (store, links) = seed_db(&db_path).await;

        let cancel = CancellationToken::new();
        store
            .store(
                &[
                    chunk_record(&project_id, "chunk-a", "Chunk A content alpha"),
                    chunk_record(&project_id, "chunk-b", "Chunk B content beta"),
                ],
                cancel.clone(),
            )
            .await
            .unwrap();
        links
            .put(&MemoryLink::new("chunk-a", "chunk-b", MemoryLinkKind::Supports), cancel.clone())
            .await
            .unwrap();

        let graph = load_memory_graph(&db_path, &project_id, 10, cancel.clone()).await.unwrap();
        assert_eq!(graph.nodes.len(), 2, "both chunks should appear as nodes");
        // Node score of chunk-b should be > 0 (it has an incoming Supports link).
        let b_node = graph.nodes.iter().find(|n| n.id == "chunk-b").unwrap();
        assert!(
            b_node.score > 0.0,
            "chunk-b score with Supports link must be > 0, got {}",
            b_node.score
        );
        assert_eq!(graph.edges.len(), 1, "one link from a to b");
        assert_eq!(graph.edges[0].from, "chunk-a");
        assert_eq!(graph.edges[0].to, "chunk-b");
        assert_eq!(graph.edges[0].kind, MemoryLinkKind::Supports);
    }

    #[tokio::test]
    async fn load_graph_excludes_fts_only_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("memory.db");
        let project_id = test_project();
        let (store, _links) = seed_db(&db_path).await;

        let cancel = CancellationToken::new();
        // Store a normal chunk and an FTS-only sentinel (empty vector).
        let mut sentinel = chunk_record(&project_id, "sentinel", "FTS only content");
        sentinel.vector = vec![];
        store
            .store(&[chunk_record(&project_id, "real", "Real content")], cancel.clone())
            .await
            .unwrap();
        store.store(&[sentinel], cancel.clone()).await.unwrap();

        let graph = load_memory_graph(&db_path, &project_id, 10, cancel.clone()).await.unwrap();
        assert_eq!(graph.nodes.len(), 1, "only the non-sentinel chunk");
        assert_eq!(graph.nodes[0].id, "real");
    }

    // -----------------------------------------------------------------------
    // load_chunk_detail
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn load_detail_missing_file_returns_none() {
        let out = load_chunk_detail(
            Path::new("/nonexistent/path/memory.db"),
            &test_project(),
            "no-such-chunk",
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn load_detail_unknown_chunk_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("memory.db");
        let project_id = test_project();
        let (_store, _links) = seed_db(&db_path).await;

        let detail =
            load_chunk_detail(&db_path, &project_id, "no-such-chunk", CancellationToken::new())
                .await
                .unwrap();
        assert!(detail.is_none());
    }

    #[tokio::test]
    async fn load_detail_with_links() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("memory.db");
        let project_id = test_project();
        let (store, links) = seed_db(&db_path).await;

        let cancel = CancellationToken::new();
        store
            .store(
                &[
                    chunk_record(&project_id, "src", "Source chunk"),
                    chunk_record(&project_id, "tgt", "Target chunk"),
                ],
                cancel.clone(),
            )
            .await
            .unwrap();
        links
            .put(&MemoryLink::new("src", "tgt", MemoryLinkKind::Supports), cancel.clone())
            .await
            .unwrap();

        let detail = load_chunk_detail(&db_path, &project_id, "tgt", cancel.clone())
            .await
            .unwrap()
            .expect("tgt exists");
        assert_eq!(detail.chunk_id, "tgt");
        assert_eq!(detail.content, "Target chunk");
        assert_eq!(detail.in_links.len(), 1);
        assert_eq!(detail.in_links[0].from, "src");
        assert_eq!(detail.in_links[0].kind, MemoryLinkKind::Supports);
        assert!(detail.score > 0.0);
        assert!(detail.out_links.is_empty(), "tgt has no outgoing links");
    }
}
