//! Integration tests for `apply_plan` — the function a `--apply` CLI flag
//! would wrap to execute a full tree-rebase plan against a real repository.
//!
//! Organized by tier:
//!   A. cascade correctness — happy path, linear / parallel / mixed trees.
//!   B. conflict + resume — halts cleanly mid-plan, replan-after-resolve.
//!   C. safety guards — refuses on mid-rebase / dirty tree; no-op when ready.
//!   D. predictive parity — predicted_conflict matches actual outcome.

use gitui::testing::{run_git, setup_repo};
use gitui::{OpResult, RepoState, apply_plan, build_cascade_plan, repo_status};
use std::path::Path;
use tempfile::TempDir;

// ============================================================================
// Fixture
// ============================================================================

/// Holds the TempDirs (which must outlive the test) plus accessors.
struct Fixture {
    _main_dir: TempDir,
    _origin_dir: TempDir,
    _upstream_dir: TempDir,
    main_branch: String,
    path: String,
}

impl Fixture {
    fn p(&self) -> &Path {
        Path::new(&self.path)
    }
}

/// Set up: main repo + bare origin + bare upstream, main branch tracking
/// origin/main, both remotes seeded with the initial commit.
fn setup_with_remotes() -> Fixture {
    let (main_dir, _repo, branch_name) = setup_repo();
    let origin_dir = tempfile::tempdir().unwrap();
    let upstream_dir = tempfile::tempdir().unwrap();
    let path = main_dir.path().to_str().unwrap().to_string();

    run_git(origin_dir.path(), &["init", "--bare", "-q"]);
    run_git(upstream_dir.path(), &["init", "--bare", "-q"]);
    run_git(
        &path,
        &[
            "remote",
            "add",
            "origin",
            origin_dir.path().to_str().unwrap(),
        ],
    );
    run_git(
        &path,
        &[
            "remote",
            "add",
            "upstream",
            upstream_dir.path().to_str().unwrap(),
        ],
    );
    run_git(
        &path,
        &["push", "-q", "origin", &format!("HEAD:{}", branch_name)],
    );
    run_git(
        &path,
        &["push", "-q", "upstream", &format!("HEAD:{}", branch_name)],
    );
    run_git(
        &path,
        &[
            "branch",
            &format!("--set-upstream-to=origin/{}", branch_name),
            &branch_name,
        ],
    );

    Fixture {
        _main_dir: main_dir,
        _origin_dir: origin_dir,
        _upstream_dir: upstream_dir,
        main_branch: branch_name,
        path,
    }
}

/// Make a new commit on the current branch by writing `content` to `file`.
fn commit_file(path: &str, file: &str, content: &str, msg: &str) -> String {
    std::fs::write(format!("{}/{}", path, file), content).unwrap();
    run_git(path, &["add", file]);
    run_git(path, &["commit", "-q", "-m", msg]);
    run_git(path, &["rev-parse", "HEAD"])
}

/// Push a commit to upstream/main, then rewind local main back to its prior
/// position so the local view is "behind upstream". Leaves working tree clean
/// and the upstream remote-tracking ref updated.
fn advance_upstream(fx: &Fixture, file: &str, content: &str, msg: &str) -> String {
    run_git(&fx.path, &["checkout", "-q", &fx.main_branch]);
    let pre = run_git(&fx.path, &["rev-parse", "HEAD"]);
    commit_file(&fx.path, file, content, msg);
    run_git(
        &fx.path,
        &[
            "push",
            "-q",
            "upstream",
            &format!("HEAD:{}", fx.main_branch),
        ],
    );
    let new_upstream = run_git(&fx.path, &["rev-parse", "HEAD"]);
    run_git(&fx.path, &["reset", "--hard", "-q", &pre]);
    new_upstream
}

/// Create branch `name` off `parent` with a single commit touching `file`.
fn stack_branch(fx: &Fixture, name: &str, parent: &str, file: &str, content: &str) {
    run_git(&fx.path, &["checkout", "-q", parent]);
    run_git(&fx.path, &["checkout", "-q", "-b", name]);
    commit_file(&fx.path, file, content, &format!("commit on {}", name));
}

/// Variant of `setup_with_remotes` whose main branch is named "master".
/// setup_repo() always returns "main"; this renames post-setup so we can
/// validate that the planner's `name == "master" || name == "main"` OR
/// check actually works in practice for both names.
fn setup_with_remotes_master() -> Fixture {
    let mut fx = setup_with_remotes();
    let old = fx.main_branch.clone();
    run_git(&fx.path, &["branch", "-m", &old, "master"]);
    run_git(&fx.path, &["push", "-q", "origin", "HEAD:master"]);
    run_git(&fx.path, &["push", "-q", "upstream", "HEAD:master"]);
    run_git(&fx.path, &["push", "-q", "origin", "--delete", &old]);
    run_git(&fx.path, &["push", "-q", "upstream", "--delete", &old]);
    run_git(
        &fx.path,
        &["branch", "--set-upstream-to=origin/master", "master"],
    );
    fx.main_branch = "master".to_string();
    fx
}

fn tip(path: &str, branch: &str) -> String {
    run_git(path, &["rev-parse", branch])
}

