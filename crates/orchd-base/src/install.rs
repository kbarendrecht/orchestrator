//! How this build got onto the machine.
//!
//! One question, asked of the running executable: which packaging put it here.
//! The answer decides whether the app can upgrade itself and what the update bar
//! is allowed to tell you to run — a `.deb` wants apt and a password, a cask wants
//! `brew`, and a file somebody downloaded wants neither.
//!
//! **Why this exists at all:** the bar used to say "Run mise up" to every install
//! mise did not make, which is wrong advice for the five that are not mise's — and
//! there was nothing in the snapshot that could have said anything better, because
//! `UpdateInfo.tool` collapses all of them into one `None`.
//!
//! **mise is deliberately not answered here.** Naming a mise tool means running
//! `mise ls --json` and matching install paths, which is a subprocess and lives in
//! `orchd`'s `update` module beside the rest of the upgrade machinery. This module
//! is the part that can be a path predicate, and it is one so it can be tested
//! without a filesystem: [`classify`] takes every fact it needs as an argument and
//! [`Install::of_running`] is the thin layer that goes and gets them.

use std::path::{Path, PathBuf};

/// Which packaging installed the running binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Install {
    /// A Homebrew cask: `brew upgrade --cask orchestrator` replaces it.
    Homebrew,
    /// The `.deb`, from the apt repository or by hand. Upgrading wants root.
    Apt,
    /// An AppImage: one file somebody downloaded and made executable.
    AppImage,
    /// A `.dmg` dragged to Applications, with no package manager behind it.
    MacBundle,
    /// A release tarball, or a `mise`/`ubi` install this could not name. Neither
    /// carries an uninstaller, so neither can be upgraded in place.
    Tarball,
    /// A `cargo build` in a checkout. Never upgraded; `git pull` is the upgrade.
    Checkout,
}

/// The marker a bundle this app wrote carries, naming the install it was copied
/// from.
///
/// **A macOS bundle is not proof of an installer.** Since #24 a mise or tarball
/// install runs from a *copy* inside a bundle this app wrote itself, so
/// `/Contents/MacOS/` is where an ordinary install starts rather than evidence
/// that Homebrew or a `.dmg` put it there. The marker is what tells the two apart,
/// and the path inside it is the install the copies came from — so the honest
/// answer for a wrapper is whatever its *source* is, which is why [`classify`]
/// starts again on it.
///
/// `desktop/src/launcher.rs` walks the same three components for its own question
/// ("did somebody else write my launcher entry?"). Two short walks rather than a
/// shared one, because the crate line runs between them and the questions are not
/// the same.
const SOURCE_MARKER: &str = "Contents/Resources/source";

/// Where Homebrew records a cask, under each prefix it might be installed at.
///
/// A cask with an `app` stanza *moves* `Orchestrator.app` to `/Applications`, so
/// the bundle itself looks exactly like a `.dmg` drag — what separates them is
/// this directory, which only Homebrew writes.
const CASKROOM: &str = "Caskroom/orchestrator";

/// The prefixes Homebrew is installed at: Apple Silicon, Intel, and whatever
/// `$HOMEBREW_PREFIX` says when somebody moved it.
fn caskroom_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(prefix) = std::env::var_os("HOMEBREW_PREFIX") {
        v.push(PathBuf::from(prefix).join(CASKROOM));
    }
    v.push(PathBuf::from("/opt/homebrew").join(CASKROOM));
    v.push(PathBuf::from("/usr/local").join(CASKROOM));
    v
}

impl Install {
    /// What installed the process that is running.
    ///
    /// Every fact [`classify`] needs is gathered here — the environment, the
    /// bundle marker, the Caskroom probe — so the decision itself stays pure.
    /// Unknowable answers fall to [`Install::Tarball`], which offers nothing and
    /// therefore cannot offer anything wrong.
    pub fn of_running() -> Self {
        let Ok(exe) = std::env::current_exe() else {
            return Install::Tarball;
        };
        let appimage = std::env::var_os("APPIMAGE").is_some();
        let source = bundle_source(&exe);
        let caskroom = caskroom_candidates().iter().any(|p| p.is_dir());
        classify(&exe, appimage, source.as_deref(), caskroom)
    }
}

