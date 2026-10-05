use super::*;
use crate::{app::Target, publication_fixture::Fixture, router::Route, storage};
use anyhow::{Context, Result};
use canix_toolbelt::review::Verdict;
use clap::Parser;
use serde_json::{json, Value};

fn scratch() -> Result<tempfile::TempDir> {
    use std::os::unix::fs::PermissionsExt;
    Ok(tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?)
}

#[derive(Parser)]
struct OfflineCommand {
    #[command(subcommand)]
    command: PublicationCommand,
}

fn run(app: &Application, args: Vec<String>) -> Result<Value> {
    OfflineCommand::try_parse_from(std::iter::once("repo-review".into()).chain(args))?
        .command
        .invoke(app)
}

fn target_args(action: &str, route: &Route) -> Vec<String> {
    vec![
        action.into(),
        "--pr".into(),
        route.candidate.url.clone(),
        "--expected-head".into(),
        route.candidate.head.clone(),
        "--remote-only".into(),
    ]
}

fn repair_args(action: &str, route: &Route) -> Vec<String> {
    let mut args = target_args(action, route);
    args.extend([
        "--request".into(),
        route.id.clone(),
        "--publication-id".into(),
        "7".into(),
    ]);
    args
}

fn update_args(route: &Route, digest: &str, forgejo: bool, apply: bool) -> Vec<String> {
    let mut args = repair_args("update-publication", route);
    args.extend(["--expected-publication-digest".into(), digest.into()]);
    if !forgejo {
        args.extend([
            "--details-url".into(),
            "https://example.invalid/details".into(),
        ]);
    }
    if apply {
        args.push("--apply".into());
    }
    args
}

fn api(app: &Application) -> Result<Value> {
    storage::read(&app.state.join("fixture-api.json"))?.context("fixture API missing")
}

fn effect_count(app: &Application, method: &str) -> Result<usize> {
    Ok(api(app)?["calls"]
        .as_array()
        .context("fixture calls missing")?
        .iter()
        .filter(|call| call["method"] == method)
        .count())
}

