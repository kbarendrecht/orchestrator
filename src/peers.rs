//! The other repositories this window is showing, and how the page reaches them.
//!
//! **One checkout per daemon, several daemons per window.** Working two
//! repositories at once is a terminal with two tabs: each tab is a whole shell
//! that knows nothing about the other, and that is exactly what makes it stable.
//! So a second repository is a second `orchd` — its own process, its own
//! `ORCHD_CONFIG_DIR`, its own port, its own `config.json`, its own instance
//! lock — and the SPA holds one connection per repository and composes them into
//! one rail. Nothing in a daemon is repo-aware, and none of `MAIN`,
//! `claim_main`, `release_main`, the swap or either PR flow had to learn a repo
//! qualifier.
//!
//! **This reverses a decision recorded in `TODO.md`**, which ruled a child
//! process out because "a daemon in a *child process* has no Tauri handle, so
//! minimise, maximise, close and the eight resize edges all stop working". That
//! is true of the daemon **serving the page** and only of that one: the handle is
//! read in exactly one place ([`crate::api::dispatch_window`]) and already
//! degrades with "no native window attached". A secondary daemon serves no page,
//! so the titlebar keeps talking to the primary, which does have the handle. The
//! isolation TODO.md listed as the in-process design's accepted cost — "a crash,
//! an OOM or a self-upgrade restart takes every repo's live sessions rather than
//! one repo's" — we keep instead of paying.
//!
//! Deliberately **not** part of the snapshot. A snapshot is one daemon's state,
//! and a daemon must stay unaware that it has siblings; this is process-level
//! wiring the shell knows and hands to the page, the same way [`crate::window`]'s
//! control is.

use serde::Serialize;
use std::path::{Path, PathBuf};

/// A repository the page should open a second connection to.
///
/// `token` is another daemon's app token, handed to a page this daemon serves.
/// That is a real widening and it is bounded by the thing that already bounds
/// the primary's own token: `GET /` is same-origin-only in practice because both
/// daemons refuse any request whose Host or Origin is not their own loopback
/// port, and every one of these ports is on this machine, started by this
/// process, for this user. A page that can already act as you on repo A is not
/// meaningfully safer for being unable to act on repo B.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Peer {
    /// Stable across restarts and independent of order, because the colour and
    /// the SPA's remembered rail order both key on it. The checkout path, which
    /// is the one thing about a repository that cannot be two values at once.
    pub id: String,
    /// What to call it in the rail: the checkout's own directory name.
    pub name: String,
    pub path: PathBuf,
    pub port: u16,
    pub token: String,
    /// Which of [`PALETTE`] this repository wears, as a CSS colour.
    ///
    /// Not `pub` for decoration: the shell mints a peer when its daemon answers
    /// and sets this once *every* repository has, because [`colours_for`] can only
    /// answer for the whole set.
    pub colour: &'static str,
}

impl Peer {
    /// `port` and `token` come from the daemon once it is up, and `colour` from
    /// [`colours_for`] over the whole set — a colour is only meaningful beside the
    /// others, so it cannot be decided here.
    pub fn new(path: PathBuf, port: u16, token: String, colour: &'static str) -> Peer {
        Peer {
            id: id_for(&path),
            name: name_for(&path),
            colour,
            path,
            port,
            token,
        }
    }
}