fn merge_base(path: &str, a: &str, b: &str) -> String {
    run_git(path, &["merge-base", a, b])
}

fn is_ancestor(path: &str, ancestor: &str, descendant: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .status()
        .unwrap()
        .success()
}

/// Assert every OpResult in `results` is Ok; print plan on failure.
fn assert_all_ok(results: &[OpResult]) {
    for (i, r) in results.iter().enumerate() {
        if !matches!(r, OpResult::Ok { .. }) {
            panic!("op {} expected Ok, got {:?} (full: {:?})", i, r, results);
        }
    }
}

// ============================================================================
// Tier A: cascade correctness
// ============================================================================

#[test]
fn tier_a_linear_stack_cascade() {
    let fx = setup_with_remotes();
    // main ← A ← B ← C
    stack_branch(&fx, "A", &fx.main_branch, "a.txt", "A");
    stack_branch(&fx, "B", "A", "b.txt", "B");
    stack_branch(&fx, "C", "B", "c.txt", "C");

    let new_upstream = advance_upstream(&fx, "u.txt", "u", "upstream commit");
    assert_eq!(
        new_upstream,
        tip(&fx.path, &format!("upstream/{}", fx.main_branch))
    );

    let plan = build_cascade_plan(fx.p(), &fx.main_branch).unwrap();
    assert!(!plan.ops.is_empty(), "expected plan, got empty");

    let results = apply_plan(fx.p(), &plan).unwrap();
    assert_all_ok(&results);

    // Every branch now has the new upstream commit in its ancestry.
    for branch in &[fx.main_branch.as_str(), "A", "B", "C"] {
        assert!(
            is_ancestor(&fx.path, &new_upstream, branch),
            "branch {} should contain new upstream commit {} in its ancestry",
            branch,
            new_upstream
        );
    }

    // Stack structure preserved: A on top of main, B on A, C on B.
    assert_eq!(
        merge_base(&fx.path, "A", &fx.main_branch),
        tip(&fx.path, &fx.main_branch)
    );
    assert_eq!(merge_base(&fx.path, "B", "A"), tip(&fx.path, "A"));
    assert_eq!(merge_base(&fx.path, "C", "B"), tip(&fx.path, "B"));
}

#[test]
fn tier_a_parallel_children_cascade() {
    let fx = setup_with_remotes();
    // main ← {A, B, C}
    stack_branch(&fx, "A", &fx.main_branch, "a.txt", "A");
    stack_branch(&fx, "B", &fx.main_branch, "b.txt", "B");
    stack_branch(&fx, "C", &fx.main_branch, "c.txt", "C");

    let new_upstream = advance_upstream(&fx, "u.txt", "u", "upstream commit");

    let plan = build_cascade_plan(fx.p(), &fx.main_branch).unwrap();
    let results = apply_plan(fx.p(), &plan).unwrap();
    assert_all_ok(&results);

    // All three feature branches sit directly on the new main tip.
    let main_tip = tip(&fx.path, &fx.main_branch);
    for branch in &["A", "B", "C"] {
        assert!(
            is_ancestor(&fx.path, &new_upstream, branch),
            "{} should contain new upstream",
            branch
        );
        assert_eq!(
            merge_base(&fx.path, branch, &fx.main_branch),
            main_tip,
            "{} should be on top of new main tip",
            branch
        );
    }
}

#[test]
fn tier_a_mixed_tree_cascade() {
    let fx = setup_with_remotes();
    // main ← A ← {B, C}; main ← D
    stack_branch(&fx, "A", &fx.main_branch, "a.txt", "A");
    stack_branch(&fx, "B", "A", "b.txt", "B");
    stack_branch(&fx, "C", "A", "c.txt", "C");
    stack_branch(&fx, "D", &fx.main_branch, "d.txt", "D");

    let new_upstream = advance_upstream(&fx, "u.txt", "u", "upstream commit");

    let plan = build_cascade_plan(fx.p(), &fx.main_branch).unwrap();
    let results = apply_plan(fx.p(), &plan).unwrap();
    assert_all_ok(&results);

    let main_tip = tip(&fx.path, &fx.main_branch);
    let a_tip = tip(&fx.path, "A");

    // main and A and D all contain the new upstream
    for branch in &[fx.main_branch.as_str(), "A", "B", "C", "D"] {
        assert!(
            is_ancestor(&fx.path, &new_upstream, branch),
            "{} should contain new upstream",
            branch
        );
    }
    // A is on new main
    assert_eq!(merge_base(&fx.path, "A", &fx.main_branch), main_tip);
    // B and C are on new A
    assert_eq!(merge_base(&fx.path, "B", "A"), a_tip);
    assert_eq!(merge_base(&fx.path, "C", "A"), a_tip);
    // D is on new main (not on A)
    assert_eq!(merge_base(&fx.path, "D", &fx.main_branch), main_tip);
}