impl Install {
    /// Whether this install puts a launcher entry of its own on the machine: the
    /// cask's and the `.dmg`'s bundle in `/Applications`, the `.deb`'s file in
    /// `/usr/share/applications`.
    ///
    /// **An AppImage is not one**, although it is a package. It is a file somebody
    /// runs from wherever they put it, so the entry a mise install wrote may be the
    /// only way anybody starts it from a menu.
    pub fn ships_its_own_entry(self) -> bool {
        matches!(self, Install::Homebrew | Install::MacBundle | Install::Apt)
    }

    /// How a sentence names this install.
    pub fn name(self) -> &'static str {
        match self {
            Install::Homebrew => "Homebrew",
            Install::Apt => "the apt package",
            Install::MacBundle => "the .dmg in /Applications",
            Install::AppImage => "an AppImage",
            Install::Tarball => "a mise or tarball install",
            Install::Checkout => "a build in a checkout",
        }
    }
}

/// Where a cask or a dragged `.dmg` puts the app. A cask can be told another
/// `appdir`; the Caskroom still answers for it, so this is only the `.dmg` probe.
const SYSTEM_BUNDLE: &str = "/Applications/Orchestrator.app";
/// Where the `.deb` puts the app, per its `files` map in `desktop/tauri.conf.json`.
const DEB_EXE: &str = "/usr/bin/orchestrator-desktop";

/// The packaged installs on this machine, whichever one is running.
///
/// **Why this is asked at all:** a mise install writes a launcher entry with the
/// packages' id, and switching to a package leaves it behind. On Linux the user's
/// `.desktop` file outranks the `.deb`'s, and on macOS LaunchServices may open
/// either bundle, so somebody who switched kept launching the old build and was
/// told to upgrade it. An AppImage cannot be found this way: it is a file
/// anywhere.
pub fn packages_present() -> Vec<Install> {
    let caskroom = caskroom_candidates().iter().any(|p| p.is_dir());
    let bundle = Path::new(SYSTEM_BUNDLE);
    let dragged = bundle.join("Contents/MacOS/orchestrator-desktop").is_file()
        && !bundle.join(SOURCE_MARKER).is_file();
    let deb = Path::new(DEB_EXE).is_file();
    packages(caskroom, dragged, deb)
}

/// The decision behind [`packages_present`], with the facts handed in.
///
/// A bundle in `/Applications` is the cask's when a Caskroom entry exists, the
/// rule [`classify`] uses too, so one install is never counted as two.
fn packages(caskroom: bool, dragged: bool, deb: bool) -> Vec<Install> {
    let mut out = Vec::new();
    if caskroom {
        out.push(Install::Homebrew);
    } else if dragged {
        out.push(Install::MacBundle);
    }
    if deb {
        out.push(Install::Apt);
    }
    out
}

/// The install a bundle records as its source, when this app wrote that bundle.
fn bundle_source(exe: &Path) -> Option<PathBuf> {
    let bundle = exe.parent()?.parent()?.parent()?;
    let text = std::fs::read_to_string(bundle.join(SOURCE_MARKER)).ok()?;
    let first = text.lines().next()?.trim().to_string();
    (!first.is_empty()).then(|| PathBuf::from(first))
}

