#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]
#![allow(missing_docs)]

//! `concerto-api-server` — axum-backed HTTP API with OpenAPI generation.
//!
//! Provides versioned routes with configurable API-key middleware for session
//! management, task submission, SSE streaming, spend querying, health, and
//! optional OpenAPI documentation.

pub mod auth;
pub mod routes;
pub mod sse;
pub mod state;