#[test]
fn tier_a_parity_with_master_named_branch() {
    // All other tests use `main` (setup_repo default). c-toxcore uses
    // `master`. The planner has an OR check so both should work, but lock
    // it in with one explicit test that exercises the master path end-to-end.
    let fx = setup_with_remotes_master();
    assert_eq!(fx.main_branch, "master");
    stack_branch(&fx, "A", &fx.main_branch, "a.txt", "A");
    stack_branch(&fx, "B", "A", "b.txt", "B");

    let new_upstream = advance_upstream(&fx, "u.txt", "u", "upstream commit");

    let plan = build_cascade_plan(fx.p(), &fx.main_branch).unwrap();
    assert!(
        !plan.ops.is_empty(),
        "expected non-empty plan for master-named main branch; got {:?}",
        plan
    );

    let results = apply_plan(fx.p(), &plan).unwrap();
    assert_all_ok(&results);

    for branch in &["master", "A", "B"] {
        assert!(
            is_ancestor(&fx.path, &new_upstream, branch),
            "{} should contain upstream commit",
            branch
        );
    }
    assert_eq!(merge_base(&fx.path, "A", "master"), tip(&fx.path, "master"));
    assert_eq!(merge_base(&fx.path, "B", "A"), tip(&fx.path, "A"));
}

#[test]
fn tier_a_multi_commit_branches_preserved() {
    // Real branches typically have multiple commits, not one. Verify all
    // commits are preserved through the cascade rebase and end up in order.
    let fx = setup_with_remotes();
    run_git(&fx.path, &["checkout", "-q", &fx.main_branch]);
    run_git(&fx.path, &["checkout", "-q", "-b", "A"]);
    commit_file(&fx.path, "a1.txt", "a1", "A commit 1");
    commit_file(&fx.path, "a2.txt", "a2", "A commit 2");
    commit_file(&fx.path, "a3.txt", "a3", "A commit 3");
    run_git(&fx.path, &["checkout", "-q", "-b", "B"]);
    commit_file(&fx.path, "b1.txt", "b1", "B commit 1");
    commit_file(&fx.path, "b2.txt", "b2", "B commit 2");

    let new_upstream = advance_upstream(&fx, "u.txt", "u", "upstream commit");

    let plan = build_cascade_plan(fx.p(), &fx.main_branch).unwrap();
    let results = apply_plan(fx.p(), &plan).unwrap();
    assert_all_ok(&results);

    // A still has its 3 commits, B still has its 2 — verify via commit count
    // between branch tip and main tip after the cascade.
    let main_tip = tip(&fx.path, &fx.main_branch);
    let a_count = run_git(
        &fx.path,
        &["rev-list", "--count", &format!("{}..A", main_tip)],
    );
    let b_count = run_git(&fx.path, &["rev-list", "--count", "A..B"]);
    assert_eq!(a_count, "3", "A should still have 3 commits past main");
    assert_eq!(b_count, "2", "B should still have 2 commits past A");

    // And the upstream commit is in everyone's ancestry.
    for branch in &[fx.main_branch.as_str(), "A", "B"] {
        assert!(is_ancestor(&fx.path, &new_upstream, branch));
    }
}

#[test]
fn tier_a_branch_names_with_slashes() {
    // c-toxcore has branches like `refactor/buffered-events`. Make sure
    // `/`-containing branch names round-trip through planner + executor.
    let fx = setup_with_remotes();
    stack_branch(&fx, "feature/foo", &fx.main_branch, "f.txt", "foo");
    stack_branch(&fx, "feature/foo-child", "feature/foo", "fc.txt", "child");

    let new_upstream = advance_upstream(&fx, "u.txt", "u", "upstream commit");

    let plan = build_cascade_plan(fx.p(), &fx.main_branch).unwrap();
    let results = apply_plan(fx.p(), &plan).unwrap();
    assert_all_ok(&results);

    for branch in &[fx.main_branch.as_str(), "feature/foo", "feature/foo-child"] {
        assert!(
            is_ancestor(&fx.path, &new_upstream, branch),
            "{} should contain upstream commit",
            branch
        );
    }
    assert_eq!(
        merge_base(&fx.path, "feature/foo-child", "feature/foo"),
        tip(&fx.path, "feature/foo"),
        "slash-named child should be stacked on slash-named parent"
    );
}

#[test]
fn tier_a_un_anchored_branches_reported_in_skipped() {
    // Build a branch with no detectable parent at all (no topological
    // ancestor that is a branch tip AND no commit-summary match to another
    // branch). The planner should not silently drop it — it should appear
    // in `Plan.skipped_branches` so the CLI can warn.
    let fx = setup_with_remotes();
    stack_branch(&fx, "Normal", &fx.main_branch, "n.txt", "normal");

    // Create a true orphan: --orphan starts a branch with no parent commit.
    // Clean both the index and the working tree so the orphan doesn't leave
    // Normal's files lingering as untracked, which would conflict later when
    // we rebase Normal back onto the new main tip.
    run_git(&fx.path, &["checkout", "-q", "--orphan", "Loner"]);
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(&fx.path)
        .args(["rm", "-rf", "--cached", "."])
        .status();
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(&fx.path)
        .args(["clean", "-fdx"])
        .status();
    std::fs::write(format!("{}/loner.txt", fx.path), "lonely content").unwrap();
    run_git(&fx.path, &["add", "loner.txt"]);
    run_git(
        &fx.path,
        &["commit", "-q", "-m", "Loner orphan commit unique xyz"],
    );

    let _ = advance_upstream(&fx, "u.txt", "u", "upstream commit");
    run_git(&fx.path, &["checkout", "-q", &fx.main_branch]);
    // Also clean lingering orphan files (loner.txt is untracked on main now).
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(&fx.path)
        .args(["clean", "-fdx"])
        .status();

    let plan = build_cascade_plan(fx.p(), &fx.main_branch).unwrap();

    assert!(
        plan.skipped_branches.iter().any(|n| n == "Loner"),
        "Loner should appear in skipped_branches; got skipped: {:?}\nops: {:?}",
        plan.skipped_branches,
        plan.ops
    );

    // Normal branch is anchored to main and still gets rebased.
    use gitui::engine::Operation;
    assert!(
        plan.ops.iter().any(|op| matches!(
            op,
            Operation::Rebase { branch, .. } if branch == "Normal"
        )),
        "Normal branch should still appear in ops: {:?}",
        plan.ops
    );
}

