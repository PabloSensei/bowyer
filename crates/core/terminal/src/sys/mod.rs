//! What a session needs from Windows beyond the pseudoconsole: a job that ends the tree of
//! the shell, the walk that pulls escaped programs into it, and the process table that walk
//! reads. Trimmed copies of the `platform` crate of Arro, private to this crate.

mod job;
mod process;
mod process_stats;
mod process_tree;

pub(crate) use job::Job;
pub(crate) use process::allow_ctrl_c_in_children;
#[cfg(test)]
pub(crate) use process_stats::process_table;
pub(crate) use process_tree::ProcessTree;
