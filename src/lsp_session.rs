//! Bounded application-side state for one live language-server service.
//!
//! The transport in [`crate::lsp_client`] bounds messages and queues. This
//! module supplies the corresponding UI-side bounds: a single service can
//! synchronize a fixed number of file-backed editor documents, and published
//! diagnostics are retained in a byte-estimated, least-recently-used cache.
//! Both stores use small vectors so secondary URI indexes do not duplicate
//! potentially long URI allocations.

use std::error::Error;
use std::fmt;
use std::mem;

use crate::lsp::DocumentVersion;
use crate::lsp_client::JsonValue;
use crate::lsp_ui::{
    Diagnostic, MAX_DIAGNOSTIC_RAW_BYTES, MAX_DIAGNOSTIC_RAW_DEPTH, MAX_DIAGNOSTIC_RAW_NODES,
    MAX_DIAGNOSTIC_URI_BYTES, file_uri_to_path,
};

/// Maximum file-backed editor documents synchronized with one service.
pub const MAX_SYNCHRONIZED_DOCUMENTS: usize = 64;

/// Maximum diagnostic URI buckets retained for one service.
pub const MAX_DIAGNOSTIC_URI_BUCKETS: usize = 128;

/// Maximum diagnostics retained from one publication URI.
pub const MAX_DIAGNOSTICS_PER_URI: usize = 1_024;

/// Maximum aggregate retained diagnostic estimate for one service.
pub const MAX_DIAGNOSTIC_RETAINED_BYTES: usize = 8 * 1024 * 1024;

/// Whether safety bounds have made a service-side view incomplete.
///
/// `Partial` is deliberately sticky for the life of a registry/cache. Once an
/// item has been evicted or a publication has been truncated, the owner cannot
/// prove that its view is complete until it clears state (normally when the
/// language server is restarted).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RetentionStatus {
    #[default]
    Complete,
    Partial,
}

impl RetentionStatus {
    pub const fn is_partial(self) -> bool {
        matches!(self, Self::Partial)
    }
}

/// Synchronization metadata for one file-backed editor document.
///
/// Identity fields are public for inexpensive application reads, but the
/// registry never exposes mutable document references. That preserves unique
/// editor-id and URI indexes after insertion.
#[derive(Debug, Eq, PartialEq)]
pub struct SynchronizedDocument {
    pub editor_id: u64,
    pub uri: String,
    pub version: DocumentVersion,
    pub state_id: u64,
    pub saved_state_id: Option<u64>,
    /// Monotonic document-local count of successful explicit saves.
    pub save_generation: u64,
    activity_ordinal: u64,
}

impl SynchronizedDocument {
    pub fn new(
        editor_id: u64,
        uri: impl Into<String>,
        version: DocumentVersion,
        state_id: u64,
        saved_state_id: Option<u64>,
        save_generation: u64,
    ) -> Self {
        Self {
            editor_id,
            uri: uri.into(),
            version,
            state_id,
            saved_state_id,
            save_generation,
            activity_ordinal: 0,
        }
    }

    /// Monotonic registry-local recency. Larger values are more recent.
    pub const fn activity_ordinal(&self) -> u64 {
        self.activity_ordinal
    }
}

/// Why a synchronized document could not be admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentInsertError {
    NonFileUri,
    DuplicateEditorId,
    DuplicateUri,
}

impl fmt::Display for DocumentInsertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::NonFileUri => {
                "synchronized documents require a bounded, control-free local file URI"
            }
            Self::DuplicateEditorId => "editor id is already synchronized",
            Self::DuplicateUri => "document URI is already synchronized",
        };
        formatter.write_str(message)
    }
}

impl Error for DocumentInsertError {}

/// Result of admitting a newly active synchronized document.
#[derive(Debug, Eq, PartialEq)]
pub struct DocumentInsertReport {
    /// Least-recently-active document removed to make room, if any. The caller
    /// should send `textDocument/didClose` for this document.
    pub evicted: Option<SynchronizedDocument>,
    /// Cache-wide completeness after this admission.
    pub retention: RetentionStatus,
}

impl DocumentInsertReport {
    pub const fn evicted(&self) -> bool {
        self.evicted.is_some()
    }

    pub const fn is_partial(&self) -> bool {
        self.retention.is_partial()
    }
}

/// Bounded synchronized-document registry for one language-server service.
#[derive(Debug, Default)]
pub struct SynchronizedDocumentRegistry {
    documents: Vec<SynchronizedDocument>,
    active_editor_id: Option<u64>,
    next_activity_ordinal: u64,
    revision: u128,
    retention: RetentionStatus,
}

impl SynchronizedDocumentRegistry {
    pub fn new() -> Self {
        Self {
            next_activity_ordinal: 1,
            ..Self::default()
        }
    }