#[test]
fn tier_b_reanchors_when_topology_parent_is_ancestor_of_sync_target() {
    // Reproduction of a planner heuristic gap. The topology resolver walks
    // a branch's commit ancestry for the nearest branch-tip ref to call its
    // "parent." If that resolved parent happens to be at a commit that is
    // *already an ancestor* of the sync target (e.g., the user has a
    // perfectly normal branch at an older commit, like `origin/clog` in
    // c-toxcore), the planner sees `parent_oid == merge_base_with_branch`
    // and concludes the branch is "already on its parent" — emitting no
    // rebase op. The branch silently drops out of the cascade.
    //
    // For `--sync <target>`, the user's intent is for everything to end up
    // rebased onto the new target. Whenever the topology hands us a parent
    // whose tip is an ancestor of the sync target, re-anchor to the target.
    // The branch name itself isn't stale — the topology choice is.
    let fx = setup_with_remotes();

    // Push origin/shadow at the initial commit (C0), so origin has a
    // dangling remote-tracking ref at a commit that will end up an
    // ancestor of main.
    run_git(&fx.path, &["push", "-q", "origin", "HEAD:shadow"]);

    // Advance main with C1.
    run_git(&fx.path, &["checkout", "-q", &fx.main_branch]);
    commit_file(&fx.path, "main_progress.txt", "C1", "Advance main");
    run_git(&fx.path, &["push", "-q", "origin", &fx.main_branch]);
    run_git(&fx.path, &["push", "-q", "upstream", &fx.main_branch]);

    // Create Sib1 off main at C1.
    stack_branch(&fx, "Sib1", &fx.main_branch, "s1.txt", "s1");

    // Advance upstream to C2.
    advance_upstream(&fx, "u.txt", "u", "upstream new commit");

    // Manually sync main to upstream (= C2). Sib1 is at C1+1; main is
    // past C1; origin/shadow is at C0. The walker from Sib1 finds
    // origin/shadow as the nearest tip in ancestry.
    run_git(&fx.path, &["checkout", "-q", &fx.main_branch]);
    run_git(
        &fx.path,
        &[
            "merge",
            "--ff-only",
            "-q",
            &format!("upstream/{}", fx.main_branch),
        ],
    );
    run_git(&fx.path, &["push", "-q", "origin", &fx.main_branch]);

    let plan = build_cascade_plan(fx.p(), &fx.main_branch).unwrap();

    let diag = {
        use gitui::engine::{Git, RealGit};
        let git = RealGit::new(&fx.path).unwrap();
        let (branches, _hist) = git.get_branches(None).unwrap();
        let mut s = String::new();
        for b in &branches {
            if !b.is_remote {
                s.push_str(&format!(
                    "  {} oid={} parent={:?} heuristic={:?}\n",
                    b.name,
                    &b.oid.to_string()[..8],
                    b.original_parent,
                    b.heuristic_parent
                ));
            }
        }
        s
    };

    use gitui::engine::Operation;
    let sib1_op = plan.ops.iter().find_map(|op| match op {
        Operation::Rebase {
            branch,
            onto,
            upstream,
            predicted_conflict,
        } if branch == "Sib1" => Some((onto.clone(), upstream.clone(), *predicted_conflict)),
        _ => None,
    });
    let (sib1_onto, sib1_upstream, sib1_predicted) = sib1_op.unwrap_or_else(|| {
        panic!(
            "Sib1 should be rebased\nplan.ops: {:?}\nplan.skipped_branches: {:?}\nbranches:\n{}",
            plan.ops, plan.skipped_branches, diag
        )
    });

    assert_eq!(
        sib1_onto, fx.main_branch,
        "Sib1 should be rebased onto main, not onto another ref;\nplan.ops: {:?}",
        plan.ops
    );

    // The rebase boundary (`upstream` parameter passed to `git rebase --onto`)
    // must NOT be the oid of the older anchor. If it is, the rebase will try
    // to replay every commit between that anchor and Sib1's tip — including
    // commits that are already in main's history — which causes the conflict
    // predictor to fire false positives and the executor to do unnecessary
    // "patch already applied" work. Correct behavior: the boundary should be
    // either None (let git compute merge-base) or the merge-base oid itself.
    let main_tip = tip(&fx.path, &fx.main_branch);
    let mb_sib1_main = merge_base(&fx.path, "Sib1", &fx.main_branch);
    if let Some(u) = &sib1_upstream {
        assert!(
            u == &main_tip || u == &mb_sib1_main,
            "Sib1's rebase upstream should be either main's tip or the merge-base,\n\
             got: {}\nmain_tip: {}\nmerge_base(Sib1, main): {}\nplan.ops: {:?}",
            u,
            main_tip,
            mb_sib1_main,
            plan.ops
        );
    }

    assert_ne!(
        sib1_predicted,
        Some(true),
        "Sib1's rebase should NOT be predicted as conflicting (the change is\n\
         a single new commit on top of an in-master ancestor; merging it onto\n\
         the new main tip is clean); plan.ops: {:?}",
        plan.ops
    );

    // Apply and confirm Sib1 ends up with exactly one commit past new main.
    let results = apply_plan(fx.p(), &plan).unwrap();
    assert_all_ok(&results);
    let new_main_tip = tip(&fx.path, &fx.main_branch);
    assert_eq!(
        merge_base(&fx.path, "Sib1", &fx.main_branch),
        new_main_tip,
        "Sib1 should sit directly on top of new main tip"
    );
    let commits_ahead = run_git(
        &fx.path,
        &["rev-list", "--count", &format!("{}..Sib1", &fx.main_branch)],
    );
    assert_eq!(
        commits_ahead, "1",
        "Sib1 should have exactly 1 commit past new main, not redoing in-master history"
    );
}