fn exercise_commands(forgejo: bool, lose_create: bool, lose_patch: bool) -> Result<()> {
    let scratch = scratch()?;
    let (config, route) = Fixture::seed(scratch.path(), forgejo)?;
    let mut app = Application {
        config: config.clone(),
        state: scratch.path().into(),
    };
    let mut guard = Fixture::install(scratch.path());
    if lose_create {
        let mut value = api(&app)?;
        value["lose_next"] = json!("POST");
        storage::save(&app.state.join("fixture-api.json"), &value)?;
    }
    let mut initial = target_args(if forgejo { "publish" } else { "publish-check" }, &route);
    if !forgejo {
        initial.extend([
            "--details-url".into(),
            "https://example.invalid/details".into(),
        ]);
    }
    let result = run(&app, initial.clone());
    assert_eq!(result.is_err(), lose_create);
    assert_eq!(app.status(&route.id)?.outcome.verdict, Verdict::Findings);
    if forgejo {
        assert!(api(&app)?["object"]["body"]
            .as_str()
            .context("missing summary")?
            .contains("findings"));
    } else {
        assert_eq!(api(&app)?["object"]["conclusion"], "failure");
    }
    let kind = if forgejo {
        "forgejo-comment"
    } else {
        "github-check"
    };
    let original = app
        .state
        .join(format!("publications/{}-{kind}.json", route.id));
    let original_bytes = std::fs::read(&original)?;
    // Reconstruct application and test transport entirely from persisted files.
    drop(guard);
    app = Application {
        config: config.clone(),
        state: scratch.path().into(),
    };
    guard = Fixture::install(scratch.path());
    let adopted = run(&app, repair_args("reconcile-publication", &route))?;
    assert_eq!(adopted["merge_authorized"], false);
    assert_eq!(adopted["observation_scope"], "historical-publication-only");
    assert!(run(&app, initial).is_err());
    let digest = adopted["payload_digest"]
        .as_str()
        .context("missing previous digest")?;
    let disposition = run(
        &app,
        vec![
            "disposition".into(),
            route.id.clone(),
            "--finding".into(),
            "low-finding".into(),
            "--reason".into(),
            "Exact-request regression proves the invariant".into(),
            "--evidence".into(),
            "src/publication/lifecycle.rs + lost_update_journal regression".into(),
        ],
    )?;
    assert_eq!(disposition["requires_fresh_observation"], true);
    assert_eq!(app.status(&route.id)?.outcome.verdict, Verdict::Findings);
    let preview = run(&app, update_args(&route, digest, forgejo, false))?;
    assert_eq!(preview["would_update"], true);
    assert_eq!(preview["applies_update"], false);
    assert_eq!(effect_count(&app, "PATCH")?, 0);
    let fresh = app.status(&route.id)?;
    assert_eq!(fresh.outcome.verdict, Verdict::Ready);
    assert!(fresh
        .outcome
        .review
        .as_ref()
        .context("review missing")?
        .findings
        .is_empty());
    assert!(fresh
        .merge_blockers
        .iter()
        .any(|s| s == "Require extra forge gate"));
    assert!(app
        .merge(
            &Target {
                pr: Some(route.candidate.url.clone()),
                expected_head: Some(route.candidate.head.clone()),
                remote_only: true,
                ..Target::default()
            },
            true
        )
        .is_err());
    let mut value = api(&app)?;
    value["lose_next"] = if lose_patch {
        json!("PATCH")
    } else {
        Value::Null
    };
    storage::save(&app.state.join("fixture-api.json"), &value)?;
    let applied = run(&app, update_args(&route, digest, forgejo, true));
    assert_eq!(applied.is_err(), lose_patch);
    if let Ok(receipt) = applied {
        assert_eq!(receipt["applies_update"], true);
        assert_eq!(receipt["merge_authorized"], false);
    }
    let journal = app
        .state
        .join(format!("publications/{}-{kind}-lifecycle.json", route.id));
    let pending: Value = storage::read(&journal)?.context("journal missing")?;
    assert_eq!(pending["pending"].is_object(), lose_patch);
    assert!(journal.with_extension("tombstone.json").is_file());
    drop(guard);
    app = Application {
        config,
        state: scratch.path().into(),
    };
    guard = Fixture::install(scratch.path());
    assert!(run(&app, update_args(&route, digest, forgejo, true)).is_err());
    // Historical adoption must not collect a new provider result, inspect the
    // current PR, or weaken the update path after the PR moves/closes.
    let mut moved = route.candidate.clone();
    moved.head = "e".repeat(40);
    moved.base = "f".repeat(40);
    moved.open = false;
    storage::save(&app.state.join("fixture-candidate.json"), &moved)?;
    let before = api(&app)?["calls"]
        .as_array()
        .context("calls missing")?
        .len();
    let recovered = run(&app, repair_args("reconcile-publication", &route))?;
    assert_eq!(recovered["update_pending"], false);
    assert_eq!(recovered["updates"], 1);
    assert_eq!(recovered["merge_authorized"], false);
    assert_eq!(
        recovered["observation_scope"],
        "historical-publication-only"
    );
    let after = api(&app)?;
    let calls = after["calls"].as_array().context("calls missing")?;
    assert_eq!(calls.len(), before + 1);
    assert_eq!(calls[before]["method"], "GET");
    assert!(calls[before]["path"]
        .as_str()
        .context("path missing")?
        .ends_with("/7"));
    assert!(run(
        &app,
        update_args(
            &route,
            recovered["payload_digest"]
                .as_str()
                .context("digest missing")?,
            forgejo,
            true
        )
    )
    .is_err());
    assert_eq!(effect_count(&app, "POST")?, 1);
    assert_eq!(effect_count(&app, "PATCH")?, 1);
    assert_eq!(std::fs::read(original)?, original_bytes);
    let state = api(&app)?;
    if forgejo {
        assert!(state["object"]["body"]
            .as_str()
            .context("summary missing")?
            .contains("ready"));
    } else {
        assert_eq!(state["object"]["conclusion"], "success");
    }
    // Correlation remains strict even for historical object reads.
    let mut changed = state.clone();
    changed["object"]["id"] = json!(8);
    storage::save(&app.state.join("fixture-api.json"), &changed)?;
    assert!(run(&app, repair_args("reconcile-publication", &route)).is_err());
    drop(guard);
    Ok(())
}

#[test]
fn github_command_findings_disposition_preview_unknown_update_restart_and_historical_adoption(
) -> Result<()> {
    exercise_commands(false, false, true)
}

#[test]
fn forgejo_command_findings_disposition_preview_unknown_update_restart_and_historical_adoption(
) -> Result<()> {
    exercise_commands(true, false, true)
}

#[test]
fn github_command_lost_create_reply_is_adopted_without_duplicate_post() -> Result<()> {
    exercise_commands(false, true, true)
}

#[test]
fn forgejo_command_lost_create_reply_is_adopted_without_duplicate_post() -> Result<()> {
    exercise_commands(true, true, true)
}

#[test]
fn github_command_confirms_disposition_update_and_rejects_old_previous_digest() -> Result<()> {
    exercise_commands(false, false, false)
}

#[test]
fn forgejo_command_confirms_disposition_update_and_rejects_old_previous_digest() -> Result<()> {
    exercise_commands(true, false, false)
}

#[test]
fn command_parser_rejects_nonpositive_object_ids_and_requires_previous_digest() -> Result<()> {
    let scratch = scratch()?;
    let (_, route) = Fixture::seed(scratch.path(), false)?;
    let mut args = repair_args("reconcile-publication", &route);
    *args.last_mut().context("args missing")? = "0".into();
    assert!(
        OfflineCommand::try_parse_from(std::iter::once("repo-review".into()).chain(args)).is_err()
    );
    assert!(OfflineCommand::try_parse_from(
        std::iter::once("repo-review".into()).chain(repair_args("update-publication", &route))
    )
    .is_err());
    Ok(())
}
