//! In-process e2e harness that stands up a real `MvpAgent` over ACP pipes.
//! Lives with the run loop (feature `test-support`) so it can construct the agent.

pub mod e2e;

pub use crate::agent::subagent::isolated_spawn_e2e::{
    IsolatedSubagentSpawn, spawn_isolated_subagent_for_e2e,
};