#[test]
fn tier_b_detects_stale_siblings_when_some_already_rebased() {
    // Closer reproduction of the c-toxcore stall: master is synced AND some
    // siblings have already been rebased (audio-opt, clock in real-world);
    // OTHER siblings (coverage-sources etc.) are still on old master and
    // need rebasing. The planner must still emit ops for the stale ones.
    let fx = setup_with_remotes();
    stack_branch(&fx, "Done1", &fx.main_branch, "d1.txt", "d1");
    stack_branch(&fx, "Done2", &fx.main_branch, "d2.txt", "d2");
    stack_branch(&fx, "Stale1", &fx.main_branch, "s1.txt", "s1");
    stack_branch(&fx, "Stale2", &fx.main_branch, "s2.txt", "s2");

    advance_upstream(&fx, "u.txt", "u", "upstream commit");

    // Capture old-master oid so we can rebase the "Done" branches off of it
    // onto new master, the same way an --apply cascade would have.
    let main_old_oid = run_git(&fx.path, &["merge-base", &fx.main_branch, "Done1"]);

    // Sync master.
    run_git(&fx.path, &["checkout", "-q", &fx.main_branch]);
    run_git(
        &fx.path,
        &[
            "merge",
            "--ff-only",
            "-q",
            &format!("upstream/{}", fx.main_branch),
        ],
    );
    run_git(&fx.path, &["push", "-q", "origin", &fx.main_branch]);

    // Rebase Done1 and Done2 manually (mimicking the partial cascade).
    for b in &["Done1", "Done2"] {
        run_git(
            &fx.path,
            &["rebase", "--onto", &fx.main_branch, &main_old_oid, b],
        );
    }
    run_git(&fx.path, &["checkout", "-q", &fx.main_branch]);

    let plan = build_cascade_plan(fx.p(), &fx.main_branch).unwrap();

    let diag = {
        use gitui::engine::{Git, RealGit};
        let git = RealGit::new(&fx.path).unwrap();
        let (branches, _hist) = git.get_branches(None).unwrap();
        let mut s = String::new();
        for b in &branches {
            if !b.is_remote {
                s.push_str(&format!(
                    "  {} oid={} parent={:?} heuristic={:?}\n",
                    b.name,
                    &b.oid.to_string()[..8],
                    b.original_parent,
                    b.heuristic_parent
                ));
            }
        }
        s
    };

    use gitui::engine::Operation;
    for stale in &["Stale1", "Stale2"] {
        let in_ops = plan
            .ops
            .iter()
            .any(|op| matches!(op, Operation::Rebase { branch, .. } if branch == stale));
        let in_skipped = plan.skipped_branches.iter().any(|n| n == stale);
        assert!(
            in_ops || in_skipped,
            "expected {} to be planned or skipped\n\
             plan.ops: {:?}\n\
             plan.skipped_branches: {:?}\n\
             branch state:\n{}",
            stale,
            plan.ops,
            plan.skipped_branches,
            diag
        );
    }
}