    pub fn len(&self) -> usize {
        self.documents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    pub const fn retention_status(&self) -> RetentionStatus {
        self.retention
    }

    pub const fn is_partial(&self) -> bool {
        self.retention.is_partial()
    }

    /// Record that an eligible document could not be synchronized without
    /// forcing registry churn. Partial state remains sticky until [`Self::clear`].
    pub fn mark_partial(&mut self) {
        self.retention = RetentionStatus::Partial;
    }

    pub const fn active_editor_id(&self) -> Option<u64> {
        self.active_editor_id
    }

    /// Changes whenever synchronized document identity or snapshot metadata
    /// changes. Focus/LRU touches deliberately do not advance it.
    pub const fn revision(&self) -> u128 {
        self.revision
    }

    pub fn active(&self) -> Option<&SynchronizedDocument> {
        self.active_editor_id
            .and_then(|editor_id| self.get_by_editor_id(editor_id))
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &SynchronizedDocument> {
        self.documents.iter()
    }

    pub fn get_by_editor_id(&self, editor_id: u64) -> Option<&SynchronizedDocument> {
        self.documents
            .iter()
            .find(|document| document.editor_id == editor_id)
    }

    pub fn get_by_uri(&self, uri: &str) -> Option<&SynchronizedDocument> {
        self.documents.iter().find(|document| document.uri == uri)
    }

    /// Admit a file-backed document and make it the active registry entry.
    ///
    /// Identity collisions are rejected without changing the registry. At
    /// capacity, the least-recently-active existing document is removed before
    /// the new active document is inserted, so the admission cannot evict
    /// itself. Equal ordinals (possible only after external corruption or a
    /// clock rebase) are resolved by editor id and then URI.
    pub fn insert(
        &mut self,
        mut document: SynchronizedDocument,
    ) -> Result<DocumentInsertReport, DocumentInsertError> {
        if !local_file_uri_is_valid(&document.uri) {
            return Err(DocumentInsertError::NonFileUri);
        }
        if self.get_by_editor_id(document.editor_id).is_some() {
            return Err(DocumentInsertError::DuplicateEditorId);
        }
        if self.get_by_uri(&document.uri).is_some() {
            return Err(DocumentInsertError::DuplicateUri);
        }

        let evicted = (self.documents.len() >= MAX_SYNCHRONIZED_DOCUMENTS)
            .then(|| self.remove_lru_document())
            .flatten();
        if evicted.is_some() {
            self.retention = RetentionStatus::Partial;
        }

        document.activity_ordinal = self.allocate_activity_ordinal();
        self.active_editor_id = Some(document.editor_id);
        self.documents.push(document);
        self.revision = self.revision.saturating_add(1);
        debug_assert!(self.documents.len() <= MAX_SYNCHRONIZED_DOCUMENTS);

        Ok(DocumentInsertReport {
            evicted,
            retention: self.retention,
        })
    }

    /// Update synchronization revisions without changing identity or recency.
    ///
    /// Activity is intentionally separate: frequent `didChange` publications
    /// should not make a background document look user-active. Call
    /// [`Self::mark_active`] when editor focus changes.
    pub fn update(
        &mut self,
        editor_id: u64,
        version: DocumentVersion,
        state_id: u64,
        saved_state_id: Option<u64>,
        save_generation: u64,
    ) -> bool {
        let Some(document) = self
            .documents
            .iter_mut()
            .find(|document| document.editor_id == editor_id)
        else {
            return false;
        };
        if document.version != version
            || document.state_id != state_id
            || document.saved_state_id != saved_state_id
            || document.save_generation != save_generation
        {
            document.version = version;
            document.state_id = state_id;
            document.saved_state_id = saved_state_id;
            document.save_generation = save_generation;
            self.revision = self.revision.saturating_add(1);
        }
        true
    }

    /// Mark an existing synchronized editor as active and most recently used.
    pub fn mark_active(&mut self, editor_id: u64) -> bool {
        let Some(index) = self
            .documents
            .iter()
            .position(|document| document.editor_id == editor_id)
        else {
            return false;
        };
        let ordinal = self.allocate_activity_ordinal();
        self.documents[index].activity_ordinal = ordinal;
        self.active_editor_id = Some(editor_id);
        true
    }

    pub fn remove_by_editor_id(&mut self, editor_id: u64) -> Option<SynchronizedDocument> {
        let index = self
            .documents
            .iter()
            .position(|document| document.editor_id == editor_id)?;
        self.remove_at(index)
    }

    pub fn remove_by_uri(&mut self, uri: &str) -> Option<SynchronizedDocument> {
        let index = self
            .documents
            .iter()
            .position(|document| document.uri == uri)?;
        self.remove_at(index)
    }

    /// Remove every document, reset activity numbering, and restore a complete
    /// empty view. Returned documents let the caller send bounded `didClose`
    /// notifications if the service is still alive.
    pub fn clear(&mut self) -> Vec<SynchronizedDocument> {
        self.active_editor_id = None;
        self.next_activity_ordinal = 1;
        self.retention = RetentionStatus::Complete;
        let documents = mem::take(&mut self.documents);
        if !documents.is_empty() {
            self.revision = self.revision.saturating_add(1);
        }
        documents
    }

    fn remove_lru_document(&mut self) -> Option<SynchronizedDocument> {
        let index = self
            .documents
            .iter()
            .enumerate()
            .min_by(|(_, left), (_, right)| {
                document_recency_key(left).cmp(&document_recency_key(right))
            })
            .map(|(index, _)| index)?;
        self.remove_at(index)
    }

    fn remove_at(&mut self, index: usize) -> Option<SynchronizedDocument> {
        if index >= self.documents.len() {
            return None;
        }
        let document = self.documents.remove(index);
        self.revision = self.revision.saturating_add(1);
        if self.active_editor_id == Some(document.editor_id) {
            self.active_editor_id = None;
        }
        Some(document)
    }

    fn allocate_activity_ordinal(&mut self) -> u64 {
        if self.next_activity_ordinal == 0 || self.next_activity_ordinal == u64::MAX {
            self.rebase_activity_ordinals();
        }
        let ordinal = self.next_activity_ordinal;
        self.next_activity_ordinal = self.next_activity_ordinal.saturating_add(1);
        ordinal
    }

    fn rebase_activity_ordinals(&mut self) {
        let mut order = (0..self.documents.len()).collect::<Vec<_>>();
        order.sort_by(|left, right| {
            document_recency_key(&self.documents[*left])
                .cmp(&document_recency_key(&self.documents[*right]))
        });
        for (rank, index) in order.into_iter().enumerate() {
            self.documents[index].activity_ordinal = rank as u64 + 1;
        }
        self.next_activity_ordinal = self.documents.len() as u64 + 1;
    }
}

fn document_recency_key(document: &SynchronizedDocument) -> (u64, u64, &str) {
    (document.activity_ordinal, document.editor_id, &document.uri)
}

fn local_file_uri_is_valid(uri: &str) -> bool {
    if uri.is_empty() || uri.len() > MAX_DIAGNOSTIC_URI_BYTES || uri.chars().any(char::is_control) {
        return false;
    }
    file_uri_to_path(uri).is_ok()
}

#[derive(Debug)]
struct DiagnosticBucket {
    uri: String,
    diagnostics: Vec<Diagnostic>,
    activity_ordinal: u64,
    retained_estimate_bytes: usize,
}

/// Outcome of replacing one URI's diagnostics.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DiagnosticReplaceReport {
    /// Diagnostics supplied by the caller before any bounds are applied.
    pub received: usize,
    /// Diagnostics retained for this URI after the replacement.
    pub retained: usize,
    /// Supplied diagnostics omitted by per-URI, URI-consistency, or byte bounds.
    pub dropped: usize,
    /// Whether any supplied diagnostic was omitted.
    pub truncated: bool,
    /// Other URI buckets evicted to satisfy count or aggregate-byte bounds.
    pub evicted_buckets: usize,
    /// Diagnostics contained by the evicted buckets.
    pub evicted_diagnostics: usize,
    /// Cache-wide completeness after this replacement.
    pub retention: RetentionStatus,
}

impl DiagnosticReplaceReport {
    pub const fn is_partial(self) -> bool {
        self.retention.is_partial()
    }
}

/// Bounded diagnostic cache for one language-server service.
#[derive(Debug, Default)]
pub struct DiagnosticCache {
    buckets: Vec<DiagnosticBucket>,
    retained_estimate_bytes: usize,
    next_activity_ordinal: u64,
    retention: RetentionStatus,
}

impl DiagnosticCache {
    pub fn new() -> Self {
        Self {
            next_activity_ordinal: 1,
            ..Self::default()
        }
    }

    /// Number of URI buckets, not the aggregate number of diagnostics.
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    pub fn diagnostic_count(&self) -> usize {
        self.buckets
            .iter()
            .map(|bucket| bucket.diagnostics.len())
            .sum()
    }

    pub const fn retained_estimate_bytes(&self) -> usize {
        self.retained_estimate_bytes
    }

    pub const fn retention_status(&self) -> RetentionStatus {
        self.retention
    }

    pub const fn is_partial(&self) -> bool {
        self.retention.is_partial()
    }

    /// Record upstream parsing or publication loss that the cache cannot infer
    /// from the retained diagnostics. Partial state remains sticky until
    /// [`Self::clear`].
    pub fn mark_partial(&mut self) {
        self.retention = RetentionStatus::Partial;
    }