/// A repository's identity, for the colour and for the SPA's remembered order.
///
/// The path rather than the directory name: two checkouts of the same repository
/// (a fork beside its parent, `orchestrator` beside `orchestrator-old`) are two
/// rows in the rail and must not collapse onto one colour.
pub fn id_for(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// What the rail calls it. Falls back to the whole path rather than to nothing:
/// a nameless row is worse than a long one, and `/` has no file name.
pub fn name_for(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Hand-picked rather than a hue rotation off the hash.
///
/// The ask was a colour you can *tell apart* at a glance, and an arbitrary hue
/// gives you muddy olive as readily as a clear blue, plus neighbouring hues for
/// two repositories whose hashes happen to land close. These eight are distinct
/// from each other and from the state colours the rail already uses for a
/// session's dot, which is the palette they have to coexist with rather than
/// match.
/// Hues, roughly: 0, 33, 76, 148, 180, 207, 245, 310. Spread on purpose — an
/// earlier list paired amber `#D9A05B` with brass `#B8A05B`, nine degrees apart,
/// and two repositories drew them side by side on the first real run. Telling
/// them apart at a glance is the entire requirement, so a near-miss here is a
/// bug, not a shade.
pub const PALETTE: [&str; 8] = [
    "#D97E7E", // clay
    "#D9A05B", // amber
    "#9DC25B", // lime
    "#6FB98F", // green
    "#5FB3B3", // teal
    "#5B9DD9", // blue
    "#8C86D9", // periwinkle
    "#C77DBB", // orchid
];

/// **By identity, never by position.** The rail's order is draggable and lives in
/// the browser, so a colour derived from position would swap two repositories'
/// colours the moment you reordered them — and the colour is the thing you were
/// using to tell them apart.
///
/// FNV-1a because it is four lines and stable across processes and platforms;
/// `DefaultHasher` promises neither, and this value is written into a page and
/// compared against a remembered one.
fn slot_for(path: &Path) -> usize {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in id_for(path).as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    (hash % PALETTE.len() as u64) as usize
}

/// One colour per checkout, and **never the same one twice**.
///
/// A hash alone is stable but not distinct: eight colours and three
/// repositories collide about a third of the time, and two repositories wearing
/// one colour defeats the entire point of having a colour. So the hash picks a
/// preferred slot and a collision probes forward for the next free one — stable
/// while the set of repositories does not change, and distinct always.
///
/// Assigned over the **whole** set rather than per repository, which is why it
/// lives here and is called by the shell: nothing that knows only one checkout
/// can promise the other one a different colour. Beyond [`PALETTE`]'s length it
/// wraps and repeats, because eight open repositories is not a thing to refuse.
pub fn colours_for(paths: &[PathBuf]) -> Vec<&'static str> {
    let mut taken = [false; PALETTE.len()];
    paths
        .iter()
        .map(|path| {
            let want = slot_for(path);
            let slot = (0..PALETTE.len())
                .map(|step| (want + step) % PALETTE.len())
                .find(|s| !taken[*s])
                .unwrap_or(want);
            taken[slot] = true;
            PALETTE[slot]
        })
        .collect()
}

/// What only the shell can do about repositories.
///
/// Beside [`crate::window::WindowControl`] and for the identical reason: a daemon
/// cannot open a native dialog and cannot start a sibling process, so both are
/// trait objects the desktop attaches once it exists. Absent in a browser tab and
/// in a headless daemon, where the routes that use it refuse rather than pretend.
///
/// **The shell owns the bookkeeping, not the daemon.** `add` writes the config,
/// launches the daemon, reassigns every colour over the new set and publishes the
/// list through [`crate::state::AppState::attach_checkouts`] — all of which need
/// the child handles the shell holds. A daemon that did half of it would be a
/// second place that has to agree about which repositories exist.
pub trait CheckoutControl: Send + Sync {
    /// Native folder dialog, blocking until answered. `None` on cancel.
    ///
    /// Runs on a request thread, never the UI thread — the dialog is marshalled by
    /// the toolkit, exactly as `firstrun::BootstrapHost::pick` does it.
    fn pick(&self) -> Option<PathBuf>;

    /// Open a repository beside the others: config, daemon, colours, list.
    fn add(&self, path: PathBuf) -> anyhow::Result<()>;

    /// Close one: stop its daemon, drop it from the config, republish the list.
    ///
    /// Its live sessions go with it — they are that daemon's children — which is
    /// why the caller confirms first.
    fn remove(&self, path: PathBuf) -> anyhow::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repository_keeps_its_colour_across_processes() {
        let set = vec![
            PathBuf::from("/home/me/development/platform"),
            PathBuf::from("/home/me/development/orchestrator"),
        ];
        // Stable: the same set is the same answer every time it is asked, which is
        // what lets the SPA remember an order without remembering a colour.
        assert_eq!(colours_for(&set), colours_for(&set));
        for c in colours_for(&set) {
            assert!(PALETTE.contains(&c));
        }
    }

    /// Two repositories in one colour defeats the whole reason for having one, and
    /// eight slots collide often enough that the hash alone will not do: this is
    /// the case that made `colours_for` take the set rather than one path.
    #[test]
    fn no_two_repositories_share_a_colour() {
        // Every checkout that hashes to a slot already taken has to move, so drive
        // it with enough paths to force that rather than hoping.
        let set: Vec<PathBuf> =
            (0..PALETTE.len()).map(|i| PathBuf::from(format!("/repos/r{i}"))).collect();
        let colours = colours_for(&set);
        let mut seen = colours.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), colours.len(), "a colour was handed out twice: {colours:?}");
    }

    /// More repositories than colours is not something to refuse, so it wraps.
    #[test]
    fn more_repositories_than_colours_still_all_get_one() {
        let set: Vec<PathBuf> =
            (0..PALETTE.len() + 3).map(|i| PathBuf::from(format!("/repos/r{i}"))).collect();
        assert_eq!(colours_for(&set).len(), set.len());
    }

    /// The order is draggable and remembered in the browser, so a colour that
    /// followed position would swap the two things you were telling apart.
    #[test]
    fn a_colour_does_not_follow_position() {
        let a = PathBuf::from("/x/alpha");
        let b = PathBuf::from("/x/beta");
        let forward = colours_for(&[a.clone(), b.clone()]);
        let backward = colours_for(&[b, a]);
        assert_eq!(forward[0], backward[1]);
        assert_eq!(forward[1], backward[0]);
    }

    /// Two checkouts of one repository are two rows, so they must not share an
    /// identity — the colour and the remembered order both key on it.
    #[test]
    fn two_checkouts_of_one_repository_are_two_identities() {
        let fork = PathBuf::from("/home/me/development/orchestrator");
        let parent = PathBuf::from("/home/me/other/orchestrator");
        assert_eq!(name_for(&fork), name_for(&parent), "same directory name");
        assert_ne!(id_for(&fork), id_for(&parent), "different identity");
    }

    #[test]
    fn a_root_path_still_gets_a_name() {
        assert!(!name_for(Path::new("/")).is_empty());
    }
}