#[test]
fn tier_b_detects_stale_siblings_after_partial_cascade() {
    // Reproduction of a stall we hit on a real repo: master got synced (by
    // a previous --apply run that halted on a conflict in one descendant),
    // some siblings of master got rebased, the rest did NOT. Re-running
    // build_cascade_plan should still emit ops for the un-rebased siblings.
    //
    // Diagnostics inline so failures show what the planner sees.
    let fx = setup_with_remotes();
    stack_branch(&fx, "Sib1", &fx.main_branch, "s1.txt", "s1");
    stack_branch(&fx, "Sib2", &fx.main_branch, "s2.txt", "s2");
    stack_branch(&fx, "Sib3", &fx.main_branch, "s3.txt", "s3");

    advance_upstream(&fx, "u.txt", "u", "upstream commit");

    // Mimic the post-Sync state: master moved to upstream/main, but none
    // of the siblings were touched. (Equivalent to the c-toxcore situation
    // after an --apply that halted somewhere mid-cascade, except even more
    // pristine — none of the siblings rebased yet.)
    run_git(&fx.path, &["checkout", "-q", &fx.main_branch]);
    run_git(
        &fx.path,
        &[
            "merge",
            "--ff-only",
            "-q",
            &format!("upstream/{}", fx.main_branch),
        ],
    );
    run_git(&fx.path, &["push", "-q", "origin", &fx.main_branch]);

    let plan = build_cascade_plan(fx.p(), &fx.main_branch).unwrap();

    // Dump branch state for diagnostics on failure.
    let diag = {
        use gitui::engine::{Git, RealGit};
        let git = RealGit::new(&fx.path).unwrap();
        let (branches, _hist) = git.get_branches(None).unwrap();
        let mut s = String::new();
        for b in &branches {
            if !b.is_remote {
                s.push_str(&format!(
                    "  {} oid={} parent={:?} heuristic={:?}\n",
                    b.name,
                    &b.oid.to_string()[..8],
                    b.original_parent,
                    b.heuristic_parent
                ));
            }
        }
        s
    };

    use gitui::engine::Operation;
    for sib in &["Sib1", "Sib2", "Sib3"] {
        let in_ops = plan
            .ops
            .iter()
            .any(|op| matches!(op, Operation::Rebase { branch, .. } if branch == sib));
        let in_skipped = plan.skipped_branches.iter().any(|n| n == sib);
        assert!(
            in_ops || in_skipped,
            "expected {} to appear in plan.ops or plan.skipped_branches\n\
             plan.ops: {:?}\n\
             plan.skipped_branches: {:?}\n\
             branch state:\n{}",
            sib,
            plan.ops,
            plan.skipped_branches,
            diag
        );
    }
}

// ============================================================================
// Tier B: conflict + resume
// ============================================================================

/// Build a 3-deep stack where the middle branch will conflict with an
/// advanced upstream. Returns the conflicting file's path-fragment.
///
/// Setup:
///   - initial commit creates conflict.txt with "v0"
///   - upstream/main moves it to "v-upstream"
///   - A modifies an unrelated file (no conflict)
///   - B modifies conflict.txt to "v-local" (will conflict on rebase to new main)
///   - C modifies a third unrelated file (no conflict in isolation, but
///     execution stops before C is reached)
fn setup_midstack_conflict(fx: &Fixture) {
    // Establish conflict.txt = v0 in the initial state, on main, pushed
    // to both remotes so it's the shared baseline.
    run_git(&fx.path, &["checkout", "-q", &fx.main_branch]);
    commit_file(&fx.path, "conflict.txt", "v0\n", "Add conflict.txt v0");
    run_git(
        &fx.path,
        &["push", "-q", "origin", &format!("HEAD:{}", fx.main_branch)],
    );
    run_git(
        &fx.path,
        &[
            "push",
            "-q",
            "upstream",
            &format!("HEAD:{}", fx.main_branch),
        ],
    );

    // A: unrelated change
    stack_branch(fx, "A", &fx.main_branch, "a.txt", "A content");
    // B (off A): conflicts on conflict.txt
    run_git(&fx.path, &["checkout", "-q", "-b", "B"]);
    commit_file(
        &fx.path,
        "conflict.txt",
        "v-local\n",
        "B changes conflict.txt",
    );
    // C (off B): unrelated change
    run_git(&fx.path, &["checkout", "-q", "-b", "C"]);
    commit_file(&fx.path, "c.txt", "C content", "C unrelated");

    // Advance upstream so it conflicts with B's change.
    advance_upstream(
        fx,
        "conflict.txt",
        "v-upstream\n",
        "upstream changes conflict.txt",
    );
}

#[test]
fn tier_b_conflict_mid_cascade_halts_cleanly() {
    let fx = setup_with_remotes();
    setup_midstack_conflict(&fx);

    let plan = build_cascade_plan(fx.p(), &fx.main_branch).unwrap();
    let results = apply_plan(fx.p(), &plan).unwrap();

    // Find where the cascade stopped.
    let last = results.last().expect("should have at least one result");
    let conflict = match last {
        OpResult::Conflict {
            branch,
            conflicted_paths,
            rebase_in_progress,
        } => {
            assert_eq!(branch, "B", "expected halt on B, got on {}", branch);
            assert!(*rebase_in_progress, "rebase should be in progress");
            assert!(
                conflicted_paths.iter().any(|p| p == "conflict.txt"),
                "expected conflict.txt in conflicted_paths, got {:?}",
                conflicted_paths
            );
            true
        }
        other => panic!("expected Conflict as last result, got {:?}", other),
    };
    assert!(conflict);

    // All earlier results were Ok (main Sync, A Rebase).
    for r in &results[..results.len() - 1] {
        assert!(
            matches!(r, OpResult::Ok { .. }),
            "earlier op not Ok: {:?}",
            r
        );
    }

    // No result for C — it was never attempted.
    assert!(
        !results.iter().any(|r| {
            matches!(r,
                OpResult::Ok { branch }
                | OpResult::Conflict { branch, .. }
                | OpResult::Error { branch, .. } if branch == "C")
        }),
        "C should not have been attempted; got {:?}",
        results
    );

    // Repo is mid-rebase per repo_status.
    let status = repo_status(fx.p()).unwrap();
    assert!(
        matches!(
            status.state,
            RepoState::Rebase | RepoState::RebaseInteractive | RepoState::RebaseMerge
        ),
        "expected rebase-in-progress state, got {:?}",
        status.state
    );
}