    /// Read a URI bucket without changing LRU order.
    pub fn get(&self, uri: &str) -> Option<&[Diagnostic]> {
        self.buckets
            .iter()
            .find(|bucket| bucket.uri == uri)
            .map(|bucket| bucket.diagnostics.as_slice())
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &[Diagnostic])> {
        self.buckets
            .iter()
            .map(|bucket| (bucket.uri.as_str(), bucket.diagnostics.as_slice()))
    }

    /// Refresh a bucket's LRU activity without cloning its URI or diagnostics.
    pub fn mark_used(&mut self, uri: &str) -> bool {
        let Some(index) = self.buckets.iter().position(|bucket| bucket.uri == uri) else {
            return false;
        };
        let ordinal = self.allocate_activity_ordinal();
        self.buckets[index].activity_ordinal = ordinal;
        true
    }

    /// Replace all diagnostics for one URI.
    ///
    /// The URI and diagnostics are consumed, so normal publications require no
    /// cache-wide clones. At most `MAX_DIAGNOSTICS_PER_URI` values are inspected
    /// and retained. An empty vector explicitly clears the URI bucket. A
    /// diagnostic whose embedded URI differs from the bucket URI is omitted so
    /// navigation cannot escape the publication identity. Invalid, remote, or
    /// over-limit bucket URIs retain nothing and make the cache partial.
    pub fn replace(
        &mut self,
        uri: String,
        diagnostics: Vec<Diagnostic>,
    ) -> DiagnosticReplaceReport {
        let received = diagnostics.len();
        if !local_file_uri_is_valid(&uri) {
            self.retention = RetentionStatus::Partial;
            return DiagnosticReplaceReport {
                received,
                dropped: received,
                truncated: received != 0,
                retention: self.retention,
                ..DiagnosticReplaceReport::default()
            };
        }
        self.purge(&uri);

        if diagnostics.is_empty() {
            return DiagnosticReplaceReport {
                received,
                retention: self.retention,
                ..DiagnosticReplaceReport::default()
            };
        }

        let uri = compact_string(uri);
        let base_estimate = mem::size_of::<DiagnosticBucket>().saturating_add(uri.capacity());
        let mut retained = Vec::new();
        let mut retained_heap_bytes = 0_usize;

        if base_estimate <= MAX_DIAGNOSTIC_RETAINED_BYTES {
            for diagnostic in diagnostics.into_iter().take(MAX_DIAGNOSTICS_PER_URI) {
                if diagnostic.uri != uri {
                    continue;
                }
                let Some(heap_bytes) =
                    diagnostic_heap_estimate(&diagnostic, MAX_DIAGNOSTIC_RETAINED_BYTES)
                else {
                    continue;
                };
                let projected = base_estimate
                    .saturating_add(
                        (retained.len() + 1).saturating_mul(mem::size_of::<Diagnostic>()),
                    )
                    .saturating_add(retained_heap_bytes)
                    .saturating_add(heap_bytes);
                if projected <= MAX_DIAGNOSTIC_RETAINED_BYTES {
                    retained.push(diagnostic);
                    retained_heap_bytes = retained_heap_bytes.saturating_add(heap_bytes);
                }
            }
        }

        // Box conversion sheds any spare capacity retained while filtering.
        let retained = retained.into_boxed_slice().into_vec();
        let retained_count = retained.len();
        let dropped = received.saturating_sub(retained_count);
        let truncated = dropped != 0;
        if truncated {
            self.retention = RetentionStatus::Partial;
        }

        if retained.is_empty() {
            return DiagnosticReplaceReport {
                received,
                retained: 0,
                dropped,
                truncated,
                retention: self.retention,
                ..DiagnosticReplaceReport::default()
            };
        }

        let bucket_estimate = base_estimate
            .saturating_add(retained.len().saturating_mul(mem::size_of::<Diagnostic>()))
            .saturating_add(retained_heap_bytes);
        debug_assert!(bucket_estimate <= MAX_DIAGNOSTIC_RETAINED_BYTES);

        let mut evicted_buckets = 0_usize;
        let mut evicted_diagnostics = 0_usize;
        while self.buckets.len() >= MAX_DIAGNOSTIC_URI_BUCKETS
            || self.retained_estimate_bytes.saturating_add(bucket_estimate)
                > MAX_DIAGNOSTIC_RETAINED_BYTES
        {
            let Some(evicted) = self.remove_lru_bucket() else {
                break;
            };
            evicted_buckets += 1;
            evicted_diagnostics = evicted_diagnostics.saturating_add(evicted.diagnostics.len());
        }
        if evicted_buckets != 0 {
            self.retention = RetentionStatus::Partial;
        }

        let activity_ordinal = self.allocate_activity_ordinal();
        self.retained_estimate_bytes = self.retained_estimate_bytes.saturating_add(bucket_estimate);
        self.buckets.push(DiagnosticBucket {
            uri,
            diagnostics: retained,
            activity_ordinal,
            retained_estimate_bytes: bucket_estimate,
        });

        debug_assert!(self.buckets.len() <= MAX_DIAGNOSTIC_URI_BUCKETS);
        debug_assert!(self.retained_estimate_bytes <= MAX_DIAGNOSTIC_RETAINED_BYTES);

        DiagnosticReplaceReport {
            received,
            retained: retained_count,
            dropped,
            truncated,
            evicted_buckets,
            evicted_diagnostics,
            retention: self.retention,
        }
    }

    /// Remove one URI bucket. Sticky partial state is intentionally preserved.
    pub fn purge(&mut self, uri: &str) -> bool {
        let Some(index) = self.buckets.iter().position(|bucket| bucket.uri == uri) else {
            return false;
        };
        self.remove_bucket_at(index);
        true
    }

    /// Drop all diagnostic state and restore a complete empty view.
    pub fn clear(&mut self) {
        self.buckets.clear();
        self.retained_estimate_bytes = 0;
        self.next_activity_ordinal = 1;
        self.retention = RetentionStatus::Complete;
    }

    fn remove_lru_bucket(&mut self) -> Option<DiagnosticBucket> {
        let index = self
            .buckets
            .iter()
            .enumerate()
            .min_by(|(_, left), (_, right)| {
                bucket_recency_key(left).cmp(&bucket_recency_key(right))
            })
            .map(|(index, _)| index)?;
        Some(self.remove_bucket_at(index))
    }

    fn remove_bucket_at(&mut self, index: usize) -> DiagnosticBucket {
        let bucket = self.buckets.remove(index);
        self.retained_estimate_bytes = self
            .retained_estimate_bytes
            .saturating_sub(bucket.retained_estimate_bytes);
        bucket
    }

    fn allocate_activity_ordinal(&mut self) -> u64 {
        if self.next_activity_ordinal == 0 || self.next_activity_ordinal == u64::MAX {
            self.rebase_activity_ordinals();
        }
        let ordinal = self.next_activity_ordinal;
        self.next_activity_ordinal = self.next_activity_ordinal.saturating_add(1);
        ordinal
    }

