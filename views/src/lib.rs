//! `ph-reactor-views` — the CQRS read-model layer over [`ph_reactor`]'s
//! document store.
//!
//! The core store is the source of truth (the event-sourced action log +
//! the reduced field map). This crate sits *on top* of it as a consumer
//! layer: it subscribes to the store's doc-change feed and maintains
//! derived, queryable indexes without touching the store's write path. That
//! mirrors the TypeScript reactor's split between the store/queue (core)
//! and the read models / processors (consumer).
//!
//! The built-in read models:
//! - [`DocumentView`] — a snapshot index: every doc by name/id, indexed by
//!   model, with field-equality queries. The main query surface.
//! - [`RelationshipIndex`] — a graph of the relationship edges declared by
//!   the model layer, with outgoing/incoming/path/ancestry queries.
//!
//! [`ReadModelCoordinator`] drives them: an initial snapshot from the store,
//! then incremental updates off the doc-change feed. [`ProcessorManager`] is
//! the event-driven processing layer (a job queue over the same feed).
//! Custom read models and processors plug in through the [`ReadModel`] and
//! [`Processor`] traits.
//!
//! "Subgraphs" in the TypeScript reactor are Apollo GraphQL subgraphs (the
//! switchboard presentation layer) — a different concern entirely — so they
//! are *not* part of this crate.

pub mod document_view;
pub mod processor;
pub mod query;
pub mod read_model;
pub mod relationship;

pub use document_view::{DocFilter, DocumentView};
pub use processor::{DocCounter, Processor, ProcessorManager};
pub use query::QueryService;
pub use read_model::{DocSnapshot, ReadModel, ReadModelCoordinator};
pub use relationship::RelationshipIndex;
