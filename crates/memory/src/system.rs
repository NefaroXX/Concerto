//! Phase 4 integrated memory system.
//!
//! `MemorySystem` wraps all Phase 4 components into a single
//! `concerto_core::traits::memory::MemoryStore` implementation that the
//! orchestrator can use as a drop-in replacement for Phase 3's basic
//! working memory.

use camino::Utf8PathBuf;
use glob::{MatchOptions, Pattern};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use concerto_core::error::MemoryError as CoreMemoryError;
use concerto_core::memory::{
    ChunkType, EmbeddingRecord, MemoryChunk, MemoryEntry, MemoryFilter, MemoryId, MemoryLink,
    MemoryLinkKind, MemoryNamespace, MemoryQuery, ProjectId,
};
use concerto_core::traits::memory::MemoryStore;
use concerto_core::types::{Message, Role};
use concerto_core::CancellationToken;

use tracing;

use crate::budget::ContextBudgetAllocator;
use crate::decision_store::DecisionStore;
use crate::embedder::EmbeddingGenerator;
use crate::embedder_health::EmbedderHealth;
use crate::entities::{L1Candidate, L1DedupJudge, L1DedupVerdict, L1ExtractedMemory, L1Extractor};
use crate::evidence::{enrich_with_standing, EvidenceStamps};
use crate::fts::FullTextStore;
use crate::global::GlobalMemoryStore;
use crate::links::{links_from_metadata, LinkStore};
use crate::rag::{FusedResult, HybridRetriever, LinkCascadeConfig, LinkScorer};
use crate::recall::{RecallCapabilityEnvelope, RecallCostBounds};
use crate::recall_status::{
    ChunkStanding, CitedDecision, DerivationContext, PathObservation, PathObservationSource,
    STORED_ENTRY_PATH,
};
use crate::scoring::{chunk_link_score, TimedLink, DECAY_FLOOR_DAYS};
use crate::sync::ChunkSyncService;
use crate::task_tree::TaskTreeStore;
use crate::vector_store::VectorStore;

use concerto_core::memory::{DecisionId, EvidentChunk};

/// How many vector candidates the L1 dedup judge may compare against.
///
/// Kept small: the judge only needs the handful of genuinely similar chunks
/// (see [`HybridRetriever::recall_candidates`]).
const DEDUP_VECTOR_TOP_K: usize = 5;
/// How many full-text candidates the L1 dedup judge may compare against.
const DEDUP_FTS_TOP_K: usize = 5;

/// ADR-69 slice 2 link scorer backed by the real link store.
///
/// Scores each chunk id from its INCOMING `memory_links` rows via
/// [`chunk_link_score`] (kind weights × decay). This is the production
/// [`LinkScorer`] the memory system's cascade uses; failures surface as
/// errors so the rag layer's fail-open keeps results unchanged.
pub struct StoreLinkScorer {
    link_store: Arc<LinkStore>,
    /// Decay window in days; `None` disables decay (`Some(0)` also disables).
    decay_days: Option<u16>,
}

impl StoreLinkScorer {
    /// Panics-free constructor; the scorer is inert until the link store
    /// answers.
    pub fn new(link_store: Arc<LinkStore>, decay_days: Option<u16>) -> Self {
        Self { link_store, decay_days }
    }
}

#[async_trait]
impl LinkScorer for StoreLinkScorer {
    async fn link_scores(
        &self,
        chunk_ids: &[String],
        cancel: CancellationToken,
    ) -> Result<HashMap<String, f64>, CoreMemoryError> {
        let incoming = self.link_store.incoming_links(chunk_ids, cancel).await?;
        let now_unix = time::OffsetDateTime::now_utc().unix_timestamp();
        Ok(chunk_ids
            .iter()
            .map(|id| {
                let score = incoming
                    .get(id)
                    .map(|links| chunk_link_score(links, self.decay_days, now_unix))
                    .unwrap_or(0.0);
                (id.clone(), score)
            })
            .collect())
    }
}

/// Outcome counters for the L1 dedup judge pass (ADR-46 symbolic offload).
///
/// Only judge-consulted store calls are counted: a store call that never
/// reached the judge (no judge configured, or recall produced no candidates)
/// leaves every counter untouched. `failures` counts every fail-open pass that
/// skipped the judge without a verdict — a cancelled pass, a judge error, or an
/// unresolvable merge target — which is exactly the signal that dedup silently
/// degraded to a plain store.
#[derive(Debug, Default)]
pub struct DedupJudgeCounters {
    /// The judge ruled `store` — keep the new memory as its own chunk.
    pub store: AtomicUsize,
    /// The judge ruled `skip` — the entry is already represented.
    pub skip: AtomicUsize,
    /// The judge ruled `update` — supersede the target chunk.
    pub update: AtomicUsize,
    /// The judge ruled `merge` — fold the entry into the target chunk.
    pub merge: AtomicUsize,
    /// Fail-open passes where the judge produced no verdict.
    pub failures: AtomicUsize,
}

/// Outcome counters for the L1 typed-extraction pass (TODO.md "L1 typed
/// extraction").
///
/// Only conversation-derived `SessionSummary` stores run the pass, so a
/// store call that never reached the extractor (no extractor configured, or
/// a non-summary entry) leaves every counter untouched. `failures` counts
/// every fail-soft pass — an extractor error or a derived-write error — which
/// is exactly the signal that typed extraction silently degraded to the plain
/// parent store.
#[derive(Debug, Default)]
pub struct L1ExtractCounters {
    /// Typed memories derived from a stored slice and written through `store`.
    pub extracted: AtomicUsize,
    /// Fail-soft passes (extraction error or a derived-write error).
    pub failures: AtomicUsize,
}

/// The integrated Phase 4 memory system.
///
/// Wraps all memory layers and presents a unified `MemoryStore` interface.
pub struct MemorySystem {
    retriever: HybridRetriever,
    sync: ChunkSyncService,
    decision_store: Arc<DecisionStore>,
    task_tree: Arc<TaskTreeStore>,
    budget: ContextBudgetAllocator,
    embedder: Option<Arc<dyn EmbeddingGenerator>>,
    project_id: ProjectId,
    /// Optional global (user-scoped) memory store backed by a separate
    /// SQLite database. When present, `Global` namespace queries and
    /// writes are routed here instead of the project-scoped stores.
    global_store: Option<Arc<GlobalMemoryStore>>,
    /// Optional L1 dedup judge (ADR-46 symbolic offload). When set, every
    /// project-namespace `store` first recalls the top similar chunks and
    /// asks the judge whether to store / update / merge / skip. Fail-open:
    /// judge or recall errors store the entry unchanged.
    dedup_judge: Option<L1DedupJudge>,
    /// L1 dedup judge outcome counters (see [`DedupJudgeCounters`]).
    dedup_counters: DedupJudgeCounters,
    /// Optional symbolic link store (ADR-69 slice 1). When set, `store`
    /// persists `memory_links` rows on the L1 dedup update/merge paths.
    /// Fail-open: a link-store error logs a warning and never fails the
    /// memory store; a missing store simply writes no links.
    link_store: Option<Arc<LinkStore>>,
    /// Optional ADR-69 slice 2 link cascade. When BOTH this and `link_store`
    /// are set, `retrieve` re-ranks the fused top-k by link evidence through
    /// [`StoreLinkScorer`]; when either is missing, `retrieve` keeps the
    /// plain RRF path (unchanged).
    link_cascade: Option<LinkCascadeConfig>,
    /// Optional L1 typed extractor (TODO.md "L1 typed extraction"). When set,
    /// storing a conversation-derived `SessionSummary` slice additionally
    /// extracts typed long-horizon memories (`persona` / `episodic` /
    /// `instruction` / `work_fact`) and writes EACH of them through the same
    /// `store` path, so the dedup judge, ADR-69 links, the embedder and FTS
    /// all treat derived memories exactly like hand-written ones. Fail-soft:
    /// an extractor or derived-write error logs a warning and never changes
    /// the parent store's result. Recursion is impossible — derived memories
    /// are `Fact` entries, which never re-enter the pass.
    l1_extractor: Option<L1Extractor>,
    /// L1 typed-extraction outcome counters (see [`L1ExtractCounters`]).
    l1_counters: L1ExtractCounters,
    /// Optional resource-fact observation seam for evidence-bearing recall
    /// (H11). When set, [`MemoryStore::retrieve_with_evidence`] consults
    /// the provider for the recalled chunks' real file paths, so a clean
    /// runtime observation can verify (and a dirty one stale) an item.
    /// Unset reads the observation axis as unknown — never as fine. The
    /// seam is infallible by contract (fail-open inside the provider).
    /// Behind a lock so the seam can be attached post-construction through
    /// the erased `Arc<dyn MemoryStore>` handle (see
    /// [`MemoryStore::attach_path_observations`]) — the critical section
    /// only swaps an `Arc`, never `.await`s under the guard.
    path_observations: std::sync::RwLock<Option<Arc<dyn PathObservationSource>>>,
}