    fn rebase_activity_ordinals(&mut self) {
        let mut order = (0..self.buckets.len()).collect::<Vec<_>>();
        order.sort_by(|left, right| {
            bucket_recency_key(&self.buckets[*left]).cmp(&bucket_recency_key(&self.buckets[*right]))
        });
        for (rank, index) in order.into_iter().enumerate() {
            self.buckets[index].activity_ordinal = rank as u64 + 1;
        }
        self.next_activity_ordinal = self.buckets.len() as u64 + 1;
    }
}

fn bucket_recency_key(bucket: &DiagnosticBucket) -> (u64, &str) {
    (bucket.activity_ordinal, &bucket.uri)
}

fn compact_string(value: String) -> String {
    value.into_boxed_str().into()
}

/// Estimate heap bytes uniquely retained beneath one diagnostic while
/// independently enforcing the adapter's raw JSON byte, node, and depth caps.
///
/// The inline `Diagnostic` itself is accounted for by its bucket's vector.
/// Arrays expose their actual element capacity. `BTreeMap` does not expose node
/// occupancy or spare slots, so every object gets at least one fully populated
/// worst-case node and, conservatively, up to one such node per live entry.
/// Number capacity is likewise private, so twice its source length (with an
/// allocator-word floor) is charged.
///
/// `None` means either the retained estimate exceeded `limit` or the raw JSON
/// exceeded a structural ceiling. The explicit work stack can never contain or
/// schedule more than `MAX_DIAGNOSTIC_RAW_NODES` entries.
fn diagnostic_heap_estimate(diagnostic: &Diagnostic, limit: usize) -> Option<usize> {
    let mut estimate = diagnostic
        .uri
        .capacity()
        .saturating_add(diagnostic.message.capacity())
        .saturating_add(
            diagnostic
                .source
                .as_ref()
                .map_or(0, |source| source.capacity()),
        );
    if estimate > limit {
        return None;
    }

    let mut raw_bytes = 0_usize;
    let mut scheduled_nodes = 1_usize;
    let mut pending = Vec::with_capacity(MAX_DIAGNOSTIC_RAW_NODES);
    pending.push((&diagnostic.raw, 1_usize));
    while let Some((value, depth)) = pending.pop() {
        if depth > MAX_DIAGNOSTIC_RAW_DEPTH {
            return None;
        }
        match value {
            JsonValue::Null => {
                add_bounded_raw_bytes(&mut raw_bytes, 4)?;
            }
            JsonValue::Bool(value) => {
                add_bounded_raw_bytes(&mut raw_bytes, if *value { 4 } else { 5 })?;
            }
            JsonValue::Number(number) => {
                let len = number.as_str().len();
                add_bounded_raw_bytes(&mut raw_bytes, len)?;
                let allocation = len.saturating_mul(2).max(mem::size_of::<usize>());
                add_bounded_estimate(&mut estimate, allocation, limit)?;
            }
            JsonValue::String(value) => {
                let encoded = bounded_json_string_len(value)?;
                add_bounded_raw_bytes(&mut raw_bytes, encoded)?;
                add_bounded_estimate(&mut estimate, value.capacity(), limit)?;
            }
            JsonValue::Array(values) => {
                let structural_bytes = 2_usize.checked_add(values.len().saturating_sub(1))?;
                add_bounded_raw_bytes(&mut raw_bytes, structural_bytes)?;
                schedule_json_children(&mut scheduled_nodes, values.len())?;
                if !values.is_empty() && depth >= MAX_DIAGNOSTIC_RAW_DEPTH {
                    return None;
                }
                let allocation = values
                    .capacity()
                    .saturating_mul(mem::size_of::<JsonValue>());
                add_bounded_estimate(&mut estimate, allocation, limit)?;
                pending.extend(values.iter().rev().map(|value| (value, depth + 1)));
            }
            JsonValue::Object(values) => {
                let structural_bytes = 2_usize
                    .checked_add(values.len().saturating_sub(1))?
                    .checked_add(values.len())?;
                add_bounded_raw_bytes(&mut raw_bytes, structural_bytes)?;
                schedule_json_children(&mut scheduled_nodes, values.len())?;
                if !values.is_empty() && depth >= MAX_DIAGNOSTIC_RAW_DEPTH {
                    return None;
                }
                add_bounded_estimate(
                    &mut estimate,
                    btree_object_allocation_estimate(values.len()),
                    limit,
                )?;
                for key in values.keys() {
                    let encoded = bounded_json_string_len(key)?;
                    add_bounded_raw_bytes(&mut raw_bytes, encoded)?;
                    add_bounded_estimate(&mut estimate, key.capacity(), limit)?;
                }
                pending.extend(values.values().rev().map(|value| (value, depth + 1)));
            }
        }
        debug_assert!(scheduled_nodes <= MAX_DIAGNOSTIC_RAW_NODES);
        debug_assert!(pending.len() <= MAX_DIAGNOSTIC_RAW_NODES);
    }
    Some(estimate)
}

/// `std::collections::BTreeMap` currently stores multiple key/value slots per
/// allocated node. Its capacity is intentionally not public, so this uses the
/// standard 11-slot layout plus full internal-edge/header allowances and then
/// assumes a maximally sparse one-node-per-entry tree. The one-node floor is
/// also charged for an empty object to stay conservative across implementations.
fn btree_object_allocation_estimate(entries: usize) -> usize {
    const WORST_CASE_NODE_SLOTS: usize = 11;
    const WORST_CASE_CHILD_EDGES: usize = WORST_CASE_NODE_SLOTS + 1;
    const NODE_HEADER_WORDS: usize = 8;

    let slot_bytes = mem::size_of::<String>().saturating_add(mem::size_of::<JsonValue>());
    let node_bytes = NODE_HEADER_WORDS
        .saturating_mul(mem::size_of::<usize>())
        .saturating_add(WORST_CASE_NODE_SLOTS.saturating_mul(slot_bytes))
        .saturating_add(WORST_CASE_CHILD_EDGES.saturating_mul(mem::size_of::<usize>()));
    entries.max(1).saturating_mul(node_bytes)
}

fn add_bounded_estimate(total: &mut usize, amount: usize, limit: usize) -> Option<()> {
    let updated = total.checked_add(amount)?;
    if updated > limit {
        return None;
    }
    *total = updated;
    Some(())
}

fn add_bounded_raw_bytes(total: &mut usize, amount: usize) -> Option<()> {
    let updated = total.checked_add(amount)?;
    if updated > MAX_DIAGNOSTIC_RAW_BYTES {
        return None;
    }
    *total = updated;
    Some(())
}

fn schedule_json_children(total: &mut usize, children: usize) -> Option<()> {
    let updated = total.checked_add(children)?;
    if updated > MAX_DIAGNOSTIC_RAW_NODES {
        return None;
    }
    *total = updated;
    Some(())
}

fn bounded_json_string_len(value: &str) -> Option<usize> {
    let mut bytes = 2_usize;
    for character in value.chars() {
        let encoded = match character {
            '"' | '\\' | '\u{08}' | '\u{0c}' | '\n' | '\r' | '\t' => 2,
            character if character <= '\u{1f}' => 6,
            character => character.len_utf8(),
        };
        bytes = bytes.checked_add(encoded)?;
        if bytes > MAX_DIAGNOSTIC_RAW_BYTES {
            return None;
        }
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::lsp::{Line, LspPosition, Utf16Offset};
    use crate::lsp_client::LspRange;
    use crate::lsp_ui::DiagnosticSeverity;

    use super::*;

    fn document(editor_id: u64) -> SynchronizedDocument {
        SynchronizedDocument::new(
            editor_id,
            format!("file:///workspace/{editor_id}.rs"),
            DocumentVersion::new(editor_id),
            editor_id * 10,
            Some(editor_id * 10),
            editor_id * 100,
        )
    }

    fn diagnostic(uri: &str, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            uri: uri.to_owned(),
            range: LspRange::new(
                LspPosition::new(Line::ZERO, Utf16Offset::ZERO),
                LspPosition::new(Line::ZERO, Utf16Offset::new(1)),
            ),
            severity: DiagnosticSeverity::Warning,
            message: message.into(),
            source: Some("test".to_owned()),
            raw: JsonValue::Null,
        }
    }

    fn nested_array(mut value: JsonValue, wrappers: usize) -> JsonValue {
        for _ in 0..wrappers {
            value = JsonValue::Array(vec![value]);
        }
        value
    }

    /// Sixty-three wrappers plus the terminal value put the leaf at depth 64.
    fn depth_64_singleton_object() -> JsonValue {
        let mut value = JsonValue::Null;
        for _ in 1..MAX_DIAGNOSTIC_RAW_DEPTH {
            let mut object = BTreeMap::new();
            object.insert("k".to_owned(), value);
            value = JsonValue::Object(object);
        }
        value
    }

    fn diagnostic_with_raw(uri: &str, raw: JsonValue) -> Diagnostic {
        let mut value = diagnostic(uri, "raw");
        value.raw = raw;
        value
    }

    #[test]
    fn document_registry_inserts_looks_up_and_tracks_active_recency() {
        let mut registry = SynchronizedDocumentRegistry::new();
        let first = registry.insert(document(1)).unwrap();
        let first_ordinal = registry.get_by_editor_id(1).unwrap().activity_ordinal();
        let second = registry.insert(document(2)).unwrap();

        assert_eq!(first.retention, RetentionStatus::Complete);
        assert!(!first.evicted());
        assert_eq!(second.retention, RetentionStatus::Complete);
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.active_editor_id(), Some(2));
        assert_eq!(registry.active().unwrap().editor_id, 2);
        assert_eq!(registry.active().unwrap().save_generation, 200);
        assert_eq!(
            registry
                .get_by_uri("file:///workspace/1.rs")
                .unwrap()
                .editor_id,
            1
        );
        assert!(registry.get_by_editor_id(2).unwrap().activity_ordinal() > first_ordinal);

        assert!(registry.mark_active(1));
        assert_eq!(registry.active_editor_id(), Some(1));
        assert!(
            registry.get_by_editor_id(1).unwrap().activity_ordinal()
                > registry.get_by_editor_id(2).unwrap().activity_ordinal()
        );
        assert!(!registry.mark_active(99));
        assert_eq!(registry.active_editor_id(), Some(1));
    }