#[test]
fn tier_b_resume_after_continue() {
    let fx = setup_with_remotes();
    setup_midstack_conflict(&fx);

    let plan1 = build_cascade_plan(fx.p(), &fx.main_branch).unwrap();
    let results1 = apply_plan(fx.p(), &plan1).unwrap();
    assert!(matches!(results1.last(), Some(OpResult::Conflict { .. })));

    // Simulate manual conflict resolution to a value distinct from both
    // parents, so B's commit remains non-empty after --continue (avoiding
    // a degenerate B-equals-A state).
    std::fs::write(format!("{}/conflict.txt", fx.path), "v-resolved\n").unwrap();
    run_git(&fx.path, &["add", "conflict.txt"]);
    // GIT_EDITOR=true skips opening an editor for the commit message during --continue.
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&fx.path)
        .args(["rebase", "--continue"])
        .env("GIT_EDITOR", "true")
        .status()
        .unwrap();
    assert!(status.success(), "git rebase --continue failed");
    assert!(matches!(
        repo_status(fx.p()).unwrap().state,
        RepoState::Clean
    ));

    let plan2 = build_cascade_plan(fx.p(), &fx.main_branch).unwrap();
    assert!(
        !plan2.ops.is_empty(),
        "expected residual plan after --continue (at minimum, C's rebase); got {:?}",
        plan2
    );

    let results2 = apply_plan(fx.p(), &plan2).unwrap();
    assert_all_ok(&results2);

    // C now sits on top of the resolved B.
    let b_tip = tip(&fx.path, "B");
    assert_eq!(merge_base(&fx.path, "C", "B"), b_tip);
    // And B contains the upstream change.
    let new_upstream = tip(&fx.path, &format!("upstream/{}", fx.main_branch));
    assert!(is_ancestor(&fx.path, &new_upstream, "B"));
    assert!(is_ancestor(&fx.path, &new_upstream, "C"));
}

#[test]
fn tier_b_resume_after_abort() {
    let fx = setup_with_remotes();
    setup_midstack_conflict(&fx);

    let a_pre = tip(&fx.path, "A");
    let b_pre = tip(&fx.path, "B");
    let c_pre = tip(&fx.path, "C");

    let plan1 = build_cascade_plan(fx.p(), &fx.main_branch).unwrap();
    let _results1 = apply_plan(fx.p(), &plan1).unwrap();

    // Abort the failed rebase of B.
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&fx.path)
        .args(["rebase", "--abort"])
        .status()
        .unwrap();
    assert!(status.success(), "git rebase --abort failed");
    assert!(matches!(
        repo_status(fx.p()).unwrap().state,
        RepoState::Clean
    ));

    // A's rebase from plan1 is preserved (it completed before B failed).
    assert_ne!(
        tip(&fx.path, "A"),
        a_pre,
        "A should remain rebased after abort"
    );
    // B and C are back at their pre-cascade positions.
    assert_eq!(tip(&fx.path, "B"), b_pre);
    assert_eq!(tip(&fx.path, "C"), c_pre);

    // A fresh plan should re-include B's rebase (and C's, since B will move).
    let plan2 = build_cascade_plan(fx.p(), &fx.main_branch).unwrap();
    use gitui::engine::Operation;
    let b_in_plan = plan2
        .ops
        .iter()
        .any(|op| matches!(op, Operation::Rebase { branch, .. } if branch == "B"));
    let c_in_plan = plan2
        .ops
        .iter()
        .any(|op| matches!(op, Operation::Rebase { branch, .. } if branch == "C"));
    assert!(
        b_in_plan,
        "B should be back in the plan after abort: {:?}",
        plan2
    );
    assert!(
        c_in_plan,
        "C should be in the plan after abort: {:?}",
        plan2
    );
}

// ============================================================================
// Tier C: safety guards
// ============================================================================