impl MemorySystem {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        vector_store: Arc<dyn VectorStore>,
        fts_store: Arc<dyn FullTextStore>,
        decision_store: Arc<DecisionStore>,
        task_tree: Arc<TaskTreeStore>,
        embedder: Option<Arc<dyn EmbeddingGenerator>>,
        project_id: ProjectId,
        global_store: Option<Arc<GlobalMemoryStore>>,
    ) -> Self {
        let sync = ChunkSyncService::new(vector_store.clone(), fts_store.clone());
        let retriever = HybridRetriever::new(vector_store, fts_store);
        let budget = ContextBudgetAllocator::default();
        Self {
            retriever,
            sync,
            decision_store,
            task_tree,
            budget,
            embedder,
            project_id,
            global_store,
            dedup_judge: None,
            dedup_counters: DedupJudgeCounters::default(),
            link_store: None,
            link_cascade: None,
            l1_extractor: None,
            l1_counters: L1ExtractCounters::default(),
            path_observations: std::sync::RwLock::new(None),
        }
    }
    /// Access the decision store.
    ///
    /// Returns the shared `Arc` so a caller that also holds the same handle
    /// (e.g. the coordinator's Phase 6 M3 write-back/read-back wiring) reads
    /// and writes the SAME in-memory store — never a divergent copy.
    pub fn decisions(&self) -> &Arc<DecisionStore> {
        &self.decision_store
    }
    /// Access the task tree store.
    ///
    /// Returns the shared `Arc` (see [`Self::decisions`]).
    pub fn task_tree(&self) -> &Arc<TaskTreeStore> {
        &self.task_tree
    }
    /// Access the RAG context budget allocator.
    ///
    /// RAG-only (ADR-16/ADR-48): the allocator provides the score-ordered RAG
    /// bound via `truncate_to_rag_limit`. Working-memory and conversation
    /// budgets are owned by the orchestrator's `ContextEngine` +
    /// `ContextGuardProvider`, not by this system. `retrieve`/`store` never
    /// apply the allocator themselves — the RAG bound is applied by the caller
    /// that injects retrieved chunks into the prompt.
    pub fn budget(&self) -> &ContextBudgetAllocator {
        &self.budget
    }
    /// The project ID this system is scoped to.
    pub fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    /// Access the embedder, if configured.
    pub fn embedder(&self) -> Option<&Arc<dyn EmbeddingGenerator>> {
        self.embedder.as_ref()
    }

    /// Enable the L1 dedup judge for [`MemoryStore::store`] (ADR-46
    /// symbolic offload).
    ///
    /// When configured, every project-namespace `store` first recalls the
    /// top similar chunks and asks `judge` whether the new entry should be
    /// stored as-is, skipped (duplicate), stored after superseding an
    /// existing chunk (`update`), or merged with one (`merge`). The judge is
    /// extra latency per write and is advisory-only and fail-open: any
    /// judge / recall error logs a warning and stores the entry unchanged.
    pub fn with_dedup_judge(mut self, judge: L1DedupJudge) -> Self {
        self.dedup_judge = Some(judge);
        self
    }

    /// Enable the L1 typed extractor for conversation-derived stores (TODO.md
    /// "L1 typed extraction").
    ///
    /// When configured, every successful project-namespace store of a
    /// `SessionSummary` entry (the conversation-derived slice written at task
    /// completion) additionally extracts typed long-horizon memories and
    /// writes each through [`MemoryStore::store`], so the ADR-46 dedup judge
    /// and ADR-69 links apply to derived memories too. The pass is opt-in
    /// (nothing selects it unless attached here) and fail-soft: an extractor
    /// or derived-write error logs a warning, bumps
    /// [`L1ExtractCounters::failures`], and leaves the parent store's result
    /// untouched.
    pub fn with_l1_extractor(mut self, extractor: L1Extractor) -> Self {
        self.l1_extractor = Some(extractor);
        self
    }

    /// Enable the symbolic link store for [`MemoryStore::store`] (ADR-69
    /// slice 1).
    ///
    /// When configured, every project-namespace `store` that resolves to an
    /// L1 dedup update/merge verdict persists `memory_links` rows — the
    /// verdict's own supersession link plus any `References` links derived
    /// from `refs` / `result_ref` metadata carried on those paths. The link
    /// write happens AFTER the chunk is stored and BEFORE the target is
    /// tombstoned, and is fail-open: any link-store error logs a warning and
    /// the memory is kept unchanged.
    ///
    /// Production reachability: the orchestrator bootstrap attaches the
    /// store on every path that builds a project memory system; it stays
    /// detached ONLY when the memory DB's own link-table DDL failed
    /// (fail-open `None`). So link-substrate verdicts (`Contradicted`,
    /// `Superseded`, corroboration) are live in production whenever
    /// memory init itself succeeded.
    pub fn with_link_store(mut self, link_store: Arc<LinkStore>) -> Self {
        self.link_store = Some(link_store);
        self
    }

    /// Enable the ADR-69 slice 2 link cascade for [`MemoryStore::retrieve`].
    ///
    /// This is how `retrieve` consumes link evidence: the fused top-k is
    /// re-ranked by `rank + γ·(10 − score)`, where the score comes from
    /// [`StoreLinkScorer`] over the configured link store and the tier γ
    /// comes from this config. Both this and [`Self::with_link_store`] must
    /// be set for the cascade to engage — otherwise `retrieve` is the plain
    /// RRF path, unchanged. Fail-open: link-store errors, scoring errors, or
    /// scoring timeouts leave the fused order untouched.
    pub fn with_link_cascade(mut self, config: LinkCascadeConfig) -> Self {
        self.link_cascade = Some(config);
        self
    }

    /// Set the recall cost bounds for [`MemoryStore::retrieve`] — the char
    /// caps (distributed across the selected results) and the per-recall
    /// timeout guard (TODO.md "Recall budget caps + timeout guard"). Default:
    /// unlimited chars and a 5000 ms timeout that skips a breached recall
    /// with a warning instead of blocking the turn.
    pub fn with_recall_bounds(mut self, bounds: RecallCostBounds) -> Self {
        self.retriever = self.retriever.with_recall_bounds(bounds);
        self
    }

    /// Attach the resource-fact observation seam for evidence-bearing
    /// recall (H11).
    ///
    /// The provider is consulted once per recall for the recalled chunks'
    /// real file paths (bounded by `top_k`); a clean observation verifies
    /// an item and a dirty one stales it, both with the observation cited
    /// in the derivation. Fail-open throughout: provider gaps read as
    /// unknown. Without a provider the observation axis is always unknown.
    /// Builder form of [`MemoryStore::attach_path_observations`] for call
    /// sites that still own the concrete system.
    pub fn with_path_observations(self, source: Arc<dyn PathObservationSource>) -> Self {
        self.attach_path_observations(source);
        self
    }

    /// The declared recall cost bounds of this system (capability envelope).
    pub fn recall_capability(&self) -> RecallCapabilityEnvelope {
        self.retriever.recall_capability()
    }

    /// How many recall passes were skipped by the timeout guard.
    pub fn recalls_skipped(&self) -> usize {
        self.retriever.recalls_skipped()
    }

    /// Outcome counters for the L1 dedup judge pass (verdicts + fail-open
    /// passes). Exposed for observability and tests: a caller holding the
    /// concrete `MemorySystem` can read the counts without any metrics
    /// infrastructure.
    pub fn dedup_counters(&self) -> &DedupJudgeCounters {
        &self.dedup_counters
    }

    /// Outcome counters for the L1 typed-extraction pass (see
    /// [`L1ExtractCounters`]). Exposed for observability and tests.
    pub fn l1_counters(&self) -> &L1ExtractCounters {
        &self.l1_counters
    }

    /// Derive typed L1 memories from a stored conversation-derived slice.
    ///
    /// Live seam for TODO.md "L1 typed extraction": only `SessionSummary`
    /// entries (the conversation-derived slice written at task completion)
    /// are extracted, and each derived memory is written through the SAME
    /// [`MemoryStore::store`] path, so the dedup judge, ADR-69 links, the
    /// embedder and FTS apply to derived memories exactly as to hand-written
    /// ones. Derived memories are `Fact` entries, so the pass cannot recurse.
    ///
    /// Fail-soft by contract: no extractor, a non-summary entry, a cancelled
    /// token, an extractor error, or a derived-write error logs at most a
    /// warning and leaves the parent store's result untouched.
    async fn extract_l1_memories(&self, entry: &MemoryEntry, cancel: CancellationToken) {
        let Some(extractor) = self.l1_extractor.as_ref() else {
            return;
        };
        if entry.chunk_type != ChunkType::SessionSummary || cancel.is_cancelled() {
            return;
        }
        let messages = vec![Message {
            role: Role::User,
            content: entry.content.clone(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        }];
        let extracted = match extractor.extract(&messages).await {
            Ok(memories) => memories,
            Err(error) => {
                self.l1_counters.failures.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(%error, entry = %entry.id.0, "l1 typed extraction failed; continuing");
                return;
            }
        };
        for memory in extracted {
            if cancel.is_cancelled() {
                return;
            }
            let derived = derived_l1_entry(entry, &memory);
            match <Self as MemoryStore>::store(self, derived, cancel.clone()).await {
                Ok(_) => {
                    self.l1_counters.extracted.fetch_add(1, Ordering::Relaxed);
                }
                Err(error) => {
                    self.l1_counters.failures.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        %error,
                        kind = ?memory.kind,
                        "failed to store L1 typed memory; continuing"
                    );
                }
            }
        }
    }

    /// Compute the query embedding for recall, with the ADR-39 degraded
    /// contract: an embedder failure records the broken window (so the
    /// retriever's FTS-only fallback and its notice engage) and yields an
    /// empty vector; a missing embedder means vector-disabled retrieval.
    async fn embed_query_text(&self, query: &MemoryQuery) -> Vec<f32> {
        if let Some(ref embedder) = self.embedder {
            match embedder.embed(&query.text).await {
                Ok(embedding) => embedding,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "query embedding unavailable; falling back to full-text memory retrieval"
                    );
                    // S1 (ADR-39): record the failure on the QUERY path too, so
                    // a broken embedder is registered even with no concurrent
                    // indexing. This makes the retriever's degraded (FTS-only)
                    // fallback and its notice reachable. `record_failure`
                    // returns `Some` only on the first failure of a (new)
                    // broken window — exactly one transition per window.
                    let health = EmbedderHealth::for_project(&query.project_id);
                    let _ = health.record_failure(std::time::Instant::now());
                    Vec::new()
                }
            }
        } else {
            tracing::trace!("vector search disabled: no embedder configured");
            Vec::new()
        }
    }

    /// Assemble one [`ChunkStanding`] per recalled chunk (H11): the TTL +
    /// authorship substrate from row provenance, the link substrate from
    /// the link store, decision supersession for cited decisions, and
    /// resource-fact observations through the seam.
    ///
    /// Fail-open throughout: a cancelled token, a missing store, or any
    /// lookup error yields missing standings, and the enrichment reads
    /// those chunks as Unknown — recall is advisory and must never fail
    /// the turn for want of standing. Every lookup is
    /// bounded by the recalled ids (at most one recall's `top_k`); the
    /// link read batches internally, and maintenance stays untouched
    /// (the existing `recall_capability` / `recalls_skipped` surface is
    /// neither consumed nor altered here).
    async fn assemble_standings(
        &self,
        chunks: &[MemoryChunk],
        cancel: &CancellationToken,
    ) -> HashMap<String, ChunkStanding> {
        let mut standings: HashMap<String, ChunkStanding> = HashMap::new();
        if chunks.is_empty() || cancel.is_cancelled() {
            return standings;
        }
        let ids: Vec<String> = chunks.iter().map(|chunk| chunk.id.clone()).collect();
        let provenance =
            self.retriever.chunk_provenance(&self.project_id, &ids, cancel.clone()).await;
        let incoming: HashMap<String, Vec<TimedLink>> = match self.link_store.as_ref() {
            Some(store) => match store.incoming_links(&ids, cancel.clone()).await {
                Ok(map) => map,
                Err(error) => {
                    tracing::debug!(
                        %error,
                        "recall standing: link substrate unavailable (fail-open)"
                    );
                    HashMap::new()
                }
            },
            None => HashMap::new(),
        };
        // The seam is keyed by real file paths: stored entries (the
        // `STORED_ENTRY_PATH` marker) and pathless chunks are never looked
        // up — there is no workspace observation for them to match.
        let mut path_to_ids: HashMap<String, Vec<String>> = HashMap::new();
        for chunk in chunks {
            if let Some(path) = chunk.file_path.as_ref() {
                let path = path.as_str();
                if !path.is_empty() && path != STORED_ENTRY_PATH {
                    path_to_ids.entry(path.to_string()).or_default().push(chunk.id.clone());
                }
            }
        }
        // Clone the provider out first: the guard must not be held across
        // the `.await` below (`RwLockReadGuard` is not `Send`).
        let source =
            self.path_observations.read().unwrap_or_else(|poison| poison.into_inner()).clone();
        let observations: HashMap<String, PathObservation> = match source {
            Some(source) if !path_to_ids.is_empty() => {
                let paths: Vec<String> = path_to_ids.keys().cloned().collect();
                source.observe_paths(&paths, cancel.clone()).await
            }
            _ => HashMap::new(),
        };
        for chunk in chunks {
            if cancel.is_cancelled() {
                break;
            }
            let proof = provenance.get(&chunk.id);
            let metadata = proof.and_then(|proof| proof.metadata.clone());
            let cited_decision = metadata
                .as_ref()
                .and_then(|metadata| metadata.get("decision_id"))
                .and_then(|value| value.as_str())
                .and_then(|id| self.decision_superseded(id));
            let path_observation =
                chunk.file_path.as_ref().and_then(|path| observations.get(path.as_str()).cloned());
            standings.insert(
                chunk.id.clone(),
                ChunkStanding {
                    chunk_type: chunk.chunk_type,
                    file_path: chunk.file_path.as_ref().map(|path| path.as_str().to_string()),
                    stale_model: chunk.stale,
                    created_at: proof.and_then(|proof| proof.created_at),
                    metadata,
                    incoming: incoming.get(&chunk.id).cloned().unwrap_or_default(),
                    cited_decision,
                    path_observation,
                },
            );
        }
        standings
    }

    /// Resolve a metadata-cited decision id to its supersession flag.
    /// `None` when the metadata names no parseable, known decision — a
    /// missing or broken cite changes nothing (fail-open).
    fn decision_superseded(&self, decision_id: &str) -> Option<CitedDecision> {
        let ulid = concerto_core::ids::Ulid::from_string(decision_id).ok()?;
        let decision = self.decision_store.get(DecisionId(ulid)).ok()??;
        Some(CitedDecision {
            id: decision_id.to_string(),
            superseded: decision.superseded_by.is_some(),
        })
    }

    /// Decide how a project-namespace entry relates to already-stored chunks.
    ///
    /// Fail-open by construction: without a judge, on cancellation, when
    /// recall yields no candidates, or when the judge errors, the entry is
    /// stored unchanged as its own chunk.
    async fn decide_dedup(
        &self,
        entry: &MemoryEntry,
        vector: &[f32],
        cancel: CancellationToken,
    ) -> DedupDecision {
        let Some(judge) = self.dedup_judge.as_ref() else {
            return DedupDecision::store(entry, vector);
        };
        if cancel.is_cancelled() {
            // The dedup pass was cancelled — fail open to a plain store,
            // and surface the pass as a counter so a cancelled worker that
            // keeps dropping the judge shows up in observability.
            self.dedup_counters.failures.fetch_add(1, Ordering::Relaxed);
            tracing::info!("l1 dedup cancelled; storing memory unjudged");
            return DedupDecision::store(entry, vector);
        }

        let recalled = self
            .retriever
            .recall_candidates(
                &self.project_id,
                &entry.content,
                vector,
                DEDUP_VECTOR_TOP_K,
                DEDUP_FTS_TOP_K,
                cancel.clone(),
            )
            .await;
        if recalled.is_empty() {
            // Nothing to compare against — no duplication possible. Not
            // counted: the judge was never consulted.
            return DedupDecision::store(entry, vector);
        }
        let candidate_count = recalled.len();
        let candidates: Vec<L1Candidate> = recalled
            .into_iter()
            .map(|candidate| L1Candidate {
                chunk_id: candidate.chunk_id,
                content: candidate.content,
            })
            .collect();

        let verdict = match judge.judge(&entry.content, &candidates).await {
            Ok(verdict) => verdict,
            Err(error) => {
                self.dedup_counters.failures.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    %error,
                    candidates = candidate_count,
                    "l1 dedup judge failed; storing memory unchanged"
                );
                return DedupDecision::store(entry, vector);
            }
        };

        match verdict {
            L1DedupVerdict::Store => {
                self.dedup_counters.store.fetch_add(1, Ordering::Relaxed);
                tracing::info!(candidates = candidate_count, "l1 dedup: store");
                DedupDecision::store(entry, vector)
            }
            L1DedupVerdict::Skip => {
                self.dedup_counters.skip.fetch_add(1, Ordering::Relaxed);
                tracing::info!(candidates = candidate_count, "l1 dedup: skip duplicate");
                DedupDecision::Skip
            }
            L1DedupVerdict::Update { target_id } => {
                self.dedup_counters.update.fetch_add(1, Ordering::Relaxed);
                tracing::info!(
                    target_id = %target_id,
                    candidates = candidate_count,
                    "l1 dedup: update"
                );
                DedupDecision::Store {
                    content: entry.content.clone(),
                    vector: vector.to_vec(),
                    tombstone_after: Some(target_id.clone()),
                    links: dedup_links(entry, MemoryLinkKind::Supersedes, &target_id),
                }
            }
            L1DedupVerdict::Merge { target_id } => {
                let Some(target) =
                    candidates.iter().find(|candidate| candidate.chunk_id == target_id)
                else {
                    self.dedup_counters.failures.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        target_id,
                        candidates = candidate_count,
                        "l1 dedup merge target missing from candidates; storing unchanged"
                    );
                    return DedupDecision::store(entry, vector);
                };
                let merged_content = format!("{}\n{}", entry.content, target.content);
                let merged_vector = if let Some(embedder) = self.embedder.as_ref() {
                    match embedder.embed(&merged_content).await {
                        Ok(embedding) => embedding,
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                "l1 dedup merge re-embedding failed; keeping original vector"
                            );
                            vector.to_vec()
                        }
                    }
                } else {
                    Vec::new()
                };
                self.dedup_counters.merge.fetch_add(1, Ordering::Relaxed);
                tracing::info!(
                    target_id = %target_id,
                    candidates = candidate_count,
                    "l1 dedup: merge"
                );
                DedupDecision::Store {
                    content: merged_content,
                    vector: merged_vector,
                    tombstone_after: Some(target_id.clone()),
                    links: dedup_links(entry, MemoryLinkKind::Merges, &target_id),
                }
            }
        }
    }
}