    #[test]
    fn document_registry_updates_revisions_without_changing_recency() {
        let mut registry = SynchronizedDocumentRegistry::new();
        registry.insert(document(7)).unwrap();
        let activity = registry.get_by_editor_id(7).unwrap().activity_ordinal();
        assert_eq!(registry.get_by_editor_id(7).unwrap().save_generation, 700);

        assert!(registry.update(7, DocumentVersion::new(19), 23, None, 701));
        let updated = registry.get_by_editor_id(7).unwrap();
        assert_eq!(updated.version, DocumentVersion::new(19));
        assert_eq!(updated.state_id, 23);
        assert_eq!(updated.saved_state_id, None);
        assert_eq!(updated.save_generation, 701);
        assert_eq!(updated.activity_ordinal(), activity);
        assert!(!registry.update(8, DocumentVersion::new(1), 1, Some(1), 1));
    }

    #[test]
    fn document_registry_revision_advances_for_identity_and_metadata_mutations() {
        let mut registry = SynchronizedDocumentRegistry::new();
        assert_eq!(registry.revision(), 0);

        registry.insert(document(1)).unwrap();
        assert_eq!(registry.revision(), 1);

        assert!(registry.update(1, DocumentVersion::new(2), 11, Some(11), 101));
        assert_eq!(registry.revision(), 2);

        assert!(registry.remove_by_editor_id(1).is_some());
        assert_eq!(registry.revision(), 3);

        registry.insert(document(2)).unwrap();
        assert_eq!(registry.revision(), 4);
        assert_eq!(registry.clear().len(), 1);
        assert_eq!(registry.revision(), 5);
    }

    #[test]
    fn document_registry_revision_ignores_activity_and_noop_updates() {
        let mut registry = SynchronizedDocumentRegistry::new();
        registry.insert(document(7)).unwrap();
        let revision = registry.revision();
        let document = registry.get_by_editor_id(7).unwrap();
        let version = document.version;
        let state_id = document.state_id;
        let saved_state_id = document.saved_state_id;
        let save_generation = document.save_generation;

        assert!(registry.mark_active(7));
        assert_eq!(registry.revision(), revision);

        assert!(registry.update(7, version, state_id, saved_state_id, save_generation,));
        assert_eq!(registry.revision(), revision);
    }

