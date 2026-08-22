//! Shared frontend services — builder APIs for initialising the runtime
//! components needed by both CLI and Desktop frontends.
//!
//! The two main builders are:
//!
//! * [`ServicesBuilder`] — constructs a [`SharedServices`] bundle from
//!   common options, wrapping the shared mutable state fields
//!   (`Arc<Mutex<Option<...>>>`) and optionally calling
//!   [`init_memory_system`] at build time.
//!
//! * [`RequestBuilder`] — constructs an [`AgentRunRequest`] from UI state
//!   fields, reducing the ~15‑line manual construction to a single chain.
//!
//! # Quick start (CLI)
//! ```ignore
//! let services = ServicesBuilder::new(bus, config, approval_sink, &project_dir)
//!     .with_session_manager(session_manager)
//!     .build();
//!
//! let request = RequestBuilder::new(input, project_dir, cancel_token)
//!     .with_session(session_id, conversation_history)
//!     .with_memory_enabled(!fast && config.memory.enabled)
//!     .build();
//!
//! let output = run_shared_agent(request, services).await?;
//! ```
//!
//! # Quick start (Desktop)
//! ```ignore
//! let services = ServicesBuilder::new(bus, config, approval_sink, &project_dir)
//!     .with_vfs(vfs)
//!     .with_session_manager(handler.manager())
//!     .build();
//!
//! let request = RequestBuilder::new(input, project_dir, cancel_token)
//!     .with_provider_model(provider_id, model)
//!     .with_session(session_id, conversation_history)
//!     .with_resume_checkpoint(resume_checkpoint)
//!     .build();
//!
//! let output = run_shared_agent(request, services).await?;
//! ```

mod builder;
mod request;
mod summarizer;

pub use builder::ServicesBuilder;
pub use request::RequestBuilder;
pub use summarizer::ProviderSummarizer;