/// Link rows to persist for an L1 dedup update/merge verdict (ADR-69 slice
/// 1): the verdict's own supersession link plus any `References` links the
/// entry metadata carries (`refs` array / `result_ref`).
///
/// Only ever called on the update/merge paths — plain stores write no links.
fn dedup_links(entry: &MemoryEntry, kind: MemoryLinkKind, target_id: &str) -> Vec<MemoryLink> {
    let from = entry.id.0.to_string();
    let mut links = vec![MemoryLink::new(&from, target_id, kind)];
    links.extend(links_from_metadata(&from, &entry.metadata));
    links
}

/// Outcome of the L1 dedup pass for one store call.
enum DedupDecision {
    /// The judge ruled the entry is already represented; the store is
    /// skipped entirely (the existing chunk stays canonical).
    Skip,
    /// Write the entry with the given (possibly merged) content/vector, then
    /// supersede `tombstone_after` — if any — AFTER the write so a failure
    /// can never leave the only copy of a memory gone.
    ///
    /// `links` (ADR-69 slice 1) are the symbolic links to persist for the
    /// verdict: empty on plain stores, populated only on the update/merge
    /// paths.
    Store {
        content: String,
        vector: Vec<f32>,
        tombstone_after: Option<String>,
        links: Vec<MemoryLink>,
    },
}

impl DedupDecision {
    /// The default fail-open decision: store the entry unchanged.
    fn store(entry: &MemoryEntry, vector: &[f32]) -> Self {
        DedupDecision::Store {
            content: entry.content.clone(),
            vector: vector.to_vec(),
            tombstone_after: None,
            links: Vec::new(),
        }
    }
}

#[async_trait]

impl MemoryStore for MemorySystem {
    /// ADR-69 slice 1 observability: report whether this system carries a
    /// symbolic link store. Defaults to `false` on the trait (fail-open:
    /// plain stores write no links, and tests that do not opt in keep
    /// fanning out plain); this override surfaces the production wiring so
    /// the init path can be asserted through an `Arc<dyn MemoryStore>`
    /// handle without downcasting.
    fn link_store_attached(&self) -> bool {
        self.link_store.is_some()
    }

    fn attach_path_observations(&self, source: Arc<dyn PathObservationSource>) {
        *self.path_observations.write().unwrap_or_else(|poison| poison.into_inner()) = Some(source);
    }

    async fn retrieve(
        &self,
        query: &MemoryQuery,
        cancel: CancellationToken,
    ) -> Result<Vec<MemoryChunk>, CoreMemoryError> {
        // Projection of the evidence-bearing path: the same fetch, order,
        // and bounds — agents that need source attribution and status call
        // `retrieve_with_evidence` instead. There is one recall
        // implementation, not an evidence-free second path.
        Ok(self
            .retrieve_with_evidence(query, cancel)
            .await?
            .into_iter()
            .map(|evident| evident.chunk)
            .collect())
    }

    async fn retrieve_with_evidence(
        &self,
        query: &MemoryQuery,
        cancel: CancellationToken,
    ) -> Result<Vec<EvidentChunk>, CoreMemoryError> {
        // Route global namespace queries to the global store (its default
        // evidence projection applies there: user-scoped facts recall as
        // model-authored claims).
        if matches!(query.namespace, MemoryNamespace::Global { .. }) {
            return match self.global_store {
                Some(ref store) => store.retrieve_with_evidence(query, cancel).await,
                None => Ok(Vec::new()),
            };
        }

        validate_scope(&self.project_id, &query.project_id, &query.namespace)?;

        let embedding = self.embed_query_text(query).await;
        // One assembly instant for the composites and their derivation
        // context, so evidence and verdicts can never disagree on "now".
        // `snapshot_generation` is 0 (unknown): the memory system holds no
        // workspace generation; consumers track their own for drift
        // detection against `observed_at`.
        let observed_at = time::OffsetDateTime::now_utc();
        let (results, poor) =
            if let (Some(link_store), Some(config)) = (&self.link_store, &self.link_cascade) {
                let scorer = StoreLinkScorer::new(link_store.clone(), config.decay_days);
                self.retriever
                    .retrieve_with_cascade_evidence(
                        query,
                        &embedding,
                        Some(query.top_k),
                        config.start_tier,
                        &scorer,
                        config,
                        0,
                        cancel.clone(),
                    )
                    .await
            } else {
                self.retriever
                    .retrieve_with_evidence(query, &embedding, Some(query.top_k), 0, cancel.clone())
                    .await
            };
        // The corroboration map the reorder cited, recovered from the
        // aligned composites (clamp-idempotent: re-clamping the clamped
        // score is exact, so the refined enrichment cites identical
        // values).
        let scores: HashMap<String, f64> = poor
            .iter()
            .map(|evidence| (evidence.chunk_id.clone(), evidence.evidence_score.value()))
            .collect();

        let ids: Vec<String> = results.iter().map(|result| result.chunk_id.clone()).collect();
        let fused_by_id: HashMap<String, FusedResult> =
            results.into_iter().map(|result| (result.chunk_id.clone(), result)).collect();
        let metadata = self.retriever.load_chunks(&query.project_id, &ids, cancel.clone()).await?;
        let metadata_is_authoritative = self.retriever.supports_chunk_metadata();
        let mut metadata_by_id: HashMap<String, MemoryChunk> =
            metadata.into_iter().map(|chunk| (chunk.id.clone(), chunk)).collect();
        let file_patterns = compile_file_patterns(&query.filters)?;

        // Overlay + filter exactly like the historical `retrieve`, keeping
        // each surviving chunk beside its fused result (relevance needs the
        // fused score; standing needs the chunk).
        let mut kept: Vec<(MemoryChunk, FusedResult)> = Vec::new();
        for id in &ids {
            let Some(result) = fused_by_id.get(id) else {
                continue;
            };
            let mut chunk = match metadata_by_id.remove(id) {
                Some(chunk) => chunk,
                None if metadata_is_authoritative => continue,
                None => MemoryChunk {
                    id: result.chunk_id.clone(),
                    project_id: query.project_id.clone(),
                    namespace: MemoryNamespace::Project(query.project_id.clone()),
                    content: result.content.clone(),
                    file_path: None,
                    start_line: None,
                    end_line: None,
                    chunk_type: ChunkType::SlidingWindow,
                    score: result.score,
                    model_id: String::new(),
                    model_version: String::new(),
                    stale: result.stale,
                },
            };
            chunk.score = result.score;
            chunk.content.clone_from(&result.content);
            chunk.stale = result.stale;
            if matches_filters(&chunk, &query.filters, &file_patterns) {
                kept.push((chunk, result.clone()));
            }
        }

        let chunks: Vec<MemoryChunk> = kept.iter().map(|(chunk, _)| chunk.clone()).collect();
        let standings = self.assemble_standings(&chunks, &cancel).await;
        let stamps = EvidenceStamps { snapshot_generation: 0, observed_at };
        let ctx = DerivationContext {
            decay_days: match &self.link_cascade {
                // A configured window — including an explicit `None`
                // (decay disabled) — is respected as configured.
                Some(config) => config.decay_days,
                // Unconfigured: the ADR-69 A5 floor, matching the cascade
                // default and the retriever-level enrichment.
                None => Some(DECAY_FLOOR_DAYS as u16),
            },
            now_unix: observed_at.unix_timestamp(),
            // The memory system holds no workspace generation (see above).
            current_generation: None,
            // Read AFTER standing assembly: a cancellation that landed
            // mid-assembly leaves partial or empty standing, and the
            // composites must say so (machine-readable `interrupted`,
            // never silent Unknown).
            interrupted: cancel.is_cancelled(),
        };
        let fused: Vec<FusedResult> = kept.iter().map(|(_, result)| result.clone()).collect();
        let rich = enrich_with_standing(&fused, &scores, stamps, &standings, &ctx);
        Ok(chunks
            .into_iter()
            .zip(rich)
            .map(|(chunk, evidence)| evidence.to_evident_chunk(chunk))
            .collect())
    }

    async fn browse(
        &self,
        project_id: &ProjectId,
        top_k: usize,
        _cancel: CancellationToken,
    ) -> Result<Vec<MemoryChunk>, CoreMemoryError> {
        if project_id != &self.project_id {
            return Err(CoreMemoryError::CrossProjectLeakage);
        }
        let results = self.retriever.browse(project_id, top_k, _cancel.clone()).await;
        let ids: Vec<String> = results.iter().map(|result| result.chunk_id.clone()).collect();
        let metadata_is_authoritative = self.retriever.supports_chunk_metadata();
        let mut metadata_by_id: HashMap<String, MemoryChunk> = self
            .retriever
            .load_chunks(project_id, &ids, _cancel.clone())
            .await?
            .into_iter()
            .map(|chunk| (chunk.id.clone(), chunk))
            .collect();
        Ok(results
            .into_iter()
            .filter_map(|result| {
                let mut chunk = match metadata_by_id.remove(&result.chunk_id) {
                    Some(chunk) => chunk,
                    None if metadata_is_authoritative => return None,
                    None => MemoryChunk {
                        id: result.chunk_id.clone(),
                        project_id: project_id.clone(),
                        namespace: MemoryNamespace::Project(project_id.clone()),
                        content: result.content.clone(),
                        file_path: None,
                        start_line: None,
                        end_line: None,
                        chunk_type: ChunkType::SlidingWindow,
                        score: result.score,
                        model_id: String::new(),
                        model_version: String::new(),
                        stale: result.stale,
                    },
                };
                chunk.score = result.score;
                chunk.content = result.content;
                chunk.stale = result.stale;
                Some(chunk)
            })
            .collect())
    }