#[test]
fn tier_c_refuses_when_mid_rebase() {
    let fx = setup_with_remotes();
    setup_midstack_conflict(&fx);

    // Drive the repo into mid-rebase state by applying a plan that conflicts.
    let plan1 = build_cascade_plan(fx.p(), &fx.main_branch).unwrap();
    let _results1 = apply_plan(fx.p(), &plan1).unwrap();
    assert!(!matches!(
        repo_status(fx.p()).unwrap().state,
        RepoState::Clean
    ));

    // A *new* apply_plan call must refuse before running any op, returning
    // exactly one OpResult::Error.
    use gitui::engine::Operation;
    let dummy_plan = gitui::Plan {
        intent: gitui::PlanIntent::Sync,
        branch: fx.main_branch.clone(),
        ops: vec![Operation::Rebase {
            branch: "A".to_string(),
            onto: fx.main_branch.clone(),
            upstream: None,
            predicted_conflict: None,
        }],
        skipped_branches: Vec::new(),
    };
    let results = apply_plan(fx.p(), &dummy_plan).unwrap();
    assert_eq!(results.len(), 1, "expected exactly one refusal result");
    match &results[0] {
        OpResult::Error { message, .. } => {
            assert!(
                message.contains("refusing") || message.contains("in-progress"),
                "error message should explain the refusal, got: {}",
                message
            );
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[test]
fn tier_c_dirty_tree_produces_empty_plan() {
    // Documents the current planner behavior: a dirty working tree causes
    // build_cascade_plan to emit no rebase/reset ops. apply_plan on the empty
    // plan is a no-op (Ok with empty results).
    let fx = setup_with_remotes();
    stack_branch(&fx, "A", &fx.main_branch, "a.txt", "A");
    advance_upstream(&fx, "u.txt", "u", "upstream commit");

    // Dirty the working tree (uncommitted).
    run_git(&fx.path, &["checkout", "-q", &fx.main_branch]);
    std::fs::write(format!("{}/dirty.txt", fx.path), "dirty\n").unwrap();
    run_git(&fx.path, &["add", "dirty.txt"]);
    assert!(repo_status(fx.p()).unwrap().is_dirty);

    let plan = build_cascade_plan(fx.p(), &fx.main_branch).unwrap();
    assert!(
        plan.ops.is_empty(),
        "expected empty plan when dirty, got: {:?}",
        plan.ops
    );
    let results = apply_plan(fx.p(), &plan).unwrap();
    assert!(
        results.is_empty(),
        "expected no results, got: {:?}",
        results
    );
}

#[test]
fn tier_c_noop_when_already_up_to_date() {
    let fx = setup_with_remotes();
    stack_branch(&fx, "A", &fx.main_branch, "a.txt", "A");

    // No advance_upstream — main is already at upstream/main.
    let plan = build_cascade_plan(fx.p(), &fx.main_branch).unwrap();
    assert!(
        plan.ops.is_empty(),
        "expected empty plan when already up to date, got: {:?}",
        plan.ops
    );
    let results = apply_plan(fx.p(), &plan).unwrap();
    assert!(results.is_empty());
}

// ============================================================================
// Tier D: predictive parity
// ============================================================================

#[test]
fn tier_d_predicted_clean_actually_applies_cleanly() {
    // Build a stack where rebase is predicted CLEAN, then apply and verify.
    let fx = setup_with_remotes();
    stack_branch(&fx, "A", &fx.main_branch, "a.txt", "A content");
    stack_branch(&fx, "B", "A", "b.txt", "B content");

    // Upstream change touches a file unrelated to anything in A or B.
    advance_upstream(&fx, "unrelated.txt", "u\n", "unrelated upstream change");

    let plan = build_cascade_plan(fx.p(), &fx.main_branch).unwrap();

    use gitui::engine::Operation;
    // Each Rebase op should have been pre-flighted as not-conflicting.
    for op in &plan.ops {
        if let Operation::Rebase {
            branch,
            predicted_conflict,
            ..
        } = op
        {
            assert_eq!(
                *predicted_conflict,
                Some(false),
                "rebase of {} should be predicted CLEAN; full op: {:?}",
                branch,
                op
            );
        }
    }

    let results = apply_plan(fx.p(), &plan).unwrap();
    assert_all_ok(&results);
}

#[test]
fn tier_d_predicted_conflict_actually_conflicts() {
    // Build a stack where a rebase is predicted CONFLICT, then apply and
    // confirm the planner did not lie: execution does reach a Conflict.
    let fx = setup_with_remotes();
    setup_midstack_conflict(&fx);

    let plan = build_cascade_plan(fx.p(), &fx.main_branch).unwrap();

    use gitui::engine::Operation;
    let b_predicted = plan.ops.iter().find_map(|op| match op {
        Operation::Rebase {
            branch,
            predicted_conflict,
            ..
        } if branch == "B" => Some(*predicted_conflict),
        _ => None,
    });
    assert_eq!(
        b_predicted,
        Some(Some(true)),
        "B's rebase should be predicted CONFLICT, full plan: {:?}",
        plan.ops
    );

    // Execute to verify the prediction matches reality.
    let results = apply_plan(fx.p(), &plan).unwrap();
    let b_result = results.iter().find(|r| match r {
        OpResult::Conflict { branch, .. }
        | OpResult::Ok { branch }
        | OpResult::Error { branch, .. } => branch == "B",
    });
    assert!(
        matches!(b_result, Some(OpResult::Conflict { .. })),
        "B's actual outcome should be Conflict, got {:?}; results: {:?}",
        b_result,
        results
    );
}
