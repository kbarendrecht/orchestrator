use super::*;
use std::path::Path;
// The amend-target cases moved with nothing but their names: the fixture they

/// **Every one of these closes a door that leads to a hang**, and a hang is
/// what makes this class of bug unreproducible: `Command::output()` nulls
/// stdin, but git and ssh ask on `/dev/tty`, so a fetch against an https remote
/// with no credential helper — or an ssh remote whose host key is unknown —
/// waits forever for an answer nobody can give. The desktop app escapes it only
/// by having no tty; `orchd` from a terminal does not, and boot fetches before
/// the window opens.
#[test]
fn network_git_cannot_stop_to_ask_a_question() {
    let env = net_env(Path::new("/"));
    let get = |k: &str| {
        env.iter()
            .find(|(name, _)| name == k)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("{k} is not set, so git can prompt again"))
    };

    // git's own prompt.
    assert_eq!(get("GIT_TERMINAL_PROMPT"), "0");

    // ssh's passphrase prompt, and its "unknown host, continue?" prompt.
    let ssh = get("GIT_SSH_COMMAND");
    assert!(ssh.contains("BatchMode=yes"), "ssh can still ask: {ssh}");
    assert!(
        ssh.contains("StrictHostKeyChecking=accept-new"),
        "ssh can still ask about a new host: {ssh}"
    );
    // `accept-new`, not `no`: a *changed* host key must still refuse, because
    // that is the case worth refusing.
    assert!(
        !ssh.contains("StrictHostKeyChecking=no"),
        "a changed host key must still be refused: {ssh}"
    );

    // And the graphical helpers, which would be spawned in place of the
    // terminal prompt and hang exactly as well.
    assert_eq!(get("GIT_ASKPASS"), "");
    assert_eq!(get("SSH_ASKPASS"), "");
}