    async fn store(
        &self,
        entry: MemoryEntry,
        cancel: CancellationToken,
    ) -> Result<MemoryId, CoreMemoryError> {
        // Route global namespace entries to the global store.
        if matches!(entry.namespace, MemoryNamespace::Global { .. }) {
            return match self.global_store {
                Some(ref store) => store.store(&entry, cancel).await,
                None => Err(CoreMemoryError::RetrievalFailed(
                    "global memory store is not configured".into(),
                )),
            };
        }

        validate_scope(&self.project_id, &entry.project_id, &entry.namespace)?;
        let vector = if let Some(ref embedder) = self.embedder {
            embedder.embed(&entry.content).await.map_err(|e| CoreMemoryError::EmbeddingFailed {
                reason: format!("embedding generation failed: {e}"),
            })?
        } else {
            Vec::new()
        };

        // L1 dedup (ADR-46 symbolic offload): an optional LLM judge decides
        // store / update / merge / skip against the already-stored chunks.
        // Fail-open: any judge or recall problem stores the entry unchanged.
        let decision = self.decide_dedup(&entry, &vector, cancel.clone()).await;

        let DedupDecision::Store { content, vector, tombstone_after, links } = decision else {
            // Skip: the judge ruled the entry is already represented.
            return Ok(entry.id);
        };

        let chunk_hash = blake3::hash(content.as_bytes()).to_string();

        let record = EmbeddingRecord {
            id: entry.id.0.to_string(),
            project_id: self.project_id.clone(),
            chunk_hash,
            content,
            file_path: Utf8PathBuf::from("memory"),
            start_line: None,
            end_line: None,
            chunk_type: entry.chunk_type,
            vector,
            model_id: entry.model_id.clone().unwrap_or_default(),
            model_version: entry.model_version.clone().unwrap_or_default(),
            stale: false,
            created_at: entry.created_at,
        };

        self.sync
            .store(&record, cancel.clone())
            .await
            .map_err(|e| CoreMemoryError::Persistence(format!("failed to store memory: {e}")))?;

        // ADR-69 slice 1: persist the verdict's symbolic links BEFORE the
        // tombstone, so a link failure can never orphan a superseded chunk.
        // Fail-open by contract: an error (or a missing store) logs and the
        // memory itself is kept — a link store must never fail a memory
        // write.
        if !links.is_empty() {
            match self.link_store {
                Some(ref store) => match store.put_many(&links, cancel.clone()).await {
                    Ok(()) => {}
                    Err(e) => tracing::warn!(
                        error = %e,
                        links = links.len(),
                        "failed to persist ADR-69 memory links; continuing"
                    ),
                },
                None => tracing::debug!(
                    links = links.len(),
                    "no ADR-69 link store configured; skipping link writes"
                ),
            }
        }

        // Supersede the replaced/merged chunk ONLY after the write succeeded,
        // so a tombstone failure is logged-and-kept rather than data loss.
        let derive_cancel = cancel.clone();
        if let Some(target_id) = tombstone_after {
            self.sync.tombstone(&target_id, &self.project_id, cancel).await.map_err(|e| {
                CoreMemoryError::Persistence(format!("failed to supersede memory {target_id}: {e}"))
            })?;
        }

        // L1 typed extraction (live store path): a conversation-derived
        // summary additionally yields typed long-horizon memories, each
        // written through this same `store` path — so the dedup judge, links,
        // embedding and FTS all apply to derived memories. Fail-soft: any
        // problem here logs and never changes the parent's `Ok(entry.id)`.
        self.extract_l1_memories(&entry, derive_cancel).await;

        Ok(entry.id)
    }

    async fn invalidate(
        &self,
        id: MemoryId,
        cancel: CancellationToken,
    ) -> Result<(), CoreMemoryError> {
        // Try global store first if configured (ignore NotFound so project
        // entries can still be invalidated below).
        if let Some(ref store) = self.global_store {
            match store.invalidate(id, cancel.clone()).await {
                Ok(()) => return Ok(()),
                Err(CoreMemoryError::NotFound(_)) => { /* not a global entry */ }
                Err(e) => return Err(e),
            }
        }

        self.sync
            .tombstone(&id.0.to_string(), &self.project_id, cancel)
            .await
            .map_err(|e| CoreMemoryError::Persistence(format!("failed to invalidate memory: {e}")))
    }

    async fn invalidate_chunk(
        &self,
        id: &str,
        cancel: CancellationToken,
    ) -> Result<(), CoreMemoryError> {
        // Try global store first if configured (ignore NotFound so project
        // entries can still be invalidated below).
        if let Some(ref store) = self.global_store {
            match store.invalidate_chunk(id, cancel.clone()).await {
                Ok(()) => return Ok(()),
                Err(CoreMemoryError::NotFound(_)) => { /* not a global entry */ }
                Err(e) => return Err(e),
            }
        }

        self.sync
            .tombstone(id, &self.project_id, cancel)
            .await
            .map_err(|e| CoreMemoryError::Persistence(format!("failed to invalidate memory: {e}")))
    }

    async fn inspect_project_chunk(
        &self,
        project_id: &ProjectId,
        id: &str,
        cancel: CancellationToken,
    ) -> Result<Option<MemoryChunk>, CoreMemoryError> {
        if project_id != &self.project_id {
            return Err(CoreMemoryError::CrossProjectLeakage);
        }
        if cancel.is_cancelled() {
            return Err(CoreMemoryError::RetrievalFailed("memory operation cancelled".into()));
        }
        self.retriever.load_chunks(project_id, &[id.to_string()], cancel).await
            .map(|chunks| chunks.into_iter().next())
    }

    async fn invalidate_project_chunk(
        &self,
        project_id: &ProjectId,
        id: &str,
        cancel: CancellationToken,
    ) -> Result<(), CoreMemoryError> {
        if self.inspect_project_chunk(project_id, id, cancel.clone()).await?.is_none() {
            return Err(CoreMemoryError::NotFound(id.to_string()));
        }
        self.sync.tombstone(id, project_id, cancel).await
            .map_err(|e| CoreMemoryError::Persistence(format!("failed to invalidate memory: {e}")))
    }
}

fn validate_scope(
    store_project: &ProjectId,
    requested_project: &ProjectId,
    namespace: &MemoryNamespace,
) -> Result<(), CoreMemoryError> {
    if store_project != requested_project {
        return Err(CoreMemoryError::CrossProjectLeakage);
    }
    match namespace {
        MemoryNamespace::Project(namespace_project) if namespace_project == requested_project => {
            Ok(())
        }
        MemoryNamespace::Project(_) => Err(CoreMemoryError::CrossProjectLeakage),
        // Global namespace is allowed — it is handled by the global store
        // before validate_scope is reached in retrieve/store, but browse()
        // still uses validate_scope for project-scoped browsing.
        MemoryNamespace::Global { .. } => Ok(()),
        _ => Err(CoreMemoryError::RetrievalFailed("unknown namespace".into())),
    }
}

/// Build the `Fact` entry for one L1-extracted memory.
///
/// The derived entry inherits the parent slice's project and namespace, and
/// its `session_id` metadata when present, so it lands in the same recall
/// scope. Provenance and the typed attributes ride along as metadata
/// (`l1_kind`, `derived_from`, plus optional `scene` / `owner` / `deadline` /
/// `status`), which is how recall and the desktop browser can tell a typed
/// memory apart from a plain fact.
fn derived_l1_entry(parent: &MemoryEntry, memory: &L1ExtractedMemory) -> MemoryEntry {
    let mut metadata = serde_json::Map::new();
    metadata.insert("l1_kind".into(), memory.kind.as_str().into());
    metadata.insert("derived_from".into(), parent.id.0.to_string().into());
    for (key, value) in [
        ("scene", &memory.scene),
        ("owner", &memory.owner),
        ("deadline", &memory.deadline),
        ("status", &memory.status),
    ] {
        if let Some(value) = value {
            metadata.insert(key.to_owned(), value.as_str().into());
        }
    }
    if let Some(session_id) = parent.metadata.get("session_id") {
        metadata.insert("session_id".into(), session_id.clone());
    }
    MemoryEntry {
        id: MemoryId(ulid::Ulid::new()),
        project_id: parent.project_id.clone(),
        namespace: parent.namespace.clone(),
        content: memory.content.clone(),
        chunk_type: ChunkType::Fact,
        model_id: None,
        model_version: None,
        metadata: serde_json::Value::Object(metadata),
        expires_at: None,
        created_at: parent.created_at,
    }
}

fn compile_file_patterns(filters: &[MemoryFilter]) -> Result<Vec<Pattern>, CoreMemoryError> {
    filters
        .iter()
        .filter_map(|filter| match filter {
            MemoryFilter::FileGlob(pattern) => Some(pattern),
            _ => None,
        })
        .map(|pattern| {
            Pattern::new(pattern).map_err(|error| {
                CoreMemoryError::RetrievalFailed(format!(
                    "invalid memory file glob '{pattern}': {error}"
                ))
            })
        })
        .collect()
}

