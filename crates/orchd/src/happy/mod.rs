//! Publishing a checkout's sessions to a phone, over Happy's relay.
//!
//! **Happy is a client this daemon speaks to, not a thing it wraps.** The obvious
//! shortcut — spawn `happy claude` instead of `claude` and let their CLI publish —
//! cannot work: both programs pass `--settings`, Claude Code takes the *last* one
//! and merges nothing (measured, 2.1.270), and Happy appends its own after the
//! caller's args. orchd would lose every hook, which is the daemon. The second
//! half of the same collision is session identity: Happy picks `--session-id` and
//! `--resume` from its own store, and orchd's id *is* Claude's id.
//!
//! So orchd is a third client on a protocol built for several
//! (`@slopus/happy-wire`, MIT). The relay stores blobs it cannot read; what makes
//! that true is [`crypto`], and the tests there are vectors from Happy's own
//! implementation rather than round trips of this one.

pub mod crypto;
