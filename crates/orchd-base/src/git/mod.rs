//! Every git call the daemon makes, in one place and nowhere else.
//!
//! **A directory rather than a file, and the seams were already drawn.** This was
//! 4,457 lines — 47% of `orchd-base` — partitioned by banner comments into the six
//! files it is now: the runner, status, refs, unpushed work, worktrees and the
//! branch moves, banked work, and the review flow's writes. Nothing moved between
//! them; the banners became file names.
//!
//! Two things the split has to preserve, and both are about the module *graph*
//! rather than about git. `tools/rust-modules.mjs` keys a module on its first path
//! component, so `git/` is still one module `git` and the graph is unchanged — a
//! cycle cannot hide in here. And `crate::git::…` still resolves for all 200-odd
//! call sites, because every item is re-exported below at the path it always had.
//!
//! Items the submodules share are `pub(super)`, which is "inside `git`" and no
//! wider. What was `pub` before is still `pub`.
//!
//! **The tests did not split with the code, and that is measured rather than
//! lazy.** The expectation was that each section's tests would move with it for
//! free. They do not: they are grouped by *fixture*, not by section, and the three
//! fixtures cross every seam — `scratch_repo` is shared by twelve tests spanning
//! refs and worktrees, `amend_repo` by fourteen spanning the review writes and the
//! blame helpers, `bank_fixture` by four. Splitting them needs a shared fixtures
//! module and a hand assignment of 62 tests, and buys a reader nothing the file's
//! own order does not: what is navigated while changing behaviour is the shipped
//! code, which is what moved.

mod bank;
mod exec;
mod refs;
mod review;
mod status;
mod unpushed;
mod worktree;

pub use bank::*;
pub use exec::*;
pub use refs::*;
pub use review::*;
pub use status::*;
pub use unpushed::*;
pub use worktree::*;

#[cfg(test)]
mod tests;