fn matches_filters(
    chunk: &MemoryChunk,
    filters: &[MemoryFilter],
    file_patterns: &[Pattern],
) -> bool {
    let options = MatchOptions {
        case_sensitive: !cfg!(windows),
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    let mut file_pattern_index = 0usize;
    for filter in filters {
        match filter {
            MemoryFilter::ChunkType(chunk_type) if chunk.chunk_type != *chunk_type => return false,
            MemoryFilter::MinScore(threshold) if chunk.score < *threshold => return false,
            MemoryFilter::FileGlob(_) => {
                let Some(pattern) = file_patterns.get(file_pattern_index) else {
                    return false;
                };
                file_pattern_index += 1;
                let Some(path) = &chunk.file_path else {
                    return false;
                };
                if !pattern.matches_path_with(path.as_std_path(), options) {
                    return false;
                }
            }
            MemoryFilter::ExcludeStale | MemoryFilter::ChunkType(_) | MemoryFilter::MinScore(_) => {
            }
            _ => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{
        FakeSummarizer, InMemoryFullTextStore, InMemoryVectorStore, MockEmbeddingGenerator,
    };
    use concerto_core::error::MemoryError;
    use concerto_core::memory::{ChunkType, MemoryLinkKind, MemoryNamespace, VectorResult};
    use std::sync::Mutex;
    use time::OffsetDateTime;

    struct TestVectorStore {
        last_query: Arc<Mutex<Option<Vec<f32>>>>,
    }

    impl TestVectorStore {
        fn new() -> Self {
            Self { last_query: Arc::new(Mutex::new(None)) }
        }
        fn get_last(&self) -> Option<Vec<f32>> {
            self.last_query.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl VectorStore for TestVectorStore {
        async fn store(
            &self,
            _records: &[EmbeddingRecord],
            _cancel: CancellationToken,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
        async fn search(
            &self,
            _project_id: &ProjectId,
            query: &[f32],
            _top_k: usize,
            _cancel: CancellationToken,
        ) -> Result<Vec<VectorResult>, MemoryError> {
            *self.last_query.lock().unwrap() = Some(query.to_vec());
            Ok(vec![])
        }
        async fn tombstone(
            &self,
            _chunk_id: &str,
            _project_id: &ProjectId,
            _cancel: CancellationToken,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
        async fn delete_tombstoned(
            &self,
            _project_id: &ProjectId,
            _cancel: CancellationToken,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
        async fn mark_stale(
            &self,
            _project_id: &ProjectId,
            _model_version: &str,
            _cancel: CancellationToken,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
        async fn delete_by_project(
            &self,
            _project_id: &ProjectId,
            _cancel: CancellationToken,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
        async fn delete_by_file_path(
            &self,
            _project_id: &ProjectId,
            _file_path: &Utf8PathBuf,
            _cancel: CancellationToken,
        ) -> Result<Vec<String>, MemoryError> {
            Ok(vec![])
        }
    }

    fn make_query(text: &str) -> MemoryQuery {
        MemoryQuery {
            text: text.to_string(),
            top_k: 5,
            project_id: ProjectId("test".into()),
            namespace: MemoryNamespace::Project(ProjectId("test".into())),
            filters: vec![],
        }
    }

    struct FailingEmbeddingGenerator;

    #[async_trait]
    impl EmbeddingGenerator for FailingEmbeddingGenerator {
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, MemoryError> {
            Err(MemoryError::EmbeddingFailed { reason: "offline".into() })
        }

        fn model_id(&self) -> &str {
            "failing"
        }

        fn model_version(&self) -> &str {
            "1"
        }

        fn dims(&self) -> usize {
            8
        }
    }

    #[tokio::test]
    async fn retrieve_uses_embedder() {
        let vector_store = Arc::new(TestVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        let decision_store = Arc::new(DecisionStore::new());
        let task_tree = Arc::new(TaskTreeStore::new());
        let embedder: Option<Arc<dyn EmbeddingGenerator>> =
            Some(Arc::new(MockEmbeddingGenerator::new(384)));
        let system = MemorySystem::new(
            vector_store.clone(),
            fts_store,
            decision_store,
            task_tree,
            embedder,
            ProjectId("test".into()),
            None,
        );
        let query = make_query("hello world");
        let _ = system.retrieve(&query, CancellationToken::new()).await.unwrap();
        let recorded = vector_store.get_last().expect("search called");
        assert!(!recorded.is_empty(), "embedding should be non-empty");
    }

    #[tokio::test]
    async fn retrieve_without_embedder_uses_empty_vector() {
        let vector_store = Arc::new(TestVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        let decision_store = Arc::new(DecisionStore::new());
        let task_tree = Arc::new(TaskTreeStore::new());
        let system = MemorySystem::new(
            vector_store.clone(),
            fts_store,
            decision_store,
            task_tree,
            None,
            ProjectId("test".into()),
            None,
        );
        let query = make_query("hello world");
        let _ = system.retrieve(&query, CancellationToken::new()).await.unwrap();
        let recorded = vector_store.get_last().expect("search called");
        assert!(recorded.is_empty(), "embedding should be empty when no embedder");
    }

    #[tokio::test]
    async fn embedding_failure_falls_back_to_attributed_fts_results() {
        // Use a project id distinct from the other `test`-project tests: the
        // query-path record_failure (S1) marks this project's embedder broken,
        // which must not leak into parallel "test"-project tests.
        let project_id = ProjectId("attributed-fts".into());
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        let sync = ChunkSyncService::new(vector_store.clone(), fts_store.clone());
        let record = EmbeddingRecord {
            id: "chunk".into(),
            project_id: project_id.clone(),
            chunk_hash: "hash".into(),
            content: "offline fallback phrase".into(),
            file_path: "src/lib.rs".into(),
            start_line: Some(7),
            end_line: Some(9),
            chunk_type: ChunkType::Function,
            vector: vec![0.0; 8],
            model_id: "test".into(),
            model_version: "1".into(),
            stale: false,
            created_at: OffsetDateTime::now_utc(),
        };
        sync.store(&record, CancellationToken::new()).await.unwrap();
        let system = MemorySystem::new(
            vector_store,
            fts_store,
            Arc::new(DecisionStore::new()),
            Arc::new(TaskTreeStore::new()),
            Some(Arc::new(FailingEmbeddingGenerator)),
            project_id.clone(),
            None,
        );
        let mut query = make_query("fallback");
        query.project_id = project_id.clone();
        query.namespace = MemoryNamespace::Project(project_id.clone());
        query.filters = vec![
            MemoryFilter::ChunkType(ChunkType::Function),
            MemoryFilter::FileGlob("src/*.rs".into()),
        ];

        let results = system.retrieve(&query, CancellationToken::new()).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].project_id, project_id);
        assert_eq!(results[0].file_path.as_deref(), Some(camino::Utf8Path::new("src/lib.rs")));
        assert_eq!(results[0].start_line, Some(7));
        assert_eq!(results[0].end_line, Some(9));
    }

    #[tokio::test]
    async fn rejects_cross_project_queries() {
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        let system = MemorySystem::new(
            vector_store,
            fts_store,
            Arc::new(DecisionStore::new()),
            Arc::new(TaskTreeStore::new()),
            None,
            ProjectId("test".into()),
            None,
        );
        let mut cross_project = make_query("anything");
        cross_project.project_id = ProjectId("other".into());
        cross_project.namespace = MemoryNamespace::Project(cross_project.project_id.clone());
        assert!(matches!(
            system.retrieve(&cross_project, CancellationToken::new()).await,
            Err(MemoryError::CrossProjectLeakage)
        ));
    }

    #[tokio::test]
    async fn global_namespace_returns_empty_when_no_store_configured() {
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        let system = MemorySystem::new(
            vector_store,
            fts_store,
            Arc::new(DecisionStore::new()),
            Arc::new(TaskTreeStore::new()),
            None,
            ProjectId("test".into()),
            None,
        );
        let mut global = make_query("anything");
        global.namespace = MemoryNamespace::Global { user_id_hash: "user".into() };
        // Without a global store, global queries return empty (not an error)
        let results = system.retrieve(&global, CancellationToken::new()).await.unwrap();
        assert!(results.is_empty());
    }

    /// ADR-54 re-enable: with a global store configured, a
    /// `MemoryNamespace::Global` entry round-trips through `MemorySystem` and
    /// is scoped to the querying user's `user_id_hash`.
    #[tokio::test]
    async fn global_store_round_trips_through_memory_system() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let global = Arc::new(GlobalMemoryStore::new(pool).await.expect("global store opens"));
        let system = MemorySystem::new(
            Arc::new(InMemoryVectorStore::new()),
            Arc::new(InMemoryFullTextStore::new()),
            Arc::new(DecisionStore::new()),
            Arc::new(TaskTreeStore::new()),
            None,
            ProjectId("test".into()),
            Some(global),
        );

        let user_id_hash = "user-global";
        let entry = MemoryEntry {
            id: MemoryId(ulid::Ulid::new()),
            project_id: ProjectId(user_id_hash.to_string()),
            namespace: MemoryNamespace::Global { user_id_hash: user_id_hash.to_string() },
            content: "remember the global preference".into(),
            chunk_type: ChunkType::Fact,
            model_id: None,
            model_version: None,
            metadata: serde_json::json!({}),
            expires_at: None,
            created_at: OffsetDateTime::now_utc(),
        };
        let expected_id = entry.id;
        let stored = system.store(entry, CancellationToken::new()).await.expect("global store");
        assert_eq!(stored, expected_id);

        let mut query = make_query("global preference");
        query.namespace = MemoryNamespace::Global { user_id_hash: user_id_hash.to_string() };
        let results =
            system.retrieve(&query, CancellationToken::new()).await.expect("global query");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "remember the global preference");

        // A different user never sees the entry (namespace isolation).
        let mut other = make_query("global preference");
        other.namespace = MemoryNamespace::Global { user_id_hash: "someone-else".into() };
        let other_results =
            system.retrieve(&other, CancellationToken::new()).await.expect("other user query");
        assert!(other_results.is_empty(), "global store must isolate by user_id_hash");
    }

    // ---------------------------------------------------------------------
    // L1 dedup judge (with_dedup_judge)
    // ---------------------------------------------------------------------

    /// Seed one fact chunk into freshly created in-memory stores, routing the
    /// write through `ChunkSyncService` so both sides agree on the record.
    async fn dedup_seed(
        project_id: &ProjectId,
        id: &str,
        content: &str,
        vector_store: Arc<InMemoryVectorStore>,
        fts_store: Arc<InMemoryFullTextStore>,
    ) {
        let sync = ChunkSyncService::new(vector_store, fts_store);
        let record = EmbeddingRecord {
            id: id.into(),
            project_id: project_id.clone(),
            chunk_hash: "hash".into(),
            content: content.into(),
            file_path: "memory".into(),
            start_line: None,
            end_line: None,
            chunk_type: ChunkType::Fact,
            vector: vec![0.0; 8],
            model_id: "test".into(),
            model_version: "1".into(),
            stale: false,
            created_at: OffsetDateTime::now_utc(),
        };
        sync.store(&record, CancellationToken::new()).await.unwrap();
    }

    fn dedup_system(
        vector_store: Arc<InMemoryVectorStore>,
        fts_store: Arc<InMemoryFullTextStore>,
        judge: Option<L1DedupJudge>,
    ) -> MemorySystem {
        let system = MemorySystem::new(
            vector_store,
            fts_store,
            Arc::new(DecisionStore::new()),
            Arc::new(TaskTreeStore::new()),
            Some(Arc::new(MockEmbeddingGenerator::new(384))),
            ProjectId("test".into()),
            None,
        );
        match judge {
            Some(judge) => system.with_dedup_judge(judge),
            None => system,
        }
    }

    fn dedup_entry(content: &str) -> MemoryEntry {
        MemoryEntry {
            id: MemoryId(ulid::Ulid::new()),
            project_id: ProjectId("test".into()),
            namespace: MemoryNamespace::Project(ProjectId("test".into())),
            content: content.into(),
            chunk_type: ChunkType::Fact,
            model_id: None,
            model_version: None,
            metadata: serde_json::json!({}),
            expires_at: None,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    #[tokio::test]
    async fn project_inspection_and_deletion_preserve_global_and_foreign_entries() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let global = Arc::new(GlobalMemoryStore::new(pool.clone()).await.unwrap());
        let vectors = Arc::new(InMemoryVectorStore::new());
        let fts = Arc::new(InMemoryFullTextStore::new());
        let project = ProjectId("test".into());
        let other = ProjectId("other".into());
        let id = ulid::Ulid::new();
        let text = "complete project content\n".repeat(50);
        dedup_seed(&project, &id.to_string(), &text, vectors.clone(), fts.clone()).await;
        dedup_seed(&other, "foreign", "foreign content", vectors.clone(), fts.clone()).await;
        let system = MemorySystem::new(vectors.clone(), fts, Arc::new(DecisionStore::new()), Arc::new(TaskTreeStore::new()), None, project.clone(), Some(global));
        let mut entry = dedup_entry("global preference");
        entry.id = MemoryId(id);
        entry.project_id = ProjectId("user".into());
        entry.namespace = MemoryNamespace::Global { user_id_hash: "user".into() };
        system.store(entry, CancellationToken::new()).await.unwrap();
        let chunk = system.inspect_project_chunk(&project, &id.to_string(), CancellationToken::new()).await.unwrap().unwrap();
        assert_eq!(chunk.content, text);
        assert!(system.inspect_project_chunk(&project, "foreign", CancellationToken::new()).await.unwrap().is_none());
        assert!(matches!(system.invalidate_project_chunk(&other, "foreign", CancellationToken::new()).await, Err(MemoryError::CrossProjectLeakage)));
        assert!(matches!(system.invalidate_project_chunk(&project, "missing", CancellationToken::new()).await, Err(MemoryError::NotFound(_))));
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(system.invalidate_project_chunk(&project, &id.to_string(), cancel).await.is_err());
        assert!(system.inspect_project_chunk(&project, &id.to_string(), CancellationToken::new()).await.unwrap().is_some());
        system.invalidate_project_chunk(&project, &id.to_string(), CancellationToken::new()).await.unwrap();
        assert!(system.inspect_project_chunk(&project, &id.to_string(), CancellationToken::new()).await.unwrap().is_none());
        assert_eq!(stored_records(&vectors, &other).await.len(), 1);
        let mut query = make_query("global preference");
        query.namespace = MemoryNamespace::Global { user_id_hash: "user".into() };
        assert_eq!(system.retrieve(&query, CancellationToken::new()).await.unwrap().len(), 1);
        pool.close().await;
    }

    async fn stored_records(
        vector_store: &InMemoryVectorStore,
        project_id: &ProjectId,
    ) -> Vec<VectorResult> {
        vector_store.search(project_id, &[0.1; 8], 100, CancellationToken::new()).await.unwrap()
    }

    /// (store, skip, update, merge, failures) counter snapshot.
    fn counter_snapshot(system: &MemorySystem) -> (usize, usize, usize, usize, usize) {
        let counters = system.dedup_counters();
        (
            counters.store.load(Ordering::Relaxed),
            counters.skip.load(Ordering::Relaxed),
            counters.update.load(Ordering::Relaxed),
            counters.merge.load(Ordering::Relaxed),
            counters.failures.load(Ordering::Relaxed),
        )
    }

    #[tokio::test]
    async fn store_without_dedup_judge_writes_plainly() {
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        let system = dedup_system(vector_store.clone(), fts_store, None);
        let entry = dedup_entry("the user prefers rust");
        let stored = system.store(entry.clone(), CancellationToken::new()).await.unwrap();
        assert_eq!(stored, entry.id);

        let records = stored_records(&vector_store, &ProjectId("test".into())).await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].content, "the user prefers rust");
        // Without a judge nothing is ever counted against the dedup pass.
        assert_eq!(counter_snapshot(&system), (0, 0, 0, 0, 0));
    }

    #[tokio::test]
    async fn store_skips_near_duplicate_without_writing() {
        let project_id = ProjectId("test".into());
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        dedup_seed(
            &project_id,
            "existing-1",
            "the user's favorite food is tacos",
            vector_store.clone(),
            fts_store.clone(),
        )
        .await;

        let judge = L1DedupJudge::new(Arc::new(FakeSummarizer::new(
            r#"{"action": "skip", "target_id": null}"#,
        )));
        let system = dedup_system(vector_store.clone(), fts_store, Some(judge));
        let entry = dedup_entry("the user's favorite food is tacos");
        let stored = system.store(entry.clone(), CancellationToken::new()).await.unwrap();
        assert_eq!(stored, entry.id);

        let records = stored_records(&vector_store, &project_id).await;
        assert_eq!(records.len(), 1, "skip must not write a second chunk");
        assert_eq!(records[0].chunk_id, "existing-1");
        assert_eq!(counter_snapshot(&system), (0, 1, 0, 0, 0), "skip verdict is counted");
    }

    #[tokio::test]
    async fn store_merges_and_tombstones_target() {
        let project_id = ProjectId("test".into());
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        dedup_seed(
            &project_id,
            "existing-1",
            "the user's preferred color is blue",
            vector_store.clone(),
            fts_store.clone(),
        )
        .await;

        let judge = L1DedupJudge::new(Arc::new(FakeSummarizer::new(
            r#"{"action": "merge", "target_id": "existing-1"}"#,
        )));
        let system = dedup_system(vector_store.clone(), fts_store, Some(judge));
        system
            .store(dedup_entry("the user's preferred color is teal"), CancellationToken::new())
            .await
            .unwrap();

        let records = stored_records(&vector_store, &project_id).await;
        assert_eq!(records.len(), 1, "merge must supersede the target chunk");
        assert!(records[0].content.contains("teal"));
        assert!(
            records[0].content.contains("blue"),
            "merged content must include the target's content: {}",
            records[0].content
        );
        assert_eq!(counter_snapshot(&system), (0, 0, 0, 1, 0), "merge verdict is counted");
    }

    #[tokio::test]
    async fn store_update_tombstones_target() {
        let project_id = ProjectId("test".into());
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        dedup_seed(
            &project_id,
            "existing-1",
            "the user's favorite city is london",
            vector_store.clone(),
            fts_store.clone(),
        )
        .await;

        let judge = L1DedupJudge::new(Arc::new(FakeSummarizer::new(
            r#"{"action": "update", "target_id": "existing-1"}"#,
        )));
        let system = dedup_system(vector_store.clone(), fts_store, Some(judge));
        system
            .store(dedup_entry("the user's favorite city is paris"), CancellationToken::new())
            .await
            .unwrap();

        let records = stored_records(&vector_store, &project_id).await;
        assert_eq!(records.len(), 1, "update must supersede the target chunk");
        assert_eq!(records[0].content, "the user's favorite city is paris");
        assert!(!records[0].content.contains("london"));
        assert_eq!(counter_snapshot(&system), (0, 0, 1, 0, 0), "update verdict is counted");
    }

    #[tokio::test]
    async fn store_judge_rules_store_and_is_counted() {
        let project_id = ProjectId("test".into());
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        dedup_seed(
            &project_id,
            "existing-1",
            "the user's favorite food is tacos",
            vector_store.clone(),
            fts_store.clone(),
        )
        .await;

        let judge = L1DedupJudge::new(Arc::new(FakeSummarizer::new(
            r#"{"action": "store", "target_id": null}"#,
        )));
        let system = dedup_system(vector_store.clone(), fts_store, Some(judge));
        system
            .store(dedup_entry("the user likes to code in rust"), CancellationToken::new())
            .await
            .unwrap();

        let records = stored_records(&vector_store, &project_id).await;
        assert_eq!(records.len(), 2, "store verdict keeps the new chunk");
        assert_eq!(counter_snapshot(&system), (1, 0, 0, 0, 0), "store verdict is counted");
    }

    #[tokio::test]
    async fn store_falls_back_to_plain_insert_on_judge_error() {
        let project_id = ProjectId("test".into());
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        dedup_seed(
            &project_id,
            "existing-1",
            "a pre-existing fact",
            vector_store.clone(),
            fts_store.clone(),
        )
        .await;

        let judge = L1DedupJudge::new(Arc::new(FakeSummarizer::new_err("judge down")));
        let system = dedup_system(vector_store.clone(), fts_store, Some(judge));
        system.store(dedup_entry("an entirely new fact"), CancellationToken::new()).await.unwrap();

        // Fail-open: the judge's error must not drop the memory.
        let records = stored_records(&vector_store, &project_id).await;
        assert_eq!(records.len(), 2);
        assert!(records.iter().any(|r| r.content == "an entirely new fact"));
        assert_eq!(counter_snapshot(&system), (0, 0, 0, 0, 1), "judge failure is counted");
    }

    // ---------------------------------------------------------------------
    // ADR-69 symbolic links (with_link_store)
    // ---------------------------------------------------------------------

    async fn in_memory_link_store() -> Arc<LinkStore> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        Arc::new(LinkStore::new(pool).await.unwrap())
    }

    fn dedup_system_with_links(
        vector_store: Arc<InMemoryVectorStore>,
        fts_store: Arc<InMemoryFullTextStore>,
        judge: Option<L1DedupJudge>,
        link_store: Arc<LinkStore>,
    ) -> MemorySystem {
        dedup_system(vector_store, fts_store, judge).with_link_store(link_store)
    }

    fn dedup_entry_with_metadata(content: &str, metadata: serde_json::Value) -> MemoryEntry {
        let mut entry = dedup_entry(content);
        entry.metadata = metadata;
        entry
    }

    #[tokio::test]
    async fn store_update_writes_supersedes_link() {
        let project_id = ProjectId("test".into());
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        dedup_seed(
            &project_id,
            "existing-1",
            "the user's favorite city is london",
            vector_store.clone(),
            fts_store.clone(),
        )
        .await;

        let judge = L1DedupJudge::new(Arc::new(FakeSummarizer::new(
            r#"{"action": "update", "target_id": "existing-1"}"#,
        )));
        let link_store = in_memory_link_store().await;
        let system = dedup_system_with_links(
            vector_store.clone(),
            fts_store,
            Some(judge),
            link_store.clone(),
        );
        let entry = dedup_entry("the user's favorite city is paris");
        system.store(entry.clone(), CancellationToken::new()).await.unwrap();

        let from = entry.id.0.to_string();
        let links = link_store.links_from(&from, CancellationToken::new()).await.unwrap();
        assert_eq!(links.len(), 1, "update verdict must write one link");
        assert_eq!(links[0].kind, MemoryLinkKind::Supersedes);
        assert_eq!(links[0].to, "existing-1");
        assert_eq!(links[0].weight, 1.0);

        // The target sees the inverse direction (slice-2 in-degree read).
        let mut into_target =
            link_store.links_to("existing-1", CancellationToken::new()).await.unwrap();
        assert_eq!(into_target.len(), 1);
        assert_eq!(into_target.remove(0).from, from);
    }

    #[tokio::test]
    async fn store_merge_writes_merges_link() {
        let project_id = ProjectId("test".into());
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        dedup_seed(
            &project_id,
            "existing-1",
            "the user's preferred color is blue",
            vector_store.clone(),
            fts_store.clone(),
        )
        .await;

        let judge = L1DedupJudge::new(Arc::new(FakeSummarizer::new(
            r#"{"action": "merge", "target_id": "existing-1"}"#,
        )));
        let link_store = in_memory_link_store().await;
        let system = dedup_system_with_links(
            vector_store.clone(),
            fts_store,
            Some(judge),
            link_store.clone(),
        );
        let entry = dedup_entry("the user's preferred color is teal");
        system.store(entry.clone(), CancellationToken::new()).await.unwrap();

        let from = entry.id.0.to_string();
        let links = link_store.links_from(&from, CancellationToken::new()).await.unwrap();
        assert_eq!(links.len(), 1, "merge verdict must write one link");
        assert_eq!(links[0].kind, MemoryLinkKind::Merges);
        assert_eq!(links[0].to, "existing-1");
    }

    #[tokio::test]
    async fn store_update_writes_references_from_metadata() {
        let project_id = ProjectId("test".into());
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        dedup_seed(
            &project_id,
            "existing-1",
            "a pre-existing fact",
            vector_store.clone(),
            fts_store.clone(),
        )
        .await;

        let judge = L1DedupJudge::new(Arc::new(FakeSummarizer::new(
            r#"{"action": "update", "target_id": "existing-1"}"#,
        )));
        let link_store = in_memory_link_store().await;
        let system = dedup_system_with_links(
            vector_store.clone(),
            fts_store,
            Some(judge),
            link_store.clone(),
        );
        let entry = dedup_entry_with_metadata(
            "an updated fact with context",
            serde_json::json!({
                "refs": ["chunk-a", "chunk-b"],
                "result_ref": "chunk-c",
            }),
        );
        system.store(entry.clone(), CancellationToken::new()).await.unwrap();

        let from = entry.id.0.to_string();
        let links = link_store.links_from(&from, CancellationToken::new()).await.unwrap();
        assert_eq!(links.len(), 4, "1 supersedes + 3 references expected");
        assert_eq!(links[0].kind, MemoryLinkKind::Supersedes);
        assert_eq!(links[0].to, "existing-1");
        let reference_targets: Vec<&str> = links[1..]
            .iter()
            .map(|link| {
                assert_eq!(link.kind, MemoryLinkKind::References);
                link.to.as_str()
            })
            .collect();
        assert_eq!(reference_targets, vec!["chunk-a", "chunk-b", "chunk-c"]);
    }

    #[tokio::test]
    async fn store_plain_verdict_writes_no_links() {
        let project_id = ProjectId("test".into());
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        dedup_seed(
            &project_id,
            "existing-1",
            "a pre-existing fact",
            vector_store.clone(),
            fts_store.clone(),
        )
        .await;

        let judge = L1DedupJudge::new(Arc::new(FakeSummarizer::new(
            r#"{"action": "store", "target_id": null}"#,
        )));
        let link_store = in_memory_link_store().await;
        let system = dedup_system_with_links(
            vector_store.clone(),
            fts_store,
            Some(judge),
            link_store.clone(),
        );
        let entry = dedup_entry_with_metadata(
            "a plain fact that happens to list refs",
            serde_json::json!({ "refs": ["chunk-a"] }),
        );
        system.store(entry.clone(), CancellationToken::new()).await.unwrap();

        // Plain stores are link-free even when refs metadata is present —
        // links exist to record supersession/dependence, not to duplicate
        // arbitrary metadata.
        let from = entry.id.0.to_string();
        let links = link_store.links_from(&from, CancellationToken::new()).await.unwrap();
        assert!(links.is_empty(), "plain store verdict must write no links");
        assert_eq!(link_store.out_degree(&from, CancellationToken::new()).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn store_fails_open_when_link_store_errors() {
        let project_id = ProjectId("test".into());
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        dedup_seed(
            &project_id,
            "existing-1",
            "a pre-existing fact",
            vector_store.clone(),
            fts_store.clone(),
        )
        .await;

        let judge = L1DedupJudge::new(Arc::new(FakeSummarizer::new(
            r#"{"action": "update", "target_id": "existing-1"}"#,
        )));
        // A link store whose pool is already closed: every link write fails.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("links.db");
        let options =
            sqlx::sqlite::SqliteConnectOptions::new().filename(&path).create_if_missing(true);
        let pool = sqlx::SqlitePool::connect_with(options).await.unwrap();
        let link_store = Arc::new(LinkStore::new(pool).await.unwrap());
        link_store.close_pool().await;

        let system = dedup_system_with_links(
            vector_store.clone(),
            fts_store,
            Some(judge),
            link_store.clone(),
        );
        let entry = dedup_entry("the user's favorite city is paris");
        system.store(entry.clone(), CancellationToken::new()).await.unwrap();

        // Fail-open by contract: the link-store error must not fail the
        // memory write nor skip the tombstone.
        let records = stored_records(&vector_store, &project_id).await;
        assert_eq!(records.len(), 1, "the chunk must still be written");
        assert_eq!(records[0].content, "the user's favorite city is paris");
        assert_eq!(counter_snapshot(&system), (0, 0, 1, 0, 0), "update is still counted");
    }

    // ---------------------------------------------------------------------
    // ADR-69 slice 2 — link cascade routing (with_link_cascade)
    // ---------------------------------------------------------------------

    /// Two seeded chunks (`a_plain`, `b_linked`) in in-memory stores; the
    /// vector store orders by chunk_id so the plain fused order is
    /// `[a_plain, b_linked]`. Evidence INTO `b_linked` must promote it ahead.
    async fn cascade_routing_system(enable_cascade: bool) -> (MemorySystem, Arc<LinkStore>) {
        use concerto_core::memory::MemoryLink;

        let project_id = ProjectId("test".into());
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        dedup_seed(&project_id, "a_plain", "plain memory", vector_store.clone(), fts_store.clone())
            .await;
        dedup_seed(
            &project_id,
            "b_linked",
            "linked memory",
            vector_store.clone(),
            fts_store.clone(),
        )
        .await;

        let link_store = in_memory_link_store().await;
        // Two positive-support links INTO `b_linked`. ADR-69 re-rank is
        // deliberately conservative: with γ = 0.2 over a one-rank gap, a
        // chunk needs score > 5.0 to promote (key_b < key_a ⟺
        // 1 + 0.2·(10−score_b) < 2.0 ⟺ score_b > 5.0). Two weighted
        // Supports links reach score 6.25; a single one (4.375) would not.
        for source in ["evidence-a", "evidence-b"] {
            link_store
                .put(
                    &MemoryLink {
                        from: source.into(),
                        to: "b_linked".into(),
                        kind: MemoryLinkKind::Supports,
                        weight: 1.0,
                    },
                    CancellationToken::new(),
                )
                .await
                .unwrap();
        }

        let system =
            dedup_system(vector_store, fts_store, None).with_link_store(link_store.clone());
        let system = if enable_cascade {
            system.with_link_cascade(LinkCascadeConfig {
                decay_days: Some(90),
                start_tier: crate::rag::CascadeTier::Emergency,
                score_timeout: std::time::Duration::from_millis(250),
            })
        } else {
            system
        };
        (system, link_store)
    }

    #[tokio::test]
    async fn retrieve_reorders_linked_chunk_when_cascade_configured() {
        let (system, _link_store) = cascade_routing_system(true).await;
        let mut query = make_query("anything");
        query.top_k = 2;

        let results = system.retrieve(&query, CancellationToken::new()).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].id, "b_linked",
            "link evidence must promote the linked chunk above the plain one"
        );
        assert_eq!(results[1].id, "a_plain");
    }

    #[tokio::test]
    async fn retrieve_keeps_rrf_order_with_links_but_no_cascade() {
        let (system, link_store) = cascade_routing_system(false).await;
        // Sanity: the evidence really exists — only the routing was off.
        assert_eq!(
            link_store.links_to("b_linked", CancellationToken::new()).await.unwrap().len(),
            2
        );
        let mut query = make_query("anything");
        query.top_k = 2;

        let results = system.retrieve(&query, CancellationToken::new()).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].id, "a_plain",
            "without the cascade, links must not change RRF order"
        );
        assert_eq!(results[1].id, "b_linked");
    }

    // ------------------------------------------------------------------
    // L1 typed extraction — the live store-path seam (TODO.md
    // "L1 typed extraction": extractor + typed memories land on `store`).
    // ------------------------------------------------------------------

    /// A conversation-derived `SessionSummary` slice, as written at task
    /// completion (see `AgentLoop::store_task_summary`).
    fn summary_entry(content: &str) -> MemoryEntry {
        MemoryEntry {
            id: MemoryId(ulid::Ulid::new()),
            project_id: ProjectId("test".into()),
            namespace: MemoryNamespace::Project(ProjectId("test".into())),
            content: content.into(),
            chunk_type: ChunkType::SessionSummary,
            model_id: None,
            model_version: None,
            metadata: serde_json::json!({ "session_id": "session-1" }),
            expires_at: None,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    #[tokio::test]
    async fn store_of_session_summary_extracts_typed_memories() {
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        let extractor = L1Extractor::new(Arc::new(FakeSummarizer::new(
            r#"{"memories":[
                {"type":"episodic","scene":"setup","content":"user prefers rust edition 2024"},
                {"type":"work_fact","owner":"me","content":"concerto workspace uses sqlx"}
            ]}"#,
        )));
        let system =
            dedup_system(vector_store.clone(), fts_store, None).with_l1_extractor(extractor);

        let parent = summary_entry("Task: wire the L1 extractor\nResult: done");
        let stored = system.store(parent.clone(), CancellationToken::new()).await.unwrap();
        assert_eq!(stored, parent.id);

        let records = stored_records(&vector_store, &ProjectId("test".into())).await;
        // The parent summary plus exactly two derived memories: a derived
        // `Fact` never re-enters the pass, so extraction cannot recurse.
        assert_eq!(records.len(), 3);
        assert_eq!(system.l1_counters().extracted.load(Ordering::Relaxed), 2);
        assert_eq!(system.l1_counters().failures.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn l1_extraction_failure_keeps_the_parent_store() {
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        let extractor = L1Extractor::new(Arc::new(FakeSummarizer::new_err("extractor down")));
        let system =
            dedup_system(vector_store.clone(), fts_store, None).with_l1_extractor(extractor);

        let parent = summary_entry("Task: anything");
        system.store(parent.clone(), CancellationToken::new()).await.unwrap();

        let records = stored_records(&vector_store, &ProjectId("test".into())).await;
        assert_eq!(records.len(), 1, "the parent summary survives a failed extraction");
        assert_eq!(system.l1_counters().extracted.load(Ordering::Relaxed), 0);
        assert_eq!(system.l1_counters().failures.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn only_conversation_summaries_trigger_extraction() {
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        let extractor = L1Extractor::new(Arc::new(FakeSummarizer::new(
            r#"{"memories":[{"type":"persona","content":"user likes rust"}]}"#,
        )));
        let system =
            dedup_system(vector_store.clone(), fts_store, None).with_l1_extractor(extractor);

        system.store(dedup_entry("plain fact"), CancellationToken::new()).await.unwrap();

        let records = stored_records(&vector_store, &ProjectId("test".into())).await;
        assert_eq!(records.len(), 1, "a non-summary store must not extract");
        assert_eq!(system.l1_counters().extracted.load(Ordering::Relaxed), 0);
        assert_eq!(system.l1_counters().failures.load(Ordering::Relaxed), 0);
    }

    // ---------------------------------------------------------------------
    // H11 evidence-bearing recall (retrieve_with_evidence)
    // ---------------------------------------------------------------------

    fn evident_system(project: &str) -> MemorySystem {
        MemorySystem::new(
            Arc::new(InMemoryVectorStore::new()),
            Arc::new(InMemoryFullTextStore::new()),
            Arc::new(DecisionStore::new()),
            Arc::new(TaskTreeStore::new()),
            None,
            ProjectId(project.into()),
            None,
        )
    }

    fn evident_query(project: &str, text: &str) -> MemoryQuery {
        MemoryQuery {
            text: text.to_string(),
            top_k: 5,
            project_id: ProjectId(project.into()),
            namespace: MemoryNamespace::Project(ProjectId(project.into())),
            filters: vec![],
        }
    }

    fn fact_entry(project: &str, content: &str) -> MemoryEntry {
        MemoryEntry {
            id: MemoryId(ulid::Ulid::new()),
            project_id: ProjectId(project.into()),
            namespace: MemoryNamespace::Project(ProjectId(project.into())),
            content: content.into(),
            chunk_type: ChunkType::Fact,
            model_id: None,
            model_version: None,
            metadata: serde_json::json!({}),
            expires_at: None,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    /// H11 wiring: a model-authored `Fact` recalls as a claim — `Assumed`
    /// truth with `ModelAuthored` source — even at top rank, and the bare
    /// `retrieve` projection serves the same chunk (one recall, not two).
    #[tokio::test]
    async fn evidence_recall_marks_model_authored_fact_a_claim() {
        use concerto_core::memory::{FactStatusView, RecallSource};

        let project = "h11-claim";
        let system = evident_system(project);
        let entry = fact_entry(project, "the widget must stay blue claim marker");
        let stored = system.store(entry, CancellationToken::new()).await.expect("store fact");

        let query = evident_query(project, "blue claim marker");
        let evident =
            system.retrieve_with_evidence(&query, CancellationToken::new()).await.expect("recall");
        assert_eq!(evident.len(), 1, "the stored fact recalls");
        let first = &evident[0];
        assert_eq!(first.chunk.id, stored.to_string(), "identity survives the recall");
        assert_eq!(first.truth, FactStatusView::Assumed, "a claim stays a claim");
        assert_eq!(first.source, RecallSource::ModelAuthored);
        assert!(first.derivation.contains("claim"), "says why: {}", first.derivation);
        assert!(!first.degraded);

        // The bare projection serves the same chunk from the same recall.
        let bare = system.retrieve(&query, CancellationToken::new()).await.expect("bare recall");
        assert_eq!(bare.len(), 1);
        assert_eq!(bare[0].id, first.chunk.id);
        assert_eq!(bare[0].content, first.chunk.content);
    }

    /// H11 wiring: a live `Contradicts` link contradicts the recalled chunk
    /// and the derivation cites the contradicting source — without any
    /// cascade configured (standing reads links independently of the
    /// reorder).
    #[tokio::test]
    async fn evidence_recall_contradiction_cites_link() {
        use concerto_core::memory::{FactStatusView, MemoryLink};

        let project = "h11-contradicted";
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("link pool connects");
        let link_store = Arc::new(LinkStore::new(pool).await.expect("link store opens"));
        let system = evident_system(project).with_link_store(link_store.clone());

        let entry = fact_entry(project, "tabs versus spaces contradicted marker");
        let stored = system.store(entry, CancellationToken::new()).await.expect("store fact");
        link_store
            .put(
                &MemoryLink::new("critic", stored.to_string(), MemoryLinkKind::Contradicts),
                CancellationToken::new(),
            )
            .await
            .expect("contradiction link persists");

        let query = evident_query(project, "contradicted marker");
        let evident =
            system.retrieve_with_evidence(&query, CancellationToken::new()).await.expect("recall");
        assert_eq!(evident.len(), 1);
        assert_eq!(evident[0].truth, FactStatusView::Contradicted);
        assert!(
            evident[0].derivation.contains("critic"),
            "cites the source: {}",
            evident[0].derivation
        );
    }

    /// H11 wiring: a row past its TTL window recalls `Stale` with the
    /// window and age named — freshness never comes from rank.
    #[tokio::test]
    async fn evidence_recall_ttl_expiry_stales() {
        use concerto_core::memory::{ChunkType, EmbeddingRecord, FactStatusView};

        let project = ProjectId("h11-ttl".into());
        let record = EmbeddingRecord {
            id: "old-fn".into(),
            project_id: project.clone(),
            chunk_hash: "hash".into(),
            content: "fn old() {} ttl expiry marker".into(),
            file_path: "src/old.rs".into(),
            start_line: Some(1),
            end_line: Some(1),
            chunk_type: ChunkType::Function,
            vector: vec![0.1; 8],
            model_id: "m".into(),
            model_version: "1".into(),
            stale: false,
            created_at: OffsetDateTime::now_utc() - time::Duration::days(200),
        };
        let system = MemorySystem::new(
            Arc::new(InMemoryVectorStore::with_records(vec![(project.clone(), record)])),
            Arc::new(InMemoryFullTextStore::new()),
            Arc::new(DecisionStore::new()),
            Arc::new(TaskTreeStore::new()),
            Some(Arc::new(MockEmbeddingGenerator::new(8))),
            project.clone(),
            None,
        );

        let query = evident_query("h11-ttl", "ttl expiry marker");
        let evident =
            system.retrieve_with_evidence(&query, CancellationToken::new()).await.expect("recall");
        assert_eq!(evident.len(), 1);
        assert_eq!(evident[0].truth, FactStatusView::Stale);
        assert!(
            evident[0].derivation.contains("90-day"),
            "names the window: {}",
            evident[0].derivation
        );
    }

    /// H11 wiring: a cancelled token fails open — recall still succeeds,
    /// with standing-dependent verdicts reading Unknown rather than
    /// inventing status — but the interruption is explicit
    /// (`interrupted`), never silent.
    #[tokio::test]
    async fn evidence_recall_cancelled_token_reads_unknown() {
        use concerto_core::memory::FactStatusView;

        let project = "h11-cancelled";
        let system = evident_system(project);
        system
            .store(fact_entry(project, "cancelled recall marker content"), CancellationToken::new())
            .await
            .expect("store fact");

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let query = evident_query(project, "cancelled recall marker");
        let evident = system.retrieve_with_evidence(&query, cancelled).await.expect("fail-open");
        assert_eq!(evident.len(), 1, "the recall itself still serves");
        assert_eq!(evident[0].truth, FactStatusView::Unknown);
        assert!(evident[0].interrupted, "interruption is machine-readable");
        assert!(
            evident[0].derivation.contains("interrupted"),
            "and prompt-visible: {}",
            evident[0].derivation
        );
    }

    /// Blocking review finding: an interrupted recall must be
    /// distinguishable from a completed recall that genuinely established
    /// nothing — WITHOUT parsing prose. The `interrupted` flag is that
    /// signal: same shape, same Unknown truth, different flag.
    #[tokio::test]
    async fn interrupted_recall_distinguishable_from_genuine_unknown() {
        use concerto_core::memory::FactStatusView;

        // A bare indexed chunk with no corroboration, links, or
        // observations anywhere: a completed recall genuinely establishes
        // no status (truth Unknown, with its known age stated).
        let project = ProjectId("h11-genuine-unknown".into());
        let record = concerto_core::memory::EmbeddingRecord {
            id: "bare".into(),
            project_id: project.clone(),
            chunk_hash: "hash".into(),
            content: "genuine unknown marker content".into(),
            file_path: "src/bare.rs".into(),
            start_line: Some(1),
            end_line: Some(1),
            chunk_type: concerto_core::memory::ChunkType::Function,
            vector: vec![0.1; 8],
            model_id: "m".into(),
            model_version: "1".into(),
            stale: false,
            // No age: the TTL substrate stays silent too.
            created_at: OffsetDateTime::now_utc(),
        };
        let vector_store =
            Arc::new(InMemoryVectorStore::with_records(vec![(project.clone(), record)]));
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        fts_store
            .insert(
                &concerto_core::memory::MemoryChunk {
                    id: "bare".into(),
                    project_id: project.clone(),
                    namespace: MemoryNamespace::Project(project.clone()),
                    content: "genuine unknown marker content".into(),
                    file_path: Some("src/bare.rs".into()),
                    start_line: Some(1),
                    end_line: Some(1),
                    chunk_type: concerto_core::memory::ChunkType::Function,
                    score: 1.0,
                    model_id: "m".into(),
                    model_version: "1".into(),
                    stale: false,
                },
                &project,
                CancellationToken::new(),
            )
            .await
            .expect("fts seed");
        let system = MemorySystem::new(
            vector_store,
            fts_store,
            Arc::new(DecisionStore::new()),
            Arc::new(TaskTreeStore::new()),
            None,
            project.clone(),
            None,
        );

        let query = evident_query("h11-genuine-unknown", "genuine unknown marker");
        let done = system
            .retrieve_with_evidence(&query, CancellationToken::new())
            .await
            .expect("completed recall");
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].truth, FactStatusView::Unknown);
        assert!(!done[0].interrupted, "a completed recall is not marked interrupted");

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let cut = system.retrieve_with_evidence(&query, cancelled).await.expect("fail-open");
        assert_eq!(cut.len(), 1);
        assert!(cut[0].interrupted, "an interrupted recall is marked");
        // Both may read Unknown truth with an "unestablished" derivation —
        // the flag, not the prose, tells them apart.
        assert_eq!(cut[0].truth, FactStatusView::Unknown);
        assert_ne!(done[0].interrupted, cut[0].interrupted);
    }

    /// H11 wiring: vector-disabled recall still assembles evidence, with
    /// the reduced capability marked on every composite instead of hidden.
    #[tokio::test]
    async fn evidence_recall_degraded_fts_marks_degraded() {
        let project = "h11-degraded";
        let system = evident_system(project);
        system
            .store(
                fact_entry(project, "degraded fts-only marker content"),
                CancellationToken::new(),
            )
            .await
            .expect("store fact");

        // Break this project's embedder: the retriever must take the
        // FTS-only fallback. The project id is unique to this test so the
        // broken window cannot leak into any other test.
        let project_id = ProjectId(project.into());
        let health = crate::embedder_health::EmbedderHealth::for_project(&project_id);
        health.record_failure(std::time::Instant::now());
        let query = evident_query(project, "fts-only marker");
        let evident = system.retrieve_with_evidence(&query, CancellationToken::new()).await;
        health.record_success();
        let evident = evident.expect("degraded recall still serves");
        assert_eq!(evident.len(), 1, "FTS fallback still returns the chunk");
        assert!(evident[0].degraded, "reduced capability surfaces, never hides");
    }

    /// H11 wiring: a chunk whose metadata cites a superseded decision
    /// recalls `Superseded` with the decision cited (decision provenance).
    #[tokio::test]
    async fn evidence_recall_cited_superseded_decision_supersedes() {
        use concerto_core::ids::Ulid;
        use concerto_core::memory::{
            ChunkType, Decision, DecisionCategory, DecisionId, EmbeddingRecord, FactStatusView,
        };

        let project = ProjectId("h11-decision".into());
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("vector pool connects");
        let vector_store = Arc::new(
            crate::vector_store::SqliteVectorStore::new(pool.clone()).await.expect("open"),
        );
        let fts_store =
            Arc::new(crate::fts::SqliteFullTextStore::new(pool.clone()).await.expect("open"));

        // Two decisions; the first is superseded by the second.
        let decision_store = Arc::new(DecisionStore::new());
        let old = Decision {
            id: DecisionId(Ulid::new()),
            session_id: Ulid::nil(),
            task_id: None,
            what: "old approach".into(),
            why: "first try".into(),
            outcome: None,
            category: DecisionCategory::Architecture,
            confidence: 0.6,
            superseded_by: None,
            created_at: OffsetDateTime::now_utc(),
        };
        let new = Decision { id: DecisionId(Ulid::new()), ..old.clone() };
        let old_id = old.id;
        decision_store.insert(old).expect("insert old");
        decision_store.insert(new.clone()).expect("insert new");
        decision_store.supersede(old_id, new.id).expect("supersede");

        // A chunk citing the superseded decision in its metadata sidecar.
        let record = EmbeddingRecord {
            id: "citing-chunk".into(),
            project_id: project.clone(),
            chunk_hash: "hash".into(),
            content: "old approach notes cited decision marker".into(),
            file_path: "memory".into(),
            start_line: None,
            end_line: None,
            chunk_type: ChunkType::Fact,
            vector: vec![0.1; 8],
            model_id: "m".into(),
            model_version: "1".into(),
            stale: false,
            created_at: OffsetDateTime::now_utc(),
        };
        vector_store
            .store_projection(
                &record,
                &serde_json::json!({ "decision_id": old_id.to_string() }),
                CancellationToken::new(),
            )
            .await
            .expect("seed citing chunk");

        let system = MemorySystem::new(
            vector_store,
            fts_store,
            decision_store,
            Arc::new(TaskTreeStore::new()),
            Some(Arc::new(MockEmbeddingGenerator::new(8))),
            project.clone(),
            None,
        );

        let query = evident_query("h11-decision", "cited decision marker");
        let evident =
            system.retrieve_with_evidence(&query, CancellationToken::new()).await.expect("recall");
        assert_eq!(evident.len(), 1);
        assert_eq!(evident[0].truth, FactStatusView::Superseded);
        assert!(
            evident[0].derivation.contains(&old_id.to_string()),
            "cites the decision: {}",
            evident[0].derivation
        );
    }

    /// A seam provider that reports every path dirty.
    struct DirtySeam;

    #[async_trait::async_trait]
    impl crate::recall_status::PathObservationSource for DirtySeam {
        async fn observe_paths(
            &self,
            file_paths: &[String],
            _cancel: CancellationToken,
        ) -> HashMap<String, PathObservation> {
            file_paths
                .iter()
                .map(|path| {
                    (
                        path.clone(),
                        PathObservation {
                            dirty: true,
                            generation: None,
                            last_event_id: Some("ev-seam".to_string()),
                        },
                    )
                })
                .collect()
        }
    }

    /// H11 wiring: a dirty resource-fact observation stales the recalled
    /// indexed chunk through the seam, citing the observation.
    #[tokio::test]
    async fn evidence_recall_dirty_seam_observation_stales() {
        use concerto_core::memory::{ChunkType, EmbeddingRecord, FactStatusView, MemoryNamespace};

        let project = ProjectId("h11-seam".into());
        let record = EmbeddingRecord {
            id: "indexed-1".into(),
            project_id: project.clone(),
            chunk_hash: "hash".into(),
            content: "fn seam() {} seam observation marker".into(),
            file_path: "src/seam.rs".into(),
            start_line: Some(1),
            end_line: Some(1),
            chunk_type: ChunkType::Function,
            vector: vec![0.1; 8],
            model_id: "m".into(),
            model_version: "1".into(),
            stale: false,
            created_at: OffsetDateTime::now_utc(),
        };
        let vector_store =
            Arc::new(InMemoryVectorStore::with_records(vec![(project.clone(), record)]));
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        // The FTS side needs the same content: index it directly.
        fts_store
            .insert(
                &concerto_core::memory::MemoryChunk {
                    id: "indexed-1".into(),
                    project_id: project.clone(),
                    namespace: MemoryNamespace::Project(project.clone()),
                    content: "fn seam() {} seam observation marker".into(),
                    file_path: Some("src/seam.rs".into()),
                    start_line: Some(1),
                    end_line: Some(1),
                    chunk_type: ChunkType::Function,
                    score: 1.0,
                    model_id: "m".into(),
                    model_version: "1".into(),
                    stale: false,
                },
                &project,
                CancellationToken::new(),
            )
            .await
            .expect("fts seed");
        let system = MemorySystem::new(
            vector_store,
            fts_store,
            Arc::new(DecisionStore::new()),
            Arc::new(TaskTreeStore::new()),
            None,
            project.clone(),
            None,
        )
        .with_path_observations(Arc::new(DirtySeam));

        let query = evident_query("h11-seam", "seam observation marker");
        let evident =
            system.retrieve_with_evidence(&query, CancellationToken::new()).await.expect("recall");
        assert_eq!(evident.len(), 1);
        assert_eq!(evident[0].truth, FactStatusView::Stale);
        assert!(
            evident[0].derivation.contains("ev-seam"),
            "cites the observation: {}",
            evident[0].derivation
        );
    }

    #[test]
    fn derived_entry_carries_l1_provenance_and_parent_scope() {
        use crate::entities::L1MemoryType;

        let parent = summary_entry("Task: ship it");
        let memory = L1ExtractedMemory {
            id: ulid::Ulid::new().to_string(),
            kind: L1MemoryType::Instruction,
            scene: Some("review".into()),
            content: "always run fmt before pushing".into(),
            owner: None,
            deadline: None,
            status: Some("active".into()),
        };

        let derived = derived_l1_entry(&parent, &memory);

        assert_eq!(derived.chunk_type, ChunkType::Fact);
        assert_eq!(derived.namespace, parent.namespace);
        assert_eq!(derived.project_id, parent.project_id);
        assert_eq!(derived.content, memory.content);
        assert_eq!(derived.metadata["l1_kind"], "instruction");
        assert_eq!(derived.metadata["scene"], "review");
        assert_eq!(derived.metadata["status"], "active");
        assert_eq!(derived.metadata["session_id"], "session-1");
        assert_eq!(derived.metadata["derived_from"], parent.id.0.to_string());
        assert!(derived.metadata.get("owner").is_none(), "unset attributes stay unset");
    }
}