/// The decision, with every fact handed in.
///
/// Order matters and each rule is exact for something this project ships:
///
/// 1. `APPIMAGE` is set by the AppImage runtime and by nothing else.
/// 2. A bundle this app wrote is a wrapper around another install, so the answer
///    is that install's — see [`SOURCE_MARKER`].
/// 3. A `target/` path is a build, whatever else is true of it.
/// 4. `/usr/bin` is the `.deb`'s `files` map in `desktop/tauri.conf.json` and
///    nothing else here writes there.
/// 5. A bundle with a Caskroom entry beside it is Homebrew's; without one it is a
///    `.dmg` that was dragged.
pub fn classify(
    exe: &Path,
    appimage: bool,
    bundle_source: Option<&Path>,
    caskroom: bool,
) -> Install {
    if appimage {
        return Install::AppImage;
    }
    if let Some(source) = bundle_source {
        // No marker on the source: a wrapper around a wrapper is not a thing this
        // writes, and recursing on one would be a loop with a filesystem in it.
        return classify(source, false, None, caskroom);
    }
    let path = exe.to_string_lossy();
    if path.contains("/target/debug/") || path.contains("/target/release/") {
        return Install::Checkout;
    }
    if path.starts_with("/usr/bin/") {
        return Install::Apt;
    }
    if path.contains("/Contents/MacOS/") {
        return if caskroom {
            Install::Homebrew
        } else {
            Install::MacBundle
        };
    }
    Install::Tarball
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_deb_is_the_only_thing_that_installs_into_usr_bin() {
        let exe = Path::new("/usr/bin/orchestrator-desktop");
        assert_eq!(classify(exe, false, None, false), Install::Apt);
        // The daemon is the same install, and it is the process that asks.
        assert_eq!(
            classify(Path::new("/usr/bin/orchd"), false, None, false),
            Install::Apt
        );
    }

    /// The pair this whole module exists for: the same path is a cask or a
    /// hand-dragged `.dmg`, and only the Caskroom beside it says which.
    #[test]
    fn a_caskroom_entry_is_what_separates_brew_from_a_dragged_dmg() {
        let exe = Path::new("/Applications/Orchestrator.app/Contents/MacOS/orchestrator-desktop");
        assert_eq!(classify(exe, false, None, true), Install::Homebrew);
        assert_eq!(classify(exe, false, None, false), Install::MacBundle);
    }

    /// An AppImage mounts itself somewhere under `/tmp`, so its path says nothing;
    /// the variable its runtime exports is the whole answer.
    #[test]
    fn an_appimage_is_known_by_its_variable_not_its_path() {
        let exe = Path::new("/tmp/.mount_Orches/usr/bin/orchestrator-desktop");
        assert_eq!(classify(exe, true, None, false), Install::AppImage);
        // Without it that mount path reads as `/usr/bin`'s sibling and would
        // otherwise be taken for the deb — the reason this rule is first.
        assert_ne!(classify(exe, false, None, false), Install::Apt);
    }

    /// The trap #24 left behind: a mise install runs from a bundle this app wrote,
    /// so the bundle must not be read as an installer's.
    #[test]
    fn a_bundle_this_app_wrote_answers_for_the_install_behind_it() {
        let wrapper =
            Path::new("/home/k/Applications/Orchestrator.app/Contents/MacOS/orchestrator-desktop");
        let source =
            Path::new("/home/k/.local/share/mise/installs/x/2026.9.23/orchestrator-desktop");
        // A Caskroom on the machine is not this install's, and the source decides.
        assert_eq!(
            classify(wrapper, false, Some(source), true),
            Install::Tarball
        );
        // And a build tree behind the wrapper is still a build tree.
        assert_eq!(
            classify(
                wrapper,
                false,
                Some(Path::new(
                    "/home/k/dev/orchestrator/target/release/orchestrator-desktop"
                )),
                false
            ),
            Install::Checkout
        );
    }

    #[test]
    fn a_build_output_is_never_an_install() {
        for p in [
            "/home/k/dev/orchestrator/target/debug/orchestrator-desktop",
            "/home/k/dev/orchestrator/target/release/orchd",
        ] {
            assert_eq!(
                classify(Path::new(p), false, None, false),
                Install::Checkout
            );
        }
    }

    /// A cask and the bundle it moved into `/Applications` are one install, not
    /// a cask and a `.dmg` beside it.
    #[test]
    fn a_cask_is_counted_once() {
        assert_eq!(packages(true, true, false), vec![Install::Homebrew]);
        assert_eq!(packages(false, true, false), vec![Install::MacBundle]);
        assert_eq!(packages(false, false, true), vec![Install::Apt]);
        assert_eq!(packages(false, false, false), vec![]);
    }

    /// Only the three with an entry of their own may make a mise install give
    /// its entry up, and an AppImage is not one of them.
    #[test]
    fn an_appimage_ships_no_entry_of_its_own() {
        assert!(Install::Homebrew.ships_its_own_entry());
        assert!(Install::MacBundle.ships_its_own_entry());
        assert!(Install::Apt.ships_its_own_entry());
        assert!(!Install::AppImage.ships_its_own_entry());
        assert!(!Install::Tarball.ships_its_own_entry());
        assert!(!Install::Checkout.ships_its_own_entry());
    }

    /// Everything unrecognised lands here, and that is the point of it: `Tarball`
    /// offers no command, so an install this does not understand is told nothing
    /// rather than told something false.
    #[test]
    fn anything_else_is_a_tarball() {
        for p in [
            "/home/k/.local/bin/orchestrator-desktop",
            "/opt/orchestrator/orchestrator-desktop",
        ] {
            assert_eq!(classify(Path::new(p), false, None, true), Install::Tarball);
        }
    }
}