/// The repo's own `core.sshCommand` is the base the options are appended to,
/// not replaced by. A fixed `ssh` threw away the identity a multi-key setup
/// depends on, and every daemon-side fetch then failed on `publickey`.
#[test]
fn network_git_keeps_the_configured_ssh_command() {
    // The inherited variable outranks the config on purpose, and it is
    // process-global, so on a machine that exports it this test has nothing
    // it can safely assert.
    if std::env::var_os("GIT_SSH_COMMAND").is_some() {
        return;
    }
    let dir = crate::testutil::scratch("sshcmd");
    assert!(run(&dir, &["init", "-q"]).unwrap().status.success());
    assert!(
        run(&dir, &["config", "core.sshCommand", "ssh -i /tmp/id_work"])
            .unwrap()
            .status
            .success()
    );

    // The uncached reader, because `net_env`'s is a process-wide `OnceLock` and
    // this asks about two different configs in a row.
    assert_eq!(
        read_ssh_command(&dir).as_deref(),
        Some("ssh -i /tmp/id_work")
    );

    // And a repo that says nothing gets plain `ssh`.
    assert!(run(&dir, &["config", "--unset", "core.sshCommand"])
        .unwrap()
        .status
        .success());
    assert_eq!(read_ssh_command(&dir), None);
    let ssh = net_env(&dir)
        .into_iter()
        .find(|(k, _)| k == "GIT_SSH_COMMAND")
        .map(|(_, v)| v)
        .unwrap();
    assert!(
        ssh.ends_with(" -oBatchMode=yes -oStrictHostKeyChecking=accept-new"),
        "the two options go on the end whatever the base is: {ssh}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The config is read once, not once per network call. It sat in front of the
/// boot fetch, every poller tick and every push, each one an extra `git` exec
/// for an answer that cannot change while the process runs.
#[test]
fn the_configured_ssh_command_is_read_once() {
    let dir = crate::testutil::scratch("sshcmd-cached");
    assert!(run(&dir, &["init", "-q"]).unwrap().status.success());

    let first = configured_ssh_command(&dir).clone();
    // Change the repo's answer under it. A second read would see this; the
    // cache must not.
    assert!(
        run(&dir, &["config", "core.sshCommand", "ssh -i /tmp/changed"])
            .unwrap()
            .status
            .success()
    );
    assert_eq!(
        configured_ssh_command(&dir).clone(),
        first,
        "the cached answer was re-read from the repo"
    );
    assert_eq!(
        read_ssh_command(&dir).as_deref(),
        Some("ssh -i /tmp/changed"),
        "and the uncached reader still sees the real config, so the cache is what differs"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A sha is not always seven bytes of ASCII, because it is not always a sha.**
/// These strings reach `short` from `sessions.json` (hand-editable), from a
/// request body, and from a branch name in an error message — and four call
/// sites used to abbreviate them with `&s[..s.len().min(7)]`, which panics
/// whenever byte 7 lands inside a character. Confirmed against the real
/// slicing: `"日本語です"` is 15 bytes and byte 7 is inside the third character.
#[test]
fn abbreviating_is_by_character_not_by_byte() {
    // The ordinary case, unchanged.
    assert_eq!(short("0123456789abcdef"), "0123456");
    // Shorter than the cut, so all of it.
    assert_eq!(short("abc"), "abc");
    assert_eq!(short(""), "");
    // The ones that panicked: multibyte, and a boundary inside a character.
    assert_eq!(short("日本語です"), "日本語です");
    assert_eq!(short("日本語ですかとても"), "日本語ですかと");
    assert_eq!(short("éééééééééé"), "ééééééé");
    // A grapheme cluster is not a character, and seven chars is the contract —
    // pinned so nobody "fixes" this into something that can slice mid-scalar.
    assert_eq!(short("🇳🇱🇳🇱🇳🇱🇳🇱").chars().count(), 7);
}

/// Whether git refused because the branch is checked out in another tree.
///
/// The *refusal* is the invariant two tests here are built on; its wording is
/// not. Git says "already used by worktree at" from 2.35 and "already checked
/// out at" before it, so matching one string made both tests fail on an older
/// git for a reason that had nothing to do with the code — which is the whole
/// failure mode a test is supposed to rule out.
fn refused_as_already_checked_out(err: &str) -> bool {
    err.contains("already used by worktree") || err.contains("already checked out")
}

/// The assumption `spawn::refuse_if_main_is_on` exists for: git will not check
/// one branch out into two trees. Pinned here because the guard's whole value
/// is turning this refusal into a sentence that says what to do, and a git
/// that stopped refusing would leave two agents on one branch instead.
#[test]
fn a_branch_checked_out_in_main_cannot_be_cut_into_a_worktree() {
    let dir = std::env::temp_dir().join(format!(
        "orchd-wt-twice-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let main = dir.join("main");
    git(&dir, &["init", "-q", "main"]).unwrap();
    git(&main, &["config", "user.email", "t@t"]).unwrap();
    git(&main, &["config", "user.name", "t"]).unwrap();
    std::fs::write(main.join("a.txt"), "x").unwrap();
    git(&main, &["add", "-A"]).unwrap();
    git(&main, &["commit", "-qm", "init"]).unwrap();
    git(&main, &["checkout", "-q", "-b", "feature/x"]).unwrap();

    assert_eq!(current_branch(&main).unwrap(), "feature/x");
    let err = worktree_add_existing(&main, &dir.join("pr-1"), "feature/x")
        .expect_err("git must refuse a second checkout of one branch");
    assert!(
        refused_as_already_checked_out(&format!("{err:#}")),
        "unexpected refusal: {err:#}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The bank is a round trip, and both halves are exact: staged stays staged,
/// unstaged stays unstaged, and the tree in between is clean enough to rebase.
#[test]
fn banking_cleans_the_tree_and_the_ref_carries_the_work_back() {
    let main = bank_fixture("bank-round-trip");
    std::fs::write(main.join("f.txt"), "edited\n").unwrap();
    std::fs::write(main.join("g.txt"), "staged\n").unwrap();
    git(&main, &["add", "g.txt"]).unwrap();
    std::fs::write(main.join("new.txt"), "untracked\n").unwrap();

    let bank = bank_wip(&main, "invoice")
        .unwrap()
        .expect("a dirty tree banks");
    assert_eq!(bank.files, 2, "tracked changes only, both of them");
    assert_eq!(
        status(&main, None, Untracked::Collapsed)
            .unwrap()
            .unstaged
            .len(),
        0,
        "the tree has to be clean or the rebase cannot start"
    );
    // The one thing a bank never carries, and it never needed to: an untracked
    // file is not in a rebase's way unless the base adds the same path, which
    // git refuses on its own.
    assert!(
        main.join("new.txt").exists(),
        "untracked files stay where they are"
    );
    assert_eq!(banked_wip(&main, "invoice").map(|b| b.sha), Some(bank.sha));

    restore_wip(&main, "invoice").unwrap();
    let set = status(&main, None, Untracked::Each).unwrap();
    assert!(
        set.staged.iter().any(|f| f.path == "g.txt"),
        "the index came back too"
    );
    assert!(set.unstaged.iter().any(|f| f.path == "f.txt"));
    assert!(
        banked_wip(&main, "invoice").is_none(),
        "a clean apply drops the ref"
    );
}

/// The failure the whole shape is for: the work does not go back, and it is
/// still there afterwards. `git rebase --autostash` answers this case by
/// pushing onto `refs/stash`, which every worktree of the repo shares.
#[test]
fn a_restore_that_conflicts_keeps_the_bank() {
    let main = bank_fixture("bank-conflict");
    std::fs::write(main.join("f.txt"), "mine\n").unwrap();
    let bank = bank_wip(&main, "invoice").unwrap().expect("banked");

    // The base moves under it, onto the same line.
    std::fs::write(main.join("f.txt"), "theirs\n").unwrap();
    git(&main, &["commit", "-qam", "somebody else"]).unwrap();

    let err = restore_wip(&main, "invoice").expect_err("it cannot apply cleanly");
    assert!(
        format!("{err:#}").contains(&bank.sha),
        "the failure has to name the object, or the work is unreachable: {err:#}"
    );
    assert_eq!(
        banked_wip(&main, "invoice").map(|b| b.sha),
        Some(bank.sha),
        "the bank stands until somebody says otherwise"
    );
    assert!(
        !unmerged(&main).unwrap().is_empty(),
        "both sides are in the tree, which is where they can be resolved"
    );
    // And the shared stack is untouched, which is the property the ref exists for.
    assert_eq!(git(&main, &["stash", "list"]).unwrap().trim(), "");
}

/// A restart has to find these, and one exec finds all of them: refs are
/// per-repository, so nothing here walks the worktrees.
#[test]
fn every_bank_in_the_repo_is_listed_at_once() {
    let main = bank_fixture("bank-list");
    std::fs::write(main.join("f.txt"), "one\n").unwrap();
    bank_wip(&main, "invoice").unwrap().expect("banked");
    std::fs::write(main.join("f.txt"), "two\n").unwrap();
    bank_wip(&main, "billing").unwrap().expect("banked");

    let mut found: Vec<String> = all_banked(&main).into_iter().map(|(at, _)| at).collect();
    found.sort();
    assert_eq!(
        found,
        vec![wip_ref("billing"), wip_ref("invoice")],
        "the ref is the answer, because a mangled one cannot be read backwards"
    );

    // The name that made the mangling necessary, listed by the ref a caller can
    // compute rather than by a workspace nobody could recover from it.
    std::fs::write(main.join("f.txt"), "three\n").unwrap();
    bank_wip(&main, "thing.lock").unwrap().expect("banked");
    assert!(all_banked(&main)
        .iter()
        .any(|(at, _)| *at == wip_ref("thing.lock")));

    discard_wip(&main, "invoice").unwrap();
    assert_eq!(
        all_banked(&main).len(),
        2,
        "a dropped bank is gone from the list"
    );
    assert!(banked_wip(&main, "invoice").is_none());
    assert!(
        banked_wip(&main, "thing.lock").is_some(),
        "and only that one went"
    );
}

/// Git names the paths under its header and then leaves the margin for advice,
/// and the generic arm above this printed the header alone — "would be
/// overwritten by checkout" with no word about what.
#[test]
fn the_untracked_collision_is_read_off_gits_own_list() {
    let msg = "error: The following untracked working tree files would be overwritten by \
               checkout:\n\tsrc/timing.rs\n\tdocs/new.md\nPlease move or remove them.\n";
    assert_eq!(
        untracked_in_the_way(msg),
        vec!["src/timing.rs", "docs/new.md"]
    );
    assert!(untracked_in_the_way("rebase failed: something else").is_empty());
}

/// A ref may not end in a dot or in `.lock`, and a worktree name may be both.
#[test]
fn a_bank_ref_is_a_legal_ref_for_any_legal_worktree_name() {
    assert_eq!(wip_ref("invoice"), "refs/orchd/wip/invoice");
    assert_eq!(wip_ref("pr-101"), "refs/orchd/wip/pr-101");
    assert_eq!(wip_ref("thing.lock"), "refs/orchd/wip/thing.lock-");
    assert_eq!(wip_ref("trailing."), "refs/orchd/wip/trailing.-");
}

fn bank_fixture(name: &str) -> std::path::PathBuf {
    let main = crate::testutil::scratch(name).join("repo");
    git(
        main.parent().unwrap(),
        &["init", "-q", "-b", "main", "repo"],
    )
    .unwrap();
    git(&main, &["config", "user.email", "t@t"]).unwrap();
    git(&main, &["config", "user.name", "t"]).unwrap();
    std::fs::write(main.join("f.txt"), "committed\n").unwrap();
    std::fs::write(main.join("g.txt"), "committed\n").unwrap();
    git(&main, &["add", "-A"]).unwrap();
    git(&main, &["commit", "-qm", "base"]).unwrap();
    main
}

/// The three file verbs, and the asymmetry that decides which is confirmed.
///
/// Stage and unstage are each other's undo. Discard is not undoable by git at
/// all — `restore` overwrites the working tree from the index and there is no
/// reflog for content that was never committed — which is the whole reason the
/// pane asks first for that one and not for the others.
#[test]
fn staging_is_reversible_and_discarding_is_not() {
    let main = crate::testutil::scratch("fileverb").join("repo");
    git(
        main.parent().unwrap(),
        &["init", "-q", "-b", "main", "repo"],
    )
    .unwrap();
    git(&main, &["config", "user.email", "t@t"]).unwrap();
    git(&main, &["config", "user.name", "t"]).unwrap();
    std::fs::write(main.join("f.txt"), "committed\n").unwrap();
    git(&main, &["add", "-A"]).unwrap();
    git(&main, &["commit", "-qm", "base"]).unwrap();

    let set = || status(&main, None, Untracked::Each).unwrap();
    let has = |v: &[crate::model::ChangedFile], p: &str| v.iter().any(|f| f.path == p);

    std::fs::write(main.join("f.txt"), "edited\n").unwrap();
    assert!(has(&set().unstaged, "f.txt"));

    file_verb(&main, FileVerb::Stage, "f.txt").unwrap();
    assert!(has(&set().staged, "f.txt"), "staged");
    assert!(!has(&set().unstaged, "f.txt"));

    // Pressing the other one is the undo, which is why neither is confirmed.
    file_verb(&main, FileVerb::Unstage, "f.txt").unwrap();
    assert!(!has(&set().staged, "f.txt"));
    assert!(
        has(&set().unstaged, "f.txt"),
        "back where it was, content intact"
    );
    assert_eq!(
        std::fs::read_to_string(main.join("f.txt")).unwrap(),
        "edited\n"
    );

    // And discard is the one that takes the content with it.
    file_verb(&main, FileVerb::Discard, "f.txt").unwrap();
    assert_eq!(
        std::fs::read_to_string(main.join("f.txt")).unwrap(),
        "committed\n"
    );
    assert!(!has(&set().unstaged, "f.txt"), "nothing left to discard");

    // Staging covers an untracked file too, which is the one row where it is
    // the only verb on offer.
    std::fs::write(main.join("new.txt"), "never added\n").unwrap();
    assert!(has(&set().untracked, "new.txt"));
    file_verb(&main, FileVerb::Stage, "new.txt").unwrap();
    assert!(has(&set().staged, "new.txt"));

    // A path that looks like a flag is a path: `--` is what makes that true.
    std::fs::write(main.join("-f"), "dashed\n").unwrap();
    file_verb(&main, FileVerb::Stage, "-f").unwrap();
    assert!(has(&set().staged, "-f"));

    let _ = std::fs::remove_dir_all(main.parent().unwrap());
}

/// A ref has the files it was committed with, and not the ones beside them.
///
/// The question a blob URL is about: the changed-files pane lists untracked
/// files on purpose, and offering "open on forge" for one opened a 404.
#[test]
fn a_ref_has_its_tracked_paths_and_not_the_untracked_ones() {
    let main = crate::testutil::scratch("haspath").join("repo");
    git(
        main.parent().unwrap(),
        &["init", "-q", "-b", "main", "repo"],
    )
    .unwrap();
    git(&main, &["config", "user.email", "t@t"]).unwrap();
    git(&main, &["config", "user.name", "t"]).unwrap();
    std::fs::write(main.join("kept.txt"), "in the commit\n").unwrap();
    git(&main, &["add", "-A"]).unwrap();
    git(&main, &["commit", "-qm", "base"]).unwrap();
    // On disk and never added, which is what the pane shows as `?`.
    std::fs::write(main.join("new.txt"), "not in any ref\n").unwrap();
    std::fs::create_dir_all(main.join("cache")).unwrap();
    std::fs::write(main.join("cache/x"), "nor this\n").unwrap();

    assert!(has_path_at(&main, "HEAD", "kept.txt"));
    assert!(
        !has_path_at(&main, "HEAD", "new.txt"),
        "on disk is not in the ref"
    );
    // The shape `--untracked-files=normal` collapses a directory to, which is
    // not a blob at any ref whatever it holds.
    assert!(!has_path_at(&main, "HEAD", "cache/"));
    // A ref that does not resolve answers no rather than opening a URL.
    assert!(!has_path_at(&main, "deadbeef", "kept.txt"));

    let _ = std::fs::remove_dir_all(main.parent().unwrap());
}

/// Who has a branch checked out, and the two answers that must not be a path.
///
/// The porcelain listing is parsed rather than the human one, so this pins the
/// two shapes that carry no branch: a detached tree prints no `branch` line at
/// all, and a branch nobody has is simply absent.
#[test]
fn the_holder_of_a_branch_is_the_tree_that_has_it_checked_out() {
    let real = crate::testutil::scratch("holder");
    /* **Reached through a symlink on purpose.** What that pins is git's own
    behaviour: the listing comes back *resolved* whichever way in you walked,
    which is the fact the canonicalise above is written not to depend on. It
    also puts the assertion in the shape a Mac gives every test under
    `$TMPDIR`, where the resolved path is a different string from the one the
    fixture built. */
    let dir = real.parent().unwrap().join(format!(
        "{}-via",
        real.file_name().unwrap().to_string_lossy()
    ));
    let _ = std::fs::remove_file(&dir);
    std::os::unix::fs::symlink(&real, &dir).unwrap();
    let main = dir.join("repo");
    git(&dir, &["init", "-q", "-b", "main", "repo"]).unwrap();
    git(&main, &["config", "user.email", "t@t"]).unwrap();
    git(&main, &["config", "user.name", "t"]).unwrap();
    std::fs::write(main.join("f.txt"), "base\n").unwrap();
    git(&main, &["add", "-A"]).unwrap();
    git(&main, &["commit", "-qm", "base"]).unwrap();
    git(&main, &["branch", "feature/b"]).unwrap();
    git(&main, &["branch", "nobody/has-this"]).unwrap();
    let tree = main.join(".claude/worktrees/w");
    git(
        &main,
        &["worktree", "add", "-q", tree.to_str().unwrap(), "feature/b"],
    )
    .unwrap();

    // Resolved, so it can be compared with the daemon's own canonical paths —
    // and `main` here is the symlinked way in, which must *not* be the answer.
    let resolved = |p: &Path| std::fs::canonicalize(p).unwrap();
    assert_eq!(
        holder_of_branch(&main, "main").unwrap(),
        Some(resolved(&main)),
        "the answer is compared against canonical paths, so it has to be one",
    );
    assert_eq!(
        holder_of_branch(&main, "feature/b").unwrap(),
        Some(resolved(&tree))
    );
    assert_eq!(holder_of_branch(&main, "nobody/has-this").unwrap(), None);

    // Released: the tree keeps the commit it had, under a name of its own, and
    // the branch it held is free for somebody else — which is the whole point.
    let fresh = release_branch(&tree, "worktree-w").expect("a clean tree may be moved");
    assert_eq!(fresh, "worktree-w");
    assert_eq!(current_branch(&tree).unwrap(), "worktree-w");
    assert_eq!(holder_of_branch(&main, "feature/b").unwrap(), None);
    assert!(
        switch_branch(&main, "feature/b").is_ok(),
        "main can have it now"
    );

    // A detached tree answers nothing rather than answering its commit.
    switch_detach(&tree).unwrap();
    assert_eq!(holder_of_branch(&main, "worktree-w").unwrap(), None);

    // And a tree carrying work is refused: a switch would take the work with it.
    std::fs::write(tree.join("f.txt"), "edited\n").unwrap();
    assert!(
        release_branch(&tree, "worktree-w").is_err(),
        "dirty is refused"
    );

    let _ = std::fs::remove_file(&dir);
    let _ = std::fs::remove_dir_all(&real);
}

/// The guards that stand between "you closed the last pane in main" and
/// someone's uncommitted work landing on develop.
#[test]
fn parking_leaves_a_dirty_checkout_exactly_where_it_is() {
    let dir = std::env::temp_dir().join(format!(
        "orchd-park-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let repo = dir.join("repo");
    git(&dir, &["init", "-q", "-b", "develop", "repo"]).unwrap();
    git(&repo, &["config", "user.email", "t@t"]).unwrap();
    git(&repo, &["config", "user.name", "t"]).unwrap();
    std::fs::write(repo.join("a.txt"), "x").unwrap();
    git(&repo, &["add", "-A"]).unwrap();
    git(&repo, &["commit", "-qm", "init"]).unwrap();

    // Already home: nothing to do, and no needless checkout.
    assert_eq!(park_on_base(&repo, "develop", None).unwrap(), None);

    git(&repo, &["checkout", "-q", "-b", "feature/x"]).unwrap();
    std::fs::write(repo.join("a.txt"), "edited").unwrap();
    // Dirty: the work would ride along to develop, so it stays put.
    assert_eq!(park_on_base(&repo, "develop", None).unwrap(), None);
    assert_eq!(current_branch(&repo).unwrap(), "feature/x");

    git(&repo, &["checkout", "-q", "--", "a.txt"]).unwrap();
    assert_eq!(
        park_on_base(&repo, "develop", None).unwrap().as_deref(),
        Some("feature/x")
    );
    assert_eq!(current_branch(&repo).unwrap(), "develop");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A fork layout is the one thing a first run can work out for itself, so it
/// has to be right about both answers: present, and absent.
#[test]
fn a_fork_layout_is_detected_and_nothing_else_is_assumed() {
    let dir = std::env::temp_dir().join(format!(
        "orchd-detect-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let repo = dir.join("repo");
    git(&dir, &["init", "-q", "repo"]).unwrap();
    git(
        &repo,
        &["remote", "add", "origin", "git@github.com:you/monorepo.git"],
    )
    .unwrap();

    // origin alone is not a fork: no opinion, so the generic default stands.
    assert_eq!(detect_base(&repo), None);

    git(
        &repo,
        &[
            "remote",
            "add",
            "upstream",
            "git@github.com:acme/monorepo.git",
        ],
    )
    .unwrap();
    // Never fetched, so `upstream/HEAD` does not resolve yet — the symbolic
    // form is the honest answer rather than a guessed branch name.
    assert_eq!(
        detect_base(&repo),
        Some(("upstream/HEAD".to_string(), "upstream".to_string()))
    );

    // Once the symref exists it is used, which is what makes a
    // develop-defaulting fork come out as `upstream/develop`.
    git(
        &repo,
        &[
            "symbolic-ref",
            "refs/remotes/upstream/HEAD",
            "refs/remotes/upstream/develop",
        ],
    )
    .unwrap();
    assert_eq!(
        detect_base(&repo),
        Some(("upstream/develop".to_string(), "upstream".to_string()))
    );

    // Not a git repo at all is also no opinion, not a panic.
    assert_eq!(detect_base(&dir.join("nope")), None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A WIP that will not re-apply is a warning on a swap that happened, never an
/// error instead of it.
///
/// It used to return `Err`, and the caller bailed on the `?` before it forgot
/// the traded branches, reconciled the panes or moved the conversations — so
/// git held the swapped world and the daemon described the pre-swap one, with
/// the SPA reporting a plain failure. Nothing could reconcile that afterwards.
///
/// The apply is made to fail the one way it can: main's banked *new* file lands
/// in a worktree that already has an untracked file of that name. Untracked
/// files do not travel, so it is still sitting there.
#[test]
fn a_wip_that_cannot_reapply_still_leaves_the_branches_swapped() {
    let dir = std::env::temp_dir().join(format!(
        "orchd-swapwip-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let main = dir.join("repo");
    git(&dir, &["init", "-q", "-b", "main", "repo"]).unwrap();
    git(&main, &["config", "user.email", "t@t"]).unwrap();
    git(&main, &["config", "user.name", "t"]).unwrap();
    std::fs::write(main.join("f.txt"), "base\n").unwrap();
    git(&main, &["add", "-A"]).unwrap();
    git(&main, &["commit", "-qm", "base"]).unwrap();
    git(&main, &["branch", "feature/b"]).unwrap();
    let tree = main.join(".claude/worktrees/w");
    git(
        &main,
        &["worktree", "add", "-q", tree.to_str().unwrap(), "feature/b"],
    )
    .unwrap();

    // Banked: a staged addition in main, which travels to the worktree.
    std::fs::write(main.join("x.txt"), "main's new file\n").unwrap();
    git(&main, &["add", "x.txt"]).unwrap();
    // In the way: the same name, untracked in the worktree, so it stays put and
    // the apply has nowhere to put main's copy.
    std::fs::write(tree.join("x.txt"), "already here, untracked\n").unwrap();

    let s = swap_branches(&main, &tree).expect("a failed re-apply is not a failed swap");
    assert_eq!(current_branch(&main).unwrap(), "feature/b");
    assert_eq!(current_branch(&tree).unwrap(), "main");
    let why = s.wip_error.expect("the re-apply failure is reported");
    assert!(
        why.contains("did not re-apply") && why.contains("git stash apply"),
        "the message must name the commit and how to recover it: {why}"
    );
    // The work is still banked, not lost — that is what makes the warning a
    // warning. And the file in the way is untouched.
    assert_eq!(
        std::fs::read_to_string(tree.join("x.txt")).unwrap(),
        "already here, untracked\n"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Moving main's branch out: the tree is cut *after* main lets go, the work
/// travels, and main is left on base rather than on a detached head.
#[test]
fn moving_a_branch_out_of_main_carries_its_work_and_leaves_main_on_base() {
    let dir = std::env::temp_dir().join(format!(
        "orchd-moveout-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let main = dir.join("repo");
    git(&dir, &["init", "-q", "-b", "develop", "repo"]).unwrap();
    git(&main, &["config", "user.email", "t@t"]).unwrap();
    git(&main, &["config", "user.name", "t"]).unwrap();
    std::fs::write(main.join("f.txt"), "base\n").unwrap();
    git(&main, &["add", "-A"]).unwrap();
    git(&main, &["commit", "-qm", "base"]).unwrap();
    git(&main, &["switch", "-qc", "feature/b"]).unwrap();

    // Dirty, staged and untracked: the three cases the carry treats differently.
    std::fs::write(main.join("f.txt"), "edited in main\n").unwrap();
    std::fs::write(main.join("staged.txt"), "staged\n").unwrap();
    git(&main, &["add", "staged.txt"]).unwrap();
    std::fs::write(main.join("loose.txt"), "untracked\n").unwrap();

    let dest = main.join(".claude/worktrees/b");
    let moved = move_branch_out(&main, &dest, "develop", "worktree-b").expect("the move");
    assert_eq!(
        (moved.branch.as_str(), moved.base.as_str()),
        ("feature/b", "develop")
    );
    assert!(!moved.created, "the branch was handed over, not cut");
    assert!(
        moved.wip_error.is_none(),
        "the carry: {:?}",
        moved.wip_error
    );

    assert_eq!(current_branch(&main).unwrap(), "develop");
    assert_eq!(current_branch(&dest).unwrap(), "feature/b");
    // The work is in the worktree, index distinction intact.
    assert_eq!(
        std::fs::read_to_string(dest.join("f.txt")).unwrap(),
        "edited in main\n"
    );
    assert!(
        dest.join("staged.txt").exists(),
        "the staged file travelled"
    );
    assert!(
        status(&dest, None, Untracked::Each)
            .unwrap()
            .staged
            .iter()
            .any(|f| f.path == "staged.txt"),
        "and it is still staged"
    );
    // Main kept none of the tracked work — that is the half that had to travel —
    // and the untracked file is still there, because `stash create` cannot take
    // one. Asserted rather than `is_clean`, which counts that file and would
    // read this correct state as dirty.
    let left = status(&main, Some(".claude/worktrees/"), Untracked::Each).unwrap();
    assert!(
        left.staged.is_empty() && left.unstaged.is_empty(),
        "main still holds tracked work: {left:?}"
    );
    assert_eq!(
        left.untracked
            .iter()
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>(),
        ["loose.txt"],
        "the untracked file stayed put, and is the only thing that did"
    );
    assert_eq!(
        std::fs::read_to_string(main.join("f.txt")).unwrap(),
        "base\n"
    );

    // --- and the other half: main on base, with work but no branch of its own ---
    //
    // Nothing to hand over, so the work gets a branch cut for it and main does
    // not move at all. This is the case you land in by starting something in main
    // without branching first, which is the common one.
    std::fs::write(main.join("f.txt"), "started in main on develop\n").unwrap();
    let second = main.join(".claude/worktrees/work");
    let cut = move_branch_out(&main, &second, "develop", "worktree-work").expect("the cut");
    assert_eq!(
        (cut.branch.as_str(), cut.base.as_str()),
        ("worktree-work", "develop")
    );
    assert!(cut.created, "the branch had to be created");
    assert!(cut.wip_error.is_none(), "the carry: {:?}", cut.wip_error);
    assert_eq!(current_branch(&second).unwrap(), "worktree-work");
    assert_eq!(
        std::fs::read_to_string(second.join("f.txt")).unwrap(),
        "started in main on develop\n",
        "the work did not travel to the branch cut for it"
    );
    // Main never left base and kept none of it.
    assert_eq!(current_branch(&main).unwrap(), "develop");
    assert_eq!(
        std::fs::read_to_string(main.join("f.txt")).unwrap(),
        "base\n"
    );

    // Again, with the branch name already taken: suffixed rather than refused,
    // since a tree deleted long ago can leave its branch behind.
    std::fs::write(main.join("f.txt"), "and again\n").unwrap();
    let third = main.join(".claude/worktrees/work-2");
    let cut = move_branch_out(&main, &third, "develop", "worktree-work").expect("the cut");
    assert_eq!(cut.branch, "worktree-work-2");
}

/// Removal is a backstop, so "already gone" is a success.
///
/// The repo's `WorktreeRemove` hook runs first and may do the whole job. Without
/// this, `git worktree remove` on a path that is no longer there fails with "is
/// not a working tree" and turns a completed teardown into a reported failure.
/// The prune is still owed: the hook deleted a directory and git's registration
/// of it is what remains.
#[test]
fn removing_a_worktree_that_a_hook_already_took_is_not_an_error() {
    let root = std::env::temp_dir().join(format!("orch-idem-{}", uuid::Uuid::new_v4()));
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let sh = |cwd: &Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    sh(&repo, &["init", "-q", "-b", "main"]);
    sh(&repo, &["config", "user.email", "t@t"]);
    sh(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("f"), "1").unwrap();
    sh(&repo, &["add", "-A"]);
    sh(&repo, &["commit", "-qm", "base"]);

    let wt = root.join("wt");
    worktree_add_new(&repo, &wt, "wt-gone", &head_sha(&repo).unwrap()).unwrap();
    // What a hook that owns removal leaves behind: no directory, and a
    // registration git still believes in.
    std::fs::remove_dir_all(&wt).unwrap();
    assert!(
        git(&repo, &["worktree", "list"])
            .unwrap()
            .contains("wt-gone")
            || git(&repo, &["worktree", "list"]).unwrap().contains("wt")
    );

    worktree_remove(&repo, &wt).expect("already gone is a success");
    // And the registration is cleared, not merely tolerated.
    assert!(!git(&repo, &["worktree", "list"]).unwrap().contains("/wt"));

    let _ = std::fs::remove_dir_all(&root);
}

/// A remote-tracking base is fetched before the cut, so a daemon-cut worktree
/// does not start from whatever the last fetch happened to leave behind.
///
/// The repo's own `WorktreeCreate` hook opens with this fetch, and this is the
/// fallback taken when there was no usable hook to run it. Driven against a real
/// second repository, because
/// the thing being asserted is that the new tree holds a commit that existed
/// only on the remote a moment ago.
#[test]
fn cutting_from_a_remote_base_fetches_it_first() {
    let root = std::env::temp_dir().join(format!("orch-fetchbase-{}", uuid::Uuid::new_v4()));
    let origin = root.join("origin");
    let repo = root.join("repo");
    std::fs::create_dir_all(&origin).unwrap();
    let sh = |cwd: &Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    sh(&origin, &["init", "-q", "-b", "develop"]);
    sh(&origin, &["config", "user.email", "t@t"]);
    sh(&origin, &["config", "user.name", "t"]);
    std::fs::write(origin.join("f"), "1").unwrap();
    sh(&origin, &["add", "-A"]);
    sh(&origin, &["commit", "-qm", "first"]);

    let out = std::process::Command::new("git")
        .args([
            "clone",
            "-q",
            &origin.to_string_lossy(),
            &repo.to_string_lossy(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    sh(&repo, &["remote", "rename", "origin", "upstream"]);

    // A commit the clone has never seen. Without the fetch, cutting from
    // `upstream/develop` gives the tree the *old* tip.
    std::fs::write(origin.join("g"), "2").unwrap();
    sh(&origin, &["add", "-A"]);
    sh(&origin, &["commit", "-qm", "landed after the clone"]);

    let wt = root.join("wt");
    worktree_add_new(&repo, &wt, "wt-fresh", "upstream/develop").unwrap();
    assert!(
        wt.join("g").exists(),
        "the new tree has the commit that landed after the clone"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Adopting a tree the repo's `WorktreeCreate` hook made.
///
/// The hook cuts from a base of its own, so the daemon puts the tree on what it
/// actually needs afterwards. Both shapes are asserted, and so is the refusal
/// that keeps the fallback honest: a dirty tree is a hook that left work behind,
/// and carrying it silently onto another branch is how that stops being noticed.
#[test]
fn a_tree_a_hook_made_can_be_put_on_the_branch_the_daemon_needs() {
    let root = std::env::temp_dir().join(format!("orch-adopt-{}", uuid::Uuid::new_v4()));
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let sh = |cwd: &Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    sh(&repo, &["init", "-q", "-b", "develop"]);
    sh(&repo, &["config", "user.email", "t@t"]);
    sh(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("f"), "1").unwrap();
    sh(&repo, &["add", "-A"]);
    sh(&repo, &["commit", "-qm", "base"]);
    // A branch that already exists, the shape a PR head has.
    sh(&repo, &["branch", "feature/head"]);

    // What a hook leaves: a tree on a branch of its own choosing.
    let made = root.join("hook-made");
    worktree_add_new(&repo, &made, "worktree-hookname", "develop").unwrap();
    assert_eq!(current_branch(&made).unwrap(), "worktree-hookname");

    // Want::Existing — the PR case.
    checkout_existing_branch(&repo, &made, "feature/head").unwrap();
    assert_eq!(current_branch(&made).unwrap(), "feature/head");

    // Want::New — the plain and fork cases. `-B`, so a retry over a name the
    // daemon already owns does not fail.
    checkout_new_branch(&repo, &made, "worktree-mine", "develop").unwrap();
    assert_eq!(current_branch(&made).unwrap(), "worktree-mine");
    checkout_new_branch(&repo, &made, "worktree-mine", "develop").unwrap();

    // A dirty tree refuses, both ways, so the caller falls back rather than
    // moving work it did not put there.
    std::fs::write(made.join("f"), "changed by the hook\n").unwrap();
    assert!(checkout_new_branch(&repo, &made, "worktree-other", "develop").is_err());
    assert!(checkout_existing_branch(&repo, &made, "feature/head").is_err());

    let _ = std::fs::remove_dir_all(&root);
}

/// A fork's carry: the work arrives, and the parent keeps it.
///
/// Both halves matter and only one is obvious. `capture_wip` banks and then
/// resets, which is right for a swap and would be theft here — a fork is a
/// second place to work, not a move, so the parent tree must look untouched
/// afterwards. That is the assertion this test exists for.
#[test]
fn a_fork_copies_the_work_across_and_leaves_the_parent_holding_it() {
    let root = std::env::temp_dir().join(format!("orch-copywip-{}", uuid::Uuid::new_v4()));
    let parent = root.join("parent");
    std::fs::create_dir_all(&parent).unwrap();
    let sh = |cwd: &Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    sh(&parent, &["init", "-q", "-b", "main"]);
    sh(&parent, &["config", "user.email", "t@t"]);
    sh(&parent, &["config", "user.name", "t"]);
    std::fs::write(parent.join("tracked.txt"), "committed\n").unwrap();
    sh(&parent, &["add", "-A"]);
    sh(&parent, &["commit", "-qm", "base"]);

    // The state a fork is supposed to reproduce: an edit to a tracked file, and
    // an untracked file that cannot travel.
    std::fs::write(parent.join("tracked.txt"), "edited in the parent\n").unwrap();
    std::fs::write(parent.join("new.txt"), "untracked\n").unwrap();

    // The fork's tree, cut from the parent's HEAD the way `spawn` cuts it.
    let head = head_sha(&parent).unwrap();
    let fork = root.join("fork");
    worktree_add_new(&parent, &fork, "wt-fork", &head).unwrap();

    let sha = copy_wip(&parent, &fork)
        .unwrap()
        .expect("there was work to carry");
    assert!(!sha.is_empty());

    // It arrived.
    assert_eq!(
        std::fs::read_to_string(fork.join("tracked.txt")).unwrap(),
        "edited in the parent\n"
    );
    // And the parent still has it — `stash create` writes an object and does not
    // touch the tree, which is the whole reason this is not `capture_wip`.
    assert_eq!(
        std::fs::read_to_string(parent.join("tracked.txt")).unwrap(),
        "edited in the parent\n"
    );
    // Untracked files do not travel, and the caller is the one that says so.
    assert!(
        !fork.join("new.txt").exists(),
        "stash create cannot carry untracked"
    );
    assert_eq!(
        untracked_in(&parent, None).unwrap(),
        vec!["new.txt".to_string()]
    );

    // A clean parent has nothing to carry and says so rather than erroring.
    sh(&parent, &["add", "-A"]);
    sh(&parent, &["commit", "-qm", "tidy"]);
    let fork2 = root.join("fork2");
    worktree_add_new(&parent, &fork2, "wt-fork2", &head_sha(&parent).unwrap()).unwrap();
    assert_eq!(copy_wip(&parent, &fork2).unwrap(), None);

    let _ = std::fs::remove_dir_all(&root);
}

/// The swap, and the refusal it is built around: git will not check one branch
/// out twice, so the naive "switch each tree" fails on the first move. Both
/// halves are pinned here because the three-step order *is* the feature.
#[test]
fn swapping_exchanges_two_branches_and_is_its_own_inverse() {
    let dir = std::env::temp_dir().join(format!(
        "orchd-swap-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let main = dir.join("repo");
    git(&dir, &["init", "-q", "-b", "main", "repo"]).unwrap();
    git(&main, &["config", "user.email", "t@t"]).unwrap();
    git(&main, &["config", "user.name", "t"]).unwrap();
    std::fs::write(main.join("f.txt"), "base\n").unwrap();
    git(&main, &["add", "-A"]).unwrap();
    git(&main, &["commit", "-qm", "base"]).unwrap();
    git(&main, &["branch", "feature/b"]).unwrap();
    let tree = main.join(".claude/worktrees/w");
    git(
        &main,
        &["worktree", "add", "-q", tree.to_str().unwrap(), "feature/b"],
    )
    .unwrap();

    assert_eq!(current_branch(&main).unwrap(), "main");
    assert_eq!(current_branch(&tree).unwrap(), "feature/b");

    // The refusal the three steps exist for: one branch, one tree.
    let naive = git(&main, &["switch", "feature/b"])
        .expect_err("git must refuse a branch already checked out");
    assert!(
        refused_as_already_checked_out(&format!("{naive:#}")),
        "unexpected refusal: {naive:#}"
    );

    let s = swap_branches(&main, &tree).expect("the swap");
    assert_eq!(
        (s.main_now.as_str(), s.worktree_now.as_str()),
        ("feature/b", "main")
    );
    assert!(
        s.wip_error.is_none(),
        "nothing to carry, nothing to warn about"
    );
    assert_eq!(current_branch(&main).unwrap(), "feature/b");
    assert_eq!(current_branch(&tree).unwrap(), "main");
    // Neither tree is left detached or dirty.
    assert!(
        is_clean_excluding(&main, Some(".claude/worktrees/")).unwrap() && is_clean(&tree).unwrap(),
        "excluding for main, which contains the worktrees dir"
    );

    // Swapping again is the undo, which is what makes the menu item safe to
    // press twice.
    swap_branches(&main, &tree).expect("swap back");
    assert_eq!(current_branch(&main).unwrap(), "main");
    assert_eq!(current_branch(&tree).unwrap(), "feature/b");

    // --- and the uncommitted work travels with its branch ---
    //
    // Both sides dirty, so the crosswise apply is exercised in both directions
    // rather than only the one anybody would test by hand.
    std::fs::write(main.join("f.txt"), "main was editing this\n").unwrap();
    std::fs::write(tree.join("f.txt"), "the worktree was editing this\n").unwrap();
    // One staged as well, since `stash create` banks the index too and
    // `stash apply --index` is what restores that distinction.
    std::fs::write(tree.join("staged.txt"), "staged in the worktree\n").unwrap();
    git(&tree, &["add", "staged.txt"]).unwrap();

    let s = swap_branches(&main, &tree).expect("swap with work in both trees");
    assert!(
        s.wip_error.is_none(),
        "both sides re-applied: {:?}",
        s.wip_error
    );

    assert_eq!(current_branch(&main).unwrap(), "feature/b");
    assert_eq!(current_branch(&tree).unwrap(), "main");
    assert_eq!(
        std::fs::read_to_string(main.join("f.txt")).unwrap(),
        "the worktree was editing this\n",
        "the worktree's edit followed its branch into main"
    );
    assert_eq!(
        std::fs::read_to_string(tree.join("f.txt")).unwrap(),
        "main was editing this\n",
        "and main's edit went the other way"
    );
    assert!(
        main.join("staged.txt").exists(),
        "a staged addition travels too"
    );
    // Still staged, not merely present: `--index` is the difference.
    let staged = git(&main, &["diff", "--cached", "--name-only"]).unwrap();
    assert!(
        staged.contains("staged.txt"),
        "index preserved, got {staged:?}"
    );

    // Nothing was left banked behind either: a clean tree means no reset ran.
    swap_branches(&main, &tree).expect("swap back with the work");
    assert_eq!(
        std::fs::read_to_string(tree.join("f.txt")).unwrap(),
        "the worktree was editing this\n",
        "and it comes home again"
    );

    // Same branch both sides is refused rather than silently doing nothing.
    let same = main.join(".claude/worktrees/same");
    git(
        &main,
        &["worktree", "add", "-q", "--detach", same.to_str().unwrap()],
    )
    .unwrap();
    git(&same, &["switch", "-q", "-c", "third"]).unwrap();
    git(&same, &["switch", "-q", "--detach"]).unwrap();
    assert!(swap_branches(&main, &main).is_err(), "main against itself");

    let _ = std::fs::remove_dir_all(&dir);
}

/// `origin/HEAD` is the default base, and `git switch HEAD` fails with "a
/// branch is expected" — so anything that checks the base out has to resolve
/// the symref first. `park_main` did not, and silently never parked.
#[test]
fn a_head_base_resolves_to_a_real_branch_before_anyone_checks_it_out() {
    let dir = std::env::temp_dir().join(format!(
        "orchd-basebranch-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let repo = dir.join("repo");
    git(&dir, &["init", "-q", "repo"]).unwrap();
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "git@github.com:acme/monorepo.git",
        ],
    )
    .unwrap();

    // Unresolvable until the symref exists, which is "cannot" rather than a name.
    assert_eq!(base_checkout_branch(&repo, "origin/HEAD"), None);

    git(
        &repo,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    )
    .unwrap();
    assert_eq!(
        base_checkout_branch(&repo, "origin/HEAD").as_deref(),
        Some("main")
    );

    // A named base needs no repo lookup and passes straight through.
    assert_eq!(
        base_checkout_branch(&repo, "upstream/develop").as_deref(),
        Some("develop")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Both supported layouts, through the two functions the panes actually run on
/// the base ref: the merge-base the changed-file list is computed from, and the
/// behind/ahead the divergence strip shows.
///
/// Neither had a test against a *configured* base at all, so `origin/HEAD`
/// becoming the default rested on it being "a valid rev". It is, but a symref
/// is not the same shape as a branch name and that is exactly the assumption
/// worth pinning.
#[test]
fn both_a_fork_and_a_plain_layout_answer_merge_base_and_divergence() {
    let dir = std::env::temp_dir().join(format!(
        "orchd-flows-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // A remote to be the origin, and a clone of it.
    git(&dir, &["init", "-q", "--bare", "-b", "main", "origin.git"]).unwrap();
    let origin = dir.join("origin.git");
    let work = dir.join("work");
    git(&dir, &["init", "-q", "-b", "main", "work"]).unwrap();
    git(&work, &["config", "user.email", "t@t"]).unwrap();
    git(&work, &["config", "user.name", "t"]).unwrap();
    std::fs::write(work.join("a.txt"), "base\n").unwrap();
    git(&work, &["add", "-A"]).unwrap();
    git(&work, &["commit", "-qm", "base"]).unwrap();
    let base_sha = git(&work, &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    git(
        &work,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    )
    .unwrap();
    git(&work, &["push", "-q", "origin", "main"]).unwrap();

    // --- plain layout: one remote, base is its own default branch ---
    // `fetch_upstream` is what records the symref; without it `origin/HEAD`
    // does not resolve, which is the trap it was written for.
    fetch_upstream(&work, "origin/HEAD").expect("fetch");
    git(&work, &["checkout", "-q", "-b", "feature/x"]).unwrap();
    std::fs::write(work.join("a.txt"), "mine\n").unwrap();
    git(&work, &["commit", "-qam", "mine"]).unwrap();

    assert_eq!(
        merge_base(&work, "origin/HEAD").expect("merge-base against a symref"),
        base_sha,
        "the changed-file list is computed from this"
    );
    assert_eq!(
        divergence(&work, "origin/HEAD").expect("divergence against a symref"),
        (0, 1),
        "one commit ahead of the remote's default branch, none behind"
    );

    // --- fork layout: a second remote, base is a named branch on it ---
    git(
        &dir,
        &["init", "-q", "--bare", "-b", "develop", "upstream.git"],
    )
    .unwrap();
    let upstream = dir.join("upstream.git");
    git(
        &work,
        &["remote", "add", "upstream", upstream.to_str().unwrap()],
    )
    .unwrap();
    git(&work, &["push", "-q", "upstream", "main:develop"]).unwrap();
    fetch_upstream(&work, "upstream/develop").expect("fetch the named base");

    assert_eq!(
        merge_base(&work, "upstream/develop").expect("merge-base against a branch"),
        base_sha
    );
    assert_eq!(
        divergence(&work, "upstream/develop").expect("divergence"),
        (0, 1)
    );

    // Detection sees the fork, and answers with the symref rather than the
    // branch — because fetching a *named* base does not record
    // `upstream/HEAD`, and only a clone or the HEAD arm ever does. That is the
    // better answer anyway: `upstream/HEAD` is self-correcting when the remote
    // renames its default branch, where a recorded `develop` would rot.
    assert_eq!(
        detect_base(&work),
        Some(("upstream/HEAD".to_string(), "upstream".to_string()))
    );
    // And it resolves, once something records the symref.
    fetch_upstream(&work, "upstream/HEAD").expect("the HEAD arm records it");
    assert_eq!(
        base_checkout_branch(&work, "upstream/HEAD").as_deref(),
        Some("develop")
    );
    assert_eq!(
        merge_base(&work, "upstream/HEAD").expect("merge-base"),
        base_sha
    );

    // What the run overview needed and `divergence` cannot say. The branch is
    // one commit beyond the base and that commit is on nobody's remote, so both
    // read 1 here — the numbers only part once something is pushed.
    assert_eq!(divergence(&work, "upstream/develop").unwrap().1, 1);
    assert_eq!(
        unpushed_count(&work, "feature/x", "upstream/develop"),
        1,
        "never pushed, so everything beyond the base is unpushed"
    );
    git(&work, &["push", "-q", "origin", "feature/x"]).unwrap();
    assert_eq!(
        unpushed_count(&work, "feature/x", "upstream/develop"),
        0,
        "pushed, and the count follows the remote rather than the base"
    );
    std::fs::write(work.join("a.txt"), "more\n").unwrap();
    git(&work, &["commit", "-qam", "more"]).unwrap();
    assert_eq!(
        (
            divergence(&work, "upstream/develop").unwrap().1,
            unpushed_count(&work, "feature/x", "upstream/develop")
        ),
        (2, 1),
        "two commits past the base, one of them pushed — the case that made \
         `ahead` the wrong number to show"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn upstream_fetch_argv_is_config_driven_not_hardcoded() {
    // A fork workflow fetches one named branch (as before).
    assert_eq!(
        upstream_fetch_argv("upstream/develop"),
        vec!["fetch", "upstream", "develop", "--no-tags"]
    );
    // A nested branch name stays intact.
    assert_eq!(
        upstream_fetch_argv("origin/release/2026"),
        vec!["fetch", "origin", "release/2026", "--no-tags"]
    );
    // A bare ref assumes origin.
    assert_eq!(
        upstream_fetch_argv("main"),
        vec!["fetch", "origin", "main", "--no-tags"]
    );
}

#[test]
fn a_head_base_ref_is_recorded_and_then_resolves() {
    // The regression this guards: `git fetch <remote>` does not create
    // `refs/remotes/<remote>/HEAD`, so `origin/HEAD` did not resolve on a
    // checkout whose remote was added by hand — and every merge-base,
    // divergence and rebase against it failed silently.
    // Its own tree, not `scratch_repo`'s: these are three sibling repos and
    // nesting them inside another checkout confuses the remote plumbing.
    let dir = std::env::temp_dir().join(format!(
        "orchd-head-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let upstream = dir.join("up.git");
    let work = dir.join("work");
    git(&dir, &["init", "-q", "--bare", "-b", "main", "up.git"]).unwrap();
    git(&dir, &["init", "-q", "work"]).unwrap();
    git(&work, &["config", "user.email", "t@t"]).unwrap();
    git(&work, &["config", "user.name", "t"]).unwrap();
    std::fs::write(work.join("a.txt"), "x").unwrap();
    git(&work, &["add", "-A"]).unwrap();
    git(&work, &["commit", "-qm", "init"]).unwrap();
    git(&work, &["branch", "-M", "main"]).unwrap();
    git(
        &work,
        &["remote", "add", "origin", upstream.to_str().unwrap()],
    )
    .unwrap();
    git(&work, &["push", "-q", "origin", "main"]).unwrap();

    // A *hand-added* remote: `git init` + `git remote add`, never cloned.
    let hand = dir.join("hand");
    git(&dir, &["init", "-q", "hand"]).unwrap();
    git(
        &hand,
        &["remote", "add", "origin", upstream.to_str().unwrap()],
    )
    .unwrap();

    fetch_upstream(&hand, "origin/HEAD").expect("the fetch");
    assert_eq!(default_branch(&hand, "origin").as_deref(), Some("main"));
    // The whole point: the ref every consumer resolves against now exists.
    assert!(
        git(&hand, &["rev-parse", "--verify", "origin/HEAD"]).is_ok(),
        "origin/HEAD must resolve after fetch_upstream"
    );
}

/// A `feature` branch on the scratch repo, and the sha a conversation on it
/// would have recorded.
fn feature_at(repo: &std::path::Path) -> (std::path::PathBuf, String) {
    git(repo, &["branch", "feature"]).unwrap();
    let sha = git(repo, &["rev-parse", "feature"])
        .unwrap()
        .trim()
        .to_string();
    (repo.join(".claude/worktrees/wt"), sha)
}

#[test]
fn head_file_resolves_and_tracks_the_branch_for_main_and_worktrees() {
    let repo = scratch_repo();

    // Main: HEAD is a real file directly under `.git`, and a checkout rewrites
    // its contents — which is exactly the change the poller reads.
    let main_head = head_file(&repo).unwrap();
    assert!(main_head.exists(), "no HEAD at {main_head:?}");
    assert!(main_head.starts_with(&repo) && main_head.ends_with("HEAD"));
    let before = std::fs::read_to_string(&main_head).unwrap();
    git(&repo, &["checkout", "-q", "-b", "other"]).unwrap();
    assert_ne!(before, std::fs::read_to_string(&main_head).unwrap());

    // Linked worktree: `<wt>/.git` is a pointer file, so the real HEAD lives
    // under the common dir — `<wt>/.git/HEAD` does not exist.
    let wt = repo.join(".claude/worktrees/wt");
    std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
    git(
        &repo,
        &["worktree", "add", "-q", "-b", "wt", wt.to_str().unwrap()],
    )
    .unwrap();
    let wt_head = head_file(&wt).unwrap();
    assert!(wt_head.exists(), "no worktree HEAD at {wt_head:?}");
    assert!(!wt.join(".git/HEAD").exists());
    assert!(
        wt_head.to_string_lossy().contains(".git/worktrees/"),
        "worktree HEAD should live under the common dir, got {wt_head:?}"
    );
}

#[test]
fn rebuilds_on_the_branch_that_is_still_there() {
    let repo = scratch_repo();
    let (wt, sha) = feature_at(&repo);
    let moved = worktree_rebuild(&repo, &wt, "feature", &sha).unwrap();
    assert!(
        moved.is_none(),
        "tip matches the record, so nothing to warn about"
    );
    assert!(
        wt.join(".git").exists(),
        "worktree was not created at {wt:?}"
    );
}

#[test]
fn recreates_a_deleted_branch_at_the_recorded_commit() {
    // The merged-and-deleted case (§2 step 1): no ref left, but the commit the
    // conversation ran on is still in the object store.
    let repo = scratch_repo();
    let (wt, sha) = feature_at(&repo);
    git(&repo, &["branch", "-D", "feature"]).unwrap();
    assert!(!branch_exists(&repo, "feature"));

    let moved = worktree_rebuild(&repo, &wt, "feature", &sha).unwrap();
    assert!(
        moved.is_none(),
        "recreated at the recorded commit, so it matches"
    );
    assert!(branch_exists(&repo, "feature"), "branch was not recreated");
    assert_eq!(head_sha(&wt).unwrap(), sha);
}

#[test]
fn reports_the_tip_when_the_branch_moved_on() {
    // §2 step 3: the transcript describes a tree that is no longer checked
    // out, and the caller has to be able to say so.
    let repo = scratch_repo();
    let (wt, recorded) = feature_at(&repo);
    git(&repo, &["switch", "-q", "feature"]).unwrap();
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "later"]).unwrap();
    let tip = git(&repo, &["rev-parse", "feature"])
        .unwrap()
        .trim()
        .to_string();
    git(&repo, &["switch", "-q", "main"]).unwrap();

    let moved = worktree_rebuild(&repo, &wt, "feature", &recorded).unwrap();
    assert_eq!(moved.as_deref(), Some(tip.as_str()));
}

#[test]
fn refuses_when_neither_the_branch_nor_the_commit_survives() {
    let repo = scratch_repo();
    let wt = repo.join(".claude/worktrees/wt");
    let err =
        worktree_rebuild(&repo, &wt, "gone", &"0".repeat(40)).expect_err("nothing to rebuild on");
    assert!(format!("{err:#}").contains("unreachable"), "got: {err:#}");
}

fn rec(parts: &[&str]) -> Vec<u8> {
    let mut v = Vec::new();
    for p in parts {
        v.extend_from_slice(p.as_bytes());
        v.push(0);
    }
    v
}

#[test]
fn splits_staged_and_unstaged_from_one_entry() {
    // XY = "MM": staged modification and a further unstaged one.
    let raw = rec(&["1 MM N... 100644 100644 100644 aaa bbb src/Foo.php"]);
    let set = parse_status(&raw, None);
    assert_eq!(set.staged.len(), 1);
    assert_eq!(set.unstaged.len(), 1);
    assert_eq!(set.staged[0].path, "src/Foo.php");
}

#[test]
fn a_staged_only_entry_does_not_appear_as_unstaged() {
    let raw = rec(&["1 M. N... 100644 100644 100644 aaa bbb src/Foo.php"]);
    let set = parse_status(&raw, None);
    assert_eq!(set.staged.len(), 1);
    assert!(set.unstaged.is_empty());
}

#[test]
fn consumes_the_original_path_of_a_rename() {
    let raw = rec(&[
        "2 R. N... 100644 100644 100644 aaa bbb R100 src/New.php",
        "src/Old.php",
        "? untracked.txt",
    ]);
    let set = parse_status(&raw, None);
    assert_eq!(set.staged.len(), 1);
    assert_eq!(set.staged[0].path, "src/New.php");
    // The old path must not be read back as an entry of its own.
    assert_eq!(set.untracked.len(), 1);
    assert_eq!(set.untracked[0].path, "untracked.txt");
}

#[test]
fn excludes_sibling_worktrees_from_mains_view() {
    let raw = rec(&["? .claude/worktrees/other/file.php", "? src/Mine.php"]);
    let set = parse_status(&raw, Some(".claude/worktrees/"));
    assert_eq!(set.untracked.len(), 1);
    assert_eq!(set.untracked[0].path, "src/Mine.php");
}

#[test]
fn keeps_worktree_paths_when_not_excluding() {
    let raw = rec(&["? .claude/worktrees/other/file.php"]);
    let set = parse_status(&raw, None);
    assert_eq!(set.untracked.len(), 1);
}

#[test]
fn excludes_the_configured_prefix_not_a_hardcoded_one() {
    // A repo whose worktrees live under a different subdir excludes *that*,
    // and leaves the old default's path alone.
    let raw = rec(&["? .worktrees/other/file.php", "? .claude/worktrees/x.php"]);
    let set = parse_status(&raw, Some(".worktrees/"));
    assert_eq!(set.untracked.len(), 1);
    assert_eq!(set.untracked[0].path, ".claude/worktrees/x.php");
}

/// One commit carrying `f.txt`, which the ancestry and blame tests read back.
fn scratch_repo() -> std::path::PathBuf {
    let dir = crate::testutil::scratch("git");
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .expect("git");
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "t@t"]);
    run(&["config", "user.name", "t"]);
    std::fs::write(dir.join("f.txt"), "one\n").unwrap();
    run(&["add", "-A"]);
    run(&["commit", "-qm", "one"]);
    // Resolved, because git reports resolved paths and a test comparing its
    // output against this one has to agree with it. `$TMPDIR` on macOS is
    // under `/var`, which is a symlink into `/private`, so unresolved it
    // matched nothing there — while on Linux `/tmp` is a real directory and
    // the difference never showed.
    std::fs::canonicalize(&dir).unwrap_or(dir)
}

#[test]
fn cuts_a_new_worktree_at_a_custom_subdir() {
    // The daemon's own creation path, used when worktrees do not live where
    // `claude --worktree` would put them — the case where delegating created
    // the worktree somewhere the daemon never looked.
    let repo = scratch_repo();
    let wt = repo.join(".worktrees/inv");
    std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
    worktree_add_new(&repo, &wt, "worktree-inv", "main").expect("worktree add");

    assert!(wt.join("f.txt").exists(), "the worktree is checked out");
    assert_eq!(current_branch(&wt).unwrap(), "worktree-inv");
    // Re-cutting the same branch must refuse rather than silently reuse it.
    let again = repo.join(".worktrees/inv2");
    assert!(worktree_add_new(&repo, &again, "worktree-inv", "main").is_err());
    // A base that does not resolve must not quietly cut from HEAD.
    let bad = repo.join(".worktrees/bad");
    assert!(worktree_add_new(&repo, &bad, "worktree-bad", "origin/nope").is_err());
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn removes_a_worktree_stale_locked_by_a_dead_claude() {
    // The finding the review fixture surfaced: `claude --worktree` locks every
    // worktree it cuts, and the lock outlives the session the daemon kills, so
    // a plain `git worktree remove` refuses forever. Teardown's preflight has
    // already proven the tree clean and no session live, so a lock whose pid
    // is dead is stale — clearing it is not `--force`.
    let repo = scratch_repo();
    let wt = repo.join(".claude/worktrees/wt");
    std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
    worktree_add_new(&repo, &wt, "worktree-wt", "main").expect("worktree add");

    // pid 2^31-ish is not in the table — the closest a test can get to
    // "claude died" without racing a real one. Mirrors claude's own reason
    // string so the parser is exercised on the real shape.
    let reason = "claude session wt (pid 2147480000 start 1)";
    git(
        &repo,
        &["worktree", "lock", "--reason", reason, wt.to_str().unwrap()],
    )
    .expect("lock");
    assert!(stale_lock_pid(&repo, &wt).is_some(), "the lock pid parses");

    worktree_remove(&repo, &wt).expect("remove clears the stale lock");
    assert!(!wt.exists(), "the worktree is gone from disk");
    assert!(
        !git(&repo, &["worktree", "list", "--porcelain"])
            .unwrap()
            .contains("worktrees/wt"),
        "and gone from git's record"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// The same stale lock, but named through a symlink — because `git worktree
/// list` answers with the *real* path and the caller's may not be one. This is
/// what failed on the macOS runner while passing on Linux: `$TMPDIR` is under
/// `/var`, a symlink into `/private`, so the string compare missed and the
/// lock was never seen as stale. Now that `scratch_repo` hands back a resolved
/// path, this is the only test that still exercises that comparison.
#[test]
fn a_stale_lock_is_found_even_when_the_worktree_is_named_through_a_symlink() {
    let repo = scratch_repo();
    let wt = repo.join(".claude/worktrees/linked");
    std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
    worktree_add_new(&repo, &wt, "worktree-linked", "main").expect("worktree add");
    let reason = "claude session linked (pid 2147480000 start 1)";
    git(
        &repo,
        &["worktree", "lock", "--reason", reason, wt.to_str().unwrap()],
    )
    .expect("lock");

    // A second route to the very same worktree.
    let link = repo.parent().unwrap().join(format!(
        "orchd-wtlink-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&repo, &link).expect("symlink");
    let via_link = link.join(".claude/worktrees/linked");

    assert!(
        stale_lock_pid(&repo, &via_link).is_some(),
        "a symlinked path must still find the lock git reports under its real name"
    );
    worktree_remove(&repo, &via_link).expect("remove through the symlink");
    assert!(!wt.exists(), "the worktree is gone from disk");

    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn keeps_refusing_a_worktree_locked_by_a_live_process() {
    // The safety half: a lock whose owner is alive must still refuse, or the
    // stale-lock path would become a rename for `--force`. Our own pid stands
    // in for a live claude.
    let repo = scratch_repo();
    let wt = repo.join(".claude/worktrees/live");
    std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
    worktree_add_new(&repo, &wt, "worktree-live", "main").expect("worktree add");

    let reason = format!("claude session live (pid {} start 1)", std::process::id());
    git(
        &repo,
        &[
            "worktree",
            "lock",
            "--reason",
            &reason,
            wt.to_str().unwrap(),
        ],
    )
    .expect("lock");

    let err = worktree_remove(&repo, &wt).unwrap_err();
    assert!(
        format!("{err:#}").contains("not escalating to --force"),
        "a live lock must surface, not be cleared: {err:#}"
    );
    assert!(wt.exists(), "the worktree is left in place");
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn adds_a_worktree_on_an_existing_branch() {
    // /resolve has to land on the PR's own head branch, not a fresh one cut
    // from upstream/develop (§8).
    let repo = scratch_repo();
    std::process::Command::new("git")
        .args(["branch", "feature/x"])
        .current_dir(&repo)
        .output()
        .unwrap();

    let wt = repo.join(".claude/worktrees/pr-1");
    std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
    worktree_add_existing(&repo, &wt, "feature/x").expect("worktree add");

    assert!(wt.join("f.txt").exists());
    assert_eq!(current_branch(&wt).unwrap(), "feature/x");
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn refuses_a_branch_that_exists_nowhere() {
    let repo = scratch_repo();
    let wt = repo.join(".claude/worktrees/pr-2");
    std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
    let err = worktree_add_existing(&repo, &wt, "feature/nope").unwrap_err();
    assert!(
        format!("{err:#}").contains("neither locally nor on any remote"),
        "unexpected: {err:#}"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// A fork whose push remote is not named `origin` still resolves a head branch.
///
/// `remote_branch` tries `origin` first for speed, then falls back to every
/// remote. Without the fallback a checkout whose fork remote is called anything
/// else refused every PR head with "exists neither locally nor on origin".
#[test]
fn a_head_branch_on_a_non_origin_remote_still_resolves() {
    let root = std::env::temp_dir().join(format!("orch-forkremote-{}", uuid::Uuid::new_v4()));
    let fork = root.join("fork");
    let repo = root.join("repo");
    std::fs::create_dir_all(&fork).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    let sh = |cwd: &Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    // The fork holds a PR head branch.
    sh(&fork, &["init", "-q", "-b", "develop"]);
    sh(&fork, &["config", "user.email", "t@t"]);
    sh(&fork, &["config", "user.name", "t"]);
    std::fs::write(fork.join("f"), "1").unwrap();
    sh(&fork, &["add", "-A"]);
    sh(&fork, &["commit", "-qm", "base"]);
    sh(&fork, &["branch", "feature/head"]);

    // The checkout knows the fork under a name that is not `origin`.
    sh(&repo, &["init", "-q", "-b", "develop"]);
    sh(&repo, &["config", "user.email", "t@t"]);
    sh(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("f"), "1").unwrap();
    sh(&repo, &["add", "-A"]);
    sh(&repo, &["commit", "-qm", "base"]);
    sh(&repo, &["remote", "add", "mine", &fork.to_string_lossy()]);

    let wt = repo.join(".claude/worktrees/pr-1");
    std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
    worktree_add_existing(&repo, &wt, "feature/head").unwrap();
    assert_eq!(current_branch(&wt).unwrap(), "feature/head");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn unpushed_commits_block_teardown() {
    assert!(Unpushed::NeverPushed {
        commits: vec!["abc work".into()]
    }
    .blocks_teardown());
    assert!(Unpushed::Ahead {
        commits: vec!["abc x".into()]
    }
    .blocks_teardown());
    assert!(!Unpushed::UpToDate.blocks_teardown());
}

#[test]
fn a_fresh_worktree_carrying_nothing_can_still_be_removed() {
    // Branched straight off upstream/develop and never pushed: there is no
    // work to lose, so teardown must not be blocked forever.
    assert!(!Unpushed::NeverPushed { commits: vec![] }.blocks_teardown());
}
/// A repo with two commits so blame has something to distinguish, plus a
/// `base` ref standing in for the merge base.
fn amend_repo() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "orchd-amend-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .expect("git");
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "me@here"]);
    run(&["config", "user.name", "me"]);
    // Commit 1 stands in for the base branch's history.
    std::fs::write(dir.join("f.txt"), "base1\nbase2\n").unwrap();
    run(&["add", "-A"]);
    run(&["commit", "-qm", "from develop"]);
    run(&["branch", "base"]);
    // Commit 2 is the PR's own.
    std::fs::write(dir.join("f.txt"), "base1\nbase2\nmine\n").unwrap();
    run(&["add", "-A"]);
    run(&["commit", "-qm", "the PR commit"]);
    dir
}

#[test]
fn a_push_to_the_base_branch_is_refused_before_it_runs() {
    // The agent-side guard only hooks Bash; a daemon push bypasses it.
    let d = amend_repo();
    let err = push_with_lease(&d, "trunk", Some("trunk"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("refusing to push"), "{err}");
    // The list used to be four hardcoded names, so this pair was backwards:
    // `trunk` sailed through and `release` was refused for its name alone.
    // Only a real push attempt gets past the check, so the error is git's.
    let err = push_with_lease(&d, "release", Some("trunk"))
        .unwrap_err()
        .to_string();
    assert!(!err.contains("refusing to push"), "{err}");
    // No resolvable base refuses nothing here either.
    let err = push_with_lease(&d, "trunk", None).unwrap_err().to_string();
    assert!(!err.contains("refusing to push"), "{err}");
}

/// The lease refusal is classified from git's wording, so the wording is pinned:
/// a hook declining or a protected branch also say `rejected`, and neither means
/// the remote moved.
#[test]
fn only_a_moved_remote_reads_as_a_lease_refusal() {
    assert!(lease_refused(
        " ! [rejected]        feature -> feature (stale info)\nerror: failed to push some refs"
    ));
    assert!(lease_refused(
        " ! [rejected]        feature -> feature (fetch first)"
    ));
    assert!(lease_refused(
        " ! [rejected]        feature -> feature (non-fast-forward)"
    ));
    assert!(!lease_refused(
        " ! [remote rejected] feature -> feature (pre-receive hook declined)"
    ));
    assert!(!lease_refused(
        " ! [remote rejected] main -> main (protected branch hook declined)"
    ));
    assert!(!lease_refused(
        "fatal: could not read from remote repository"
    ));
}