    #[test]
    fn document_registry_rejects_identity_collisions_without_mutation() {
        let mut registry = SynchronizedDocumentRegistry::new();
        registry.insert(document(1)).unwrap();
        let active = registry.active_editor_id();

        assert_eq!(
            registry.insert(SynchronizedDocument::new(
                1,
                "file:///workspace/other.rs",
                DocumentVersion::new(1),
                1,
                None,
                0,
            )),
            Err(DocumentInsertError::DuplicateEditorId)
        );
        assert_eq!(
            registry.insert(SynchronizedDocument::new(
                2,
                "file:///workspace/1.rs",
                DocumentVersion::new(1),
                1,
                None,
                0,
            )),
            Err(DocumentInsertError::DuplicateUri)
        );
        assert_eq!(
            registry.insert(SynchronizedDocument::new(
                2,
                "untitled:2",
                DocumentVersion::new(1),
                1,
                None,
                0,
            )),
            Err(DocumentInsertError::NonFileUri)
        );
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.active_editor_id(), active);
        assert_eq!(registry.retention_status(), RetentionStatus::Complete);
    }

    #[test]
    fn registry_and_cache_require_bounded_control_free_local_file_uris() {
        let exact_uri = format!(
            "file:///{}",
            "a".repeat(MAX_DIAGNOSTIC_URI_BYTES - "file:///".len())
        );
        assert_eq!(exact_uri.len(), MAX_DIAGNOSTIC_URI_BYTES);

        let mut registry = SynchronizedDocumentRegistry::new();
        registry
            .insert(SynchronizedDocument::new(
                1,
                exact_uri.clone(),
                DocumentVersion::new(1),
                1,
                None,
                0,
            ))
            .unwrap();
        assert_eq!(registry.len(), 1);

        let invalid_uris = [
            format!("{exact_uri}x"),
            "file:///tmp/raw\u{1b}.rs".to_owned(),
            "file:///tmp/encoded%1B.rs".to_owned(),
            "file://remote/tmp/a.rs".to_owned(),
            "file:///tmp/%QQ.rs".to_owned(),
            "https://example.test/a.rs".to_owned(),
        ];
        for (offset, invalid_uri) in invalid_uris.iter().enumerate() {
            assert_eq!(
                registry.insert(SynchronizedDocument::new(
                    offset as u64 + 2,
                    invalid_uri,
                    DocumentVersion::new(1),
                    1,
                    None,
                    0,
                )),
                Err(DocumentInsertError::NonFileUri),
                "{invalid_uri:?}"
            );
        }
        assert_eq!(registry.len(), 1);

        let mut cache = DiagnosticCache::new();
        let exact = cache.replace(
            exact_uri.clone(),
            vec![diagnostic(&exact_uri, "exact bound")],
        );
        assert_eq!(exact.retained, 1);
        assert!(!exact.is_partial());

        for invalid_uri in invalid_uris {
            let report = cache.replace(
                invalid_uri.clone(),
                vec![diagnostic(&invalid_uri, "invalid URI")],
            );
            assert_eq!(report.received, 1, "{invalid_uri:?}");
            assert_eq!(report.retained, 0, "{invalid_uri:?}");
            assert_eq!(report.dropped, 1, "{invalid_uri:?}");
            assert!(report.truncated, "{invalid_uri:?}");
            assert!(report.is_partial(), "{invalid_uri:?}");
        }
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&exact_uri).is_some());

        let localhost = "file://localhost/tmp/local.rs";
        cache.clear();
        assert_eq!(
            cache
                .replace(
                    localhost.to_owned(),
                    vec![diagnostic(localhost, "local host")],
                )
                .retained,
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn registry_and_cache_admit_distinct_non_utf8_unix_basenames() {
        use std::os::unix::ffi::OsStringExt as _;

        use crate::lsp_client::file_uri;

        let first_path = std::path::PathBuf::from(std::ffi::OsString::from_vec(
            b"/workspace/non-utf8-\x80.rs".to_vec(),
        ));
        let second_path = std::path::PathBuf::from(std::ffi::OsString::from_vec(
            b"/workspace/non-utf8-\x81.rs".to_vec(),
        ));
        let first_uri = file_uri(&first_path);
        let second_uri = file_uri(&second_path);
        assert_ne!(first_uri, second_uri);
        assert!(first_uri.len() <= MAX_DIAGNOSTIC_URI_BYTES);
        assert!(second_uri.len() <= MAX_DIAGNOSTIC_URI_BYTES);

        let mut registry = SynchronizedDocumentRegistry::new();
        for (editor_id, uri) in [(1, &first_uri), (2, &second_uri)] {
            let report = registry
                .insert(SynchronizedDocument::new(
                    editor_id,
                    uri.clone(),
                    DocumentVersion::new(1),
                    editor_id,
                    Some(editor_id),
                    editor_id,
                ))
                .unwrap();
            assert!(!report.evicted());
            assert!(!report.is_partial());
        }
        assert_eq!(registry.len(), 2);
        assert!(registry.get_by_uri(&first_uri).is_some());
        assert!(registry.get_by_uri(&second_uri).is_some());

        let mut cache = DiagnosticCache::new();
        for uri in [&first_uri, &second_uri] {
            let report = cache.replace(uri.clone(), vec![diagnostic(uri, "non-UTF-8 path")]);
            assert_eq!(report.retained, 1);
            assert!(!report.is_partial());
        }
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.diagnostic_count(), 2);
        assert!(cache.get(&first_uri).is_some());
        assert!(cache.get(&second_uri).is_some());
    }

    #[test]
    fn document_registry_evicts_the_deterministic_lru_before_new_admission() {
        let mut registry = SynchronizedDocumentRegistry::new();
        for editor_id in 1..=MAX_SYNCHRONIZED_DOCUMENTS as u64 {
            registry.insert(document(editor_id)).unwrap();
        }
        assert!(registry.mark_active(1));

        let report = registry.document_admission_for_test(65);

        assert_eq!(
            report.evicted.as_ref().map(|entry| entry.editor_id),
            Some(2)
        );
        assert!(report.is_partial());
        assert_eq!(registry.len(), MAX_SYNCHRONIZED_DOCUMENTS);
        assert!(registry.get_by_editor_id(1).is_some());
        assert!(registry.get_by_editor_id(2).is_none());
        assert!(registry.get_by_editor_id(65).is_some());
        assert_eq!(registry.active_editor_id(), Some(65));
    }

    #[test]
    fn document_registry_remove_and_clear_reset_only_at_session_boundary() {
        let mut registry = SynchronizedDocumentRegistry::new();
        for editor_id in 1..=MAX_SYNCHRONIZED_DOCUMENTS as u64 + 1 {
            registry.insert(document(editor_id)).unwrap();
        }
        assert!(registry.is_partial());

        let active = registry.remove_by_editor_id(65).unwrap();
        assert_eq!(active.editor_id, 65);
        assert_eq!(registry.active_editor_id(), None);
        let by_uri = registry.remove_by_uri("file:///workspace/64.rs").unwrap();
        assert_eq!(by_uri.editor_id, 64);
        assert!(
            registry
                .remove_by_uri("file:///workspace/missing.rs")
                .is_none()
        );
        assert!(registry.is_partial());

        let remaining = registry.clear();
        assert_eq!(remaining.len(), MAX_SYNCHRONIZED_DOCUMENTS - 2);
        assert!(registry.is_empty());
        assert_eq!(registry.retention_status(), RetentionStatus::Complete);
        assert_eq!(registry.active_editor_id(), None);

        registry.insert(document(100)).unwrap();
        assert_eq!(registry.active().unwrap().activity_ordinal(), 1);
    }

    #[test]
    fn explicit_partial_signals_are_sticky_until_each_service_store_is_cleared() {
        let mut registry = SynchronizedDocumentRegistry::new();
        registry.insert(document(1)).unwrap();
        assert_eq!(registry.retention_status(), RetentionStatus::Complete);

        registry.mark_partial();
        assert!(registry.is_partial());
        registry.remove_by_editor_id(1);
        registry.insert(document(2)).unwrap();
        assert!(registry.is_partial());

        registry.clear();
        assert_eq!(registry.retention_status(), RetentionStatus::Complete);

        let uri = "file:///workspace/parsed.rs";
        let mut cache = DiagnosticCache::new();
        cache.replace(uri.to_owned(), vec![diagnostic(uri, "retained")]);
        assert_eq!(cache.retention_status(), RetentionStatus::Complete);

        cache.mark_partial();
        assert!(cache.is_partial());
        cache.replace(uri.to_owned(), Vec::new());
        assert!(cache.is_partial());

        cache.clear();
        assert_eq!(cache.retention_status(), RetentionStatus::Complete);
    }

    #[test]
    fn document_activity_clock_rebases_without_changing_lru_order() {
        let mut registry = SynchronizedDocumentRegistry::new();
        registry.insert(document(1)).unwrap();
        registry.insert(document(2)).unwrap();
        registry.next_activity_ordinal = u64::MAX;

        assert!(registry.mark_active(1));

        assert_eq!(registry.get_by_editor_id(2).unwrap().activity_ordinal(), 2);
        assert_eq!(registry.get_by_editor_id(1).unwrap().activity_ordinal(), 3);
        assert_eq!(registry.next_activity_ordinal, 4);
    }

    #[test]
    fn diagnostic_cache_replaces_reads_marks_used_and_clears_empty_publication() {
        let mut cache = DiagnosticCache::new();
        let uri = "file:///workspace/main.rs";
        let report = cache.replace(uri.to_owned(), vec![diagnostic(uri, "first")]);

        assert_eq!(report.received, 1);
        assert_eq!(report.retained, 1);
        assert_eq!(report.dropped, 0);
        assert!(!report.truncated);
        assert!(!report.is_partial());
        assert_eq!(cache.get(uri).unwrap()[0].message, "first");
        assert!(cache.mark_used(uri));
        assert!(!cache.mark_used("file:///missing.rs"));

        let replacement = cache.replace(uri.to_owned(), vec![diagnostic(uri, "second")]);
        assert_eq!(replacement.retained, 1);
        assert_eq!(cache.diagnostic_count(), 1);
        assert_eq!(cache.get(uri).unwrap()[0].message, "second");

        let clear = cache.replace(uri.to_owned(), Vec::new());
        assert_eq!(clear.received, 0);
        assert_eq!(clear.retained, 0);
        assert!(cache.get(uri).is_none());
        assert!(cache.is_empty());
        assert_eq!(cache.retained_estimate_bytes(), 0);
    }

    #[test]
    fn diagnostic_cache_caps_each_uri_and_reports_truncation() {
        let mut cache = DiagnosticCache::new();
        let uri = "file:///workspace/many.rs";
        let diagnostics = (0..MAX_DIAGNOSTICS_PER_URI + 3)
            .map(|index| diagnostic(uri, index.to_string()))
            .collect();

        let report = cache.replace(uri.to_owned(), diagnostics);

        assert_eq!(report.received, MAX_DIAGNOSTICS_PER_URI + 3);
        assert_eq!(report.retained, MAX_DIAGNOSTICS_PER_URI);
        assert_eq!(report.dropped, 3);
        assert!(report.truncated);
        assert!(report.is_partial());
        assert_eq!(cache.get(uri).unwrap().len(), MAX_DIAGNOSTICS_PER_URI);
        assert!(cache.retained_estimate_bytes() <= MAX_DIAGNOSTIC_RETAINED_BYTES);
    }

    #[test]
    fn diagnostic_cache_rejects_cross_uri_entries_without_cloning() {
        let mut cache = DiagnosticCache::new();
        let uri = "file:///workspace/right.rs";
        let report = cache.replace(
            uri.to_owned(),
            vec![
                diagnostic("file:///workspace/wrong.rs", "wrong"),
                diagnostic(uri, "right"),
            ],
        );

        assert_eq!(report.received, 2);
        assert_eq!(report.retained, 1);
        assert_eq!(report.dropped, 1);
        assert!(report.truncated);
        assert_eq!(cache.get(uri).unwrap()[0].message, "right");
    }

    #[test]
    fn diagnostic_cache_evicts_oldest_bucket_at_uri_limit_and_honors_touch() {
        let mut cache = DiagnosticCache::new();
        for index in 0..MAX_DIAGNOSTIC_URI_BUCKETS {
            let uri = format!("file:///workspace/{index}.rs");
            cache.replace(uri.clone(), vec![diagnostic(&uri, "warning")]);
        }
        assert!(cache.mark_used("file:///workspace/0.rs"));

        let newest = "file:///workspace/new.rs";
        let report = cache.replace(newest.to_owned(), vec![diagnostic(newest, "new")]);

        assert_eq!(report.evicted_buckets, 1);
        assert_eq!(report.evicted_diagnostics, 1);
        assert!(report.is_partial());
        assert_eq!(cache.len(), MAX_DIAGNOSTIC_URI_BUCKETS);
        assert!(cache.get("file:///workspace/0.rs").is_some());
        assert!(cache.get("file:///workspace/1.rs").is_none());
        assert!(cache.get(newest).is_some());
    }

    #[test]
    fn diagnostic_cache_evicts_lru_buckets_to_hold_newest_within_byte_cap() {
        let mut cache = DiagnosticCache::new();
        let payload = "x".repeat(2 * 1024 * 1024);
        let mut last_report = DiagnosticReplaceReport::default();
        for index in 0..4 {
            let uri = format!("file:///workspace/large-{index}.rs");
            last_report = cache.replace(uri.clone(), vec![diagnostic(&uri, payload.clone())]);
        }

        assert_eq!(last_report.evicted_buckets, 1);
        assert_eq!(last_report.evicted_diagnostics, 1);
        assert!(last_report.is_partial());
        assert!(cache.get("file:///workspace/large-0.rs").is_none());
        assert!(cache.get("file:///workspace/large-3.rs").is_some());
        assert!(cache.retained_estimate_bytes() <= MAX_DIAGNOSTIC_RETAINED_BYTES);
    }

    #[test]
    fn diagnostic_cache_drops_single_value_larger_than_aggregate_budget() {
        let mut cache = DiagnosticCache::new();
        let uri = "file:///workspace/oversized.rs";
        let report = cache.replace(
            uri.to_owned(),
            vec![diagnostic(
                uri,
                "x".repeat(MAX_DIAGNOSTIC_RETAINED_BYTES + 1),
            )],
        );

        assert_eq!(report.received, 1);
        assert_eq!(report.retained, 0);
        assert_eq!(report.dropped, 1);
        assert!(report.truncated);
        assert!(report.is_partial());
        assert!(cache.is_empty());
        assert_eq!(cache.retained_estimate_bytes(), 0);
    }

    #[test]
    fn diagnostic_estimate_includes_nested_json_allocations() {
        let uri = "file:///workspace/raw.rs";
        let mut fields = BTreeMap::new();
        fields.insert(
            "payload".to_owned(),
            JsonValue::String("x".repeat(MAX_DIAGNOSTIC_RETAINED_BYTES)),
        );
        let mut value = diagnostic(uri, "small");
        value.raw = JsonValue::Object(fields);
        let mut cache = DiagnosticCache::new();

        let report = cache.replace(uri.to_owned(), vec![value]);

        assert_eq!(report.retained, 0);
        assert_eq!(report.dropped, 1);
        assert!(report.truncated);
    }

    #[test]
    fn diagnostic_cache_enforces_raw_json_byte_node_and_depth_ceilings() {
        let uri = "file:///workspace/raw-bounds.rs";
        let mut cache = DiagnosticCache::new();

        let exact_depth = diagnostic_with_raw(
            uri,
            nested_array(JsonValue::Null, MAX_DIAGNOSTIC_RAW_DEPTH.saturating_sub(1)),
        );
        assert_eq!(cache.replace(uri.to_owned(), vec![exact_depth]).retained, 1);

        let exact_nodes = diagnostic_with_raw(
            uri,
            JsonValue::Array(vec![
                JsonValue::Array(Vec::new());
                MAX_DIAGNOSTIC_RAW_NODES - 1
            ]),
        );
        assert_eq!(cache.replace(uri.to_owned(), vec![exact_nodes]).retained, 1);

        let exact_bytes = diagnostic_with_raw(
            uri,
            JsonValue::String("x".repeat(MAX_DIAGNOSTIC_RAW_BYTES - 2)),
        );
        assert_eq!(cache.replace(uri.to_owned(), vec![exact_bytes]).retained, 1);

        let oversized_values = [
            nested_array(JsonValue::Null, MAX_DIAGNOSTIC_RAW_DEPTH),
            JsonValue::Array(vec![JsonValue::Array(Vec::new()); MAX_DIAGNOSTIC_RAW_NODES]),
            JsonValue::String("x".repeat(MAX_DIAGNOSTIC_RAW_BYTES - 1)),
        ];
        for raw in oversized_values {
            cache.clear();
            let report = cache.replace(uri.to_owned(), vec![diagnostic_with_raw(uri, raw)]);
            assert_eq!(report.received, 1);
            assert_eq!(report.retained, 0);
            assert_eq!(report.dropped, 1);
            assert!(report.truncated);
            assert!(report.is_partial());
            assert!(cache.is_empty());
        }
    }

    #[test]
    fn repeated_depth_64_singleton_objects_are_omitted_at_aggregate_cap() {
        let uri = "file:///workspace/deep-omission.rs";
        let sample = diagnostic_with_raw(uri, depth_64_singleton_object());
        let per_diagnostic = diagnostic_heap_estimate(&sample, MAX_DIAGNOSTIC_RETAINED_BYTES)
            .unwrap()
            .saturating_add(mem::size_of::<Diagnostic>());
        assert!(
            per_diagnostic >= (MAX_DIAGNOSTIC_RAW_DEPTH - 1) * btree_object_allocation_estimate(1)
        );
        let count = MAX_DIAGNOSTIC_RETAINED_BYTES
            .checked_div(per_diagnostic)
            .unwrap()
            .saturating_add(2)
            .min(MAX_DIAGNOSTICS_PER_URI);
        let diagnostics = (0..count)
            .map(|_| diagnostic_with_raw(uri, depth_64_singleton_object()))
            .collect();
        let mut cache = DiagnosticCache::new();

        let report = cache.replace(uri.to_owned(), diagnostics);

        assert_eq!(report.received, count);
        assert!(report.retained > 0);
        assert!(report.retained < report.received);
        assert_eq!(report.dropped, report.received - report.retained);
        assert!(report.truncated);
        assert!(report.is_partial());
        assert!(cache.retained_estimate_bytes() <= MAX_DIAGNOSTIC_RETAINED_BYTES);
    }

    #[test]
    fn repeated_depth_64_singleton_objects_evict_old_bucket_for_newest() {
        let first_uri = "file:///workspace/deep-first.rs";
        let second_uri = "file:///workspace/deep-second.rs";
        let sample = diagnostic_with_raw(first_uri, depth_64_singleton_object());
        let per_diagnostic = diagnostic_heap_estimate(&sample, MAX_DIAGNOSTIC_RETAINED_BYTES)
            .unwrap()
            .saturating_add(mem::size_of::<Diagnostic>());
        let count = MAX_DIAGNOSTIC_RETAINED_BYTES
            .checked_div(per_diagnostic.saturating_mul(2))
            .unwrap()
            .saturating_add(2)
            .min(MAX_DIAGNOSTICS_PER_URI);
        let deep_diagnostics = |uri: &str| {
            (0..count)
                .map(|_| diagnostic_with_raw(uri, depth_64_singleton_object()))
                .collect()
        };
        let mut cache = DiagnosticCache::new();
        let first = cache.replace(first_uri.to_owned(), deep_diagnostics(first_uri));
        assert_eq!(first.retained, count);

        let second = cache.replace(second_uri.to_owned(), deep_diagnostics(second_uri));

        assert_eq!(second.retained, count);
        assert_eq!(second.evicted_buckets, 1);
        assert_eq!(second.evicted_diagnostics, count);
        assert!(second.is_partial());
        assert!(cache.get(first_uri).is_none());
        assert!(cache.get(second_uri).is_some());
        assert!(cache.retained_estimate_bytes() <= MAX_DIAGNOSTIC_RETAINED_BYTES);
    }

    #[test]
    fn diagnostic_number_estimate_has_conservative_allocation_multiplier() {
        let uri = "file:///workspace/number.rs";
        let mut value = diagnostic(uri, "number");
        value.raw = JsonValue::from(123_456_789_i64);
        let scalar_heap = value
            .uri
            .capacity()
            .saturating_add(value.message.capacity())
            .saturating_add(value.source.as_ref().unwrap().capacity());

        let estimate = diagnostic_heap_estimate(&value, MAX_DIAGNOSTIC_RETAINED_BYTES).unwrap();

        assert!(estimate >= scalar_heap + "123456789".len() * 2);
    }

    #[test]
    fn diagnostic_purge_preserves_partial_until_clear_resets_session() {
        let mut cache = DiagnosticCache::new();
        let uri = "file:///workspace/problem.rs";
        let diagnostics = (0..MAX_DIAGNOSTICS_PER_URI + 1)
            .map(|index| diagnostic(uri, index.to_string()))
            .collect();
        cache.replace(uri.to_owned(), diagnostics);
        assert!(cache.is_partial());

        assert!(cache.purge(uri));
        assert!(!cache.purge(uri));
        assert!(cache.is_empty());
        assert!(cache.is_partial());

        cache.clear();
        assert_eq!(cache.retention_status(), RetentionStatus::Complete);
        assert_eq!(cache.retained_estimate_bytes(), 0);
        assert_eq!(cache.diagnostic_count(), 0);
    }

    #[test]
    fn diagnostic_activity_clock_rebases_and_keeps_oldest_deterministic() {
        let mut cache = DiagnosticCache::new();
        for index in 0..2 {
            let uri = format!("file:///workspace/{index}.rs");
            cache.replace(uri.clone(), vec![diagnostic(&uri, "warning")]);
        }
        cache.next_activity_ordinal = u64::MAX;

        assert!(cache.mark_used("file:///workspace/0.rs"));

        let zero = cache
            .buckets
            .iter()
            .find(|bucket| bucket.uri.ends_with("/0.rs"))
            .unwrap();
        let one = cache
            .buckets
            .iter()
            .find(|bucket| bucket.uri.ends_with("/1.rs"))
            .unwrap();
        assert_eq!(one.activity_ordinal, 2);
        assert_eq!(zero.activity_ordinal, 3);
        assert_eq!(cache.next_activity_ordinal, 4);
    }

    impl SynchronizedDocumentRegistry {
        fn document_admission_for_test(&mut self, editor_id: u64) -> DocumentInsertReport {
            self.insert(document(editor_id)).unwrap()
        }
    }
}
