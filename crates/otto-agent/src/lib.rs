//! Agent definitions + built-ins + subagent-permission derivation — a Rust
//! port of opencode `agent/agent.ts` + `agent/subagent-permissions.ts`.
//!
//! * [`agent`] — the [`AgentInfo`] value type (port of the `Info` schema,
//!   agent.ts:35-56) plus [`AgentMode`] and [`ModelRef`].
//! * [`builtins`] — the built-in agents build/plan/general/explore and the
//!   hidden internals compaction/title/summary (agent.ts:140-265).
//! * [`config`] — [`resolve_agents`], the deep-merge of `cfg.agent` over the
//!   built-ins (agent.ts:267-294), plus [`config::get`] / [`config::list`].
//! * [`subagent`] — [`derive_subagent_permission`], the child-session
//!   narrowing (subagent-permissions.ts).

#![forbid(unsafe_code)]

pub mod agent;
pub mod builtins;
pub mod config;
pub mod subagent;

pub use agent::{AgentInfo, AgentMode, ModelRef};
pub use builtins::{builtins, defaults};
pub use config::resolve_agents;
pub use subagent::derive_subagent_permission;
