//! The CLI is the whole product surface for someone who will never open a Rust file, so the
//! paths that need no model are tested end to end against a live app.

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::{Arc, Mutex};

use app::domain::World as AppWorld;
use app::{router, AppState};
use serde_json::Value;
use tokio::sync::broadcast;

/// The app has to keep serving while the test thread blocks on the CLI subprocess, so it gets a
/// runtime of its own. Spawning it on the test's own runtime deadlocks: `Command::output` parks the
/// only thread that could poll the listener, and the CLI hangs fetching the world model.
fn serve(seed: u64) -> String {
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let (tx, _) = broadcast::channel(1024);
            let state = Arc::new(AppState { world: Mutex::new(AppWorld::seeded(seed)), events: tx });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            addr_tx.send(listener.local_addr().unwrap()).unwrap();
            axum::serve(listener, router(state)).await.unwrap();
        });
    });
    format!("http://{}", addr_rx.recv().expect("the app bound a port"))
}

/// Oracle reads from a synchronous test.
fn get_json(url: String) -> Value {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async { reqwest::get(url).await.unwrap().json().await.unwrap() })
}

fn rwmcp(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_rwmcp")).args(args).output().expect("run rwmcp");
    (out.status.success(), String::from_utf8_lossy(&out.stdout).to_string(), String::from_utf8_lossy(&out.stderr).to_string())
}

/// The exit code, which is the only thing a caller can branch on without parsing text.
fn code(args: &[&str]) -> i32 {
    Command::new(env!("CARGO_BIN_EXE_rwmcp")).args(args).output().expect("run rwmcp").status.code().unwrap_or(-1)
}

fn wants_file(name: &str, body: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("rwmcp-{}-{name}.wants", std::process::id()));
    std::fs::write(&p, body).unwrap();
    p
}

#[test]
fn world_reports_what_the_app_can_be_asked_for() {
    let base = serve(2);
    let (ok, out, err) = rwmcp(&["--app", &base, "--world"]);
    assert!(ok, "{err}");
    assert!(out.contains("invoice(id=$id).status='sent'"), "{out}");
    assert!(out.contains("[ui only: approveInvoice]"), "{out}");
    assert!(out.contains("no postcondition"), "unplannable operations are named: {out}");
}

#[test]
fn a_wants_file_plans_and_runs_with_no_model_call() {
    let base = serve(2);
    let w = wants_file(
        "good",
        "# an agent wrote this\ninvoice(customer=each(customer())).exists\ninvoice(customer=each(customer())).status='sent'\nreport(invoices=[all(invoice(customer=each(customer())))]).exists\n",
    );
    let w = w.to_str().unwrap();

    let (ok, out, err) = rwmcp(&["--app", &base, "--wants", w]);
    assert!(ok, "{err}");
    assert!(out.contains("31 steps, 4 deep"), "{out}");
    assert!(out.contains("10 steps leave the system (email, money): sendInvoice"), "the op is named once, not ten times: {out}");
    assert!(out.contains("Planning cost 0 model calls"), "a wants file costs nothing: {out}");

    // An effectful plan refuses to run unnoticed.
    let (ok, _, err) = rwmcp(&["--app", &base, "--wants", w, "--run"]);
    assert!(!ok);
    assert!(err.contains("Re-run with --yes"), "{err}");

    // And nothing happened.
    let state: Value = get_json(format!("{base}/oracle/state"));
    assert_eq!(state["invoices"].as_array().unwrap().len(), 0);

    let (ok, out, err) = rwmcp(&["--app", &base, "--wants", w, "--run", "--yes"]);
    assert!(ok, "{err}");
    assert!(out.contains("Committed"), "{out}");
    assert!(out.contains("0 model calls"), "{out}");
    let state: Value = get_json(format!("{base}/oracle/state"));
    assert_eq!(state["invoices"].as_array().unwrap().len(), 10);
    assert!(state["invoices"].as_array().unwrap().iter().all(|i| i["status"] == "sent"));
    assert_eq!(state["reports"].as_array().unwrap().len(), 1);
    let effects: Value = get_json(format!("{base}/oracle/effects"));
    assert_eq!(effects["double_sends"], 0);
}

#[test]
fn planning_alone_changes_nothing() {
    let base = serve(2);
    let w = wants_file("dry", "invoice(customer=customer(name='Acme')).status='sent'\n");
    let (ok, out, err) = rwmcp(&["--app", &base, "--wants", w.to_str().unwrap()]);
    assert!(ok, "{err}");
    assert!(out.contains("Nothing was done. Add --run"), "{out}");
    let state: Value = get_json(format!("{base}/oracle/state"));
    assert_eq!(state["invoices"].as_array().unwrap().len(), 0);
}

#[test]
fn a_bad_want_is_explained_and_nothing_is_planned() {
    let base = serve(2);
    let w = wants_file("bad", "widget(name='x').exists\ninvoice(customer=customer(name='Acme')).nonesuch='x'\n");
    let (ok, _, err) = rwmcp(&["--app", &base, "--wants", w.to_str().unwrap()]);
    assert!(!ok);
    assert!(err.contains("unknown entity 'widget'"), "{err}");
    assert!(err.contains("does not exist"), "an unknown field is named too: {err}");
}

/// A `$name` left in a wants file is a parameter nobody bound, not a malformed want — so the
/// message is the flag that fixes it rather than a lint about variables.
#[test]
fn an_unbound_placeholder_names_the_flag_that_fills_it() {
    let base = serve(2);
    let w = wants_file("unbound", "invoice(customer=customer(name=$who)).exists\n");
    let w = w.to_str().unwrap();
    let (ok, _, err) = rwmcp(&["--app", &base, "--wants", w]);
    assert!(!ok);
    assert!(err.contains("--set who=") && err.contains("still need values"), "{err}");
    assert_eq!(code(&["--app", &base, "--wants", w]), 10);
    // And binding it gets past the gate.
    assert_eq!(code(&["--app", &base, "--wants", w, "--set", "who=Acme"]), 0);
}

#[test]
fn an_openapi_file_can_be_inspected_without_a_running_app() {
    let doc = concat!(env!("CARGO_MANIFEST_DIR"), "/../app/static/openapi.json");
    let (ok, out, err) = rwmcp(&["--app", doc, "--world"]);
    assert!(ok, "{err}");
    assert!(out.contains("can make true"), "{out}");
    // But running against a file is refused rather than half-attempted.
    let w = wants_file("file", "invoice(customer=customer(name='Acme')).exists\n");
    let (ok, _, err) = rwmcp(&["--app", doc, "--wants", w.to_str().unwrap(), "--run", "--yes"]);
    assert!(!ok);
    assert!(err.contains("give me the app's URL"), "{err}");
}

#[test]
fn a_recipe_replaces_the_model_when_only_the_parameters_change() {
    let base = serve(2);
    let dir = std::env::temp_dir().join(format!("rwmcp-recipes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let d = dir.to_str().unwrap().to_string();
    let w = wants_file("param", "invoice(customer=customer(name=$who)).exists\ninvoice(customer=customer(name=$who)).status='sent'\n");
    let wp = w.to_str().unwrap().to_string();

    // Saving keeps the placeholders, and says what they are.
    let (ok, out, err) = rwmcp(&["--app", &base, "--wants", &wp, "--save", "billing", "--recipes-dir", &d, "--set", "who=Acme"]);
    assert!(ok, "{err}");
    assert!(out.contains("billing.json") && out.contains("--set who="), "{out}");

    // It is listed with the parameter it needs.
    let (ok, out, err) = rwmcp(&["--list", "--recipes-dir", &d]);
    assert!(ok, "{err}");
    assert!(out.contains("billing") && out.contains("--set who="), "{out}");

    // Running it for a different customer costs nothing, and needs no --app: the recipe knows.
    let (ok, out, err) = rwmcp(&["--recipe", "billing", "--set", "who=Globex", "--recipes-dir", &d, "--run", "--yes"]);
    assert!(ok, "{err}");
    assert!(out.contains("Committed") && out.contains("0 model calls"), "{out}");
    let state: Value = get_json(format!("{base}/oracle/state"));
    let invoices = state["invoices"].as_array().unwrap();
    assert_eq!(invoices.len(), 1);
    assert_eq!(invoices[0]["customer_id"], 2, "Globex, not Acme");

    // Forgetting the parameter is explained, not guessed at.
    let (ok, _, err) = rwmcp(&["--app", &base, "--recipe", "billing", "--recipes-dir", &d, "--run", "--yes"]);
    assert!(!ok);
    assert!(err.contains("--set who="), "{err}");
    // The saved file is JSON an agent writes and a program reads.
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("billing.json")).unwrap()).unwrap();
    assert_eq!(saved["name"], "billing");
    assert_eq!(saved["app"], base, "the recipe remembers the app it was made for");
    assert_eq!(saved["params"], serde_json::json!(["who"]));
    assert!(saved["wants"][0].as_str().unwrap().contains("customer(name=$who)"), "placeholders are kept, not baked in: {saved}");

    // A recipe can also be a path, so it can live in the repo next to the code it drives.
    let (ok, out, err) = rwmcp(&["--app", &base, "--recipe", dir.join("billing.json").to_str().unwrap(), "--set", "who=Initech"]);
    assert!(ok, "{err}");
    assert!(out.contains("createInvoice"), "{out}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn saying_nothing_at_all_explains_the_options() {
    let base = serve(2);
    let (ok, _, err) = rwmcp(&["--app", &base]);
    assert!(!ok);
    assert!(err.contains("--goal") && err.contains("--wants") && err.contains("--recipe"), "{err}");
}

#[test]
fn every_failure_has_its_own_exit_code() {
    let base = serve(6); // two customers named Acme, so a plan must ask
    let ask = wants_file("ask", "invoice(customer=customer(name='Acme')).status='sent'\n");
    let ask = ask.to_str().unwrap();
    let bad = wants_file("nope", "widget(x=1).exists\n");
    let bad = bad.to_str().unwrap();

    assert_eq!(code(&["--app", &base, "--wants", ask]), 0, "planning alone succeeds");
    assert_eq!(code(&["--app", &base, "--wants", bad]), 10, "wants rejected");
    assert_eq!(code(&["--app", &base]), 10, "no source given");
    assert_eq!(code(&["--app", &base, "--wants", ask, "--run"]), 12, "refused without --yes");
    assert_eq!(code(&["--app", &base, "--wants", ask, "--run", "--yes"]), 11, "needs an answer");
    assert_eq!(code(&["--app", "http://127.0.0.1:1", "--wants", ask]), 13, "app unreachable");
    // clap keeps its own usage code, so bad flags stay distinguishable from bad wants.
    assert_eq!(code(&["--nonsense"]), 2);
}

#[test]
fn failures_are_json_when_asked() {
    let base = serve(6);
    let bad = wants_file("json-bad", "widget(x=1).exists\ninvoice(customer=customer(name='Acme')).nonesuch='x'\n");
    let (ok, out, _) = rwmcp(&["--app", &base, "--wants", bad.to_str().unwrap(), "--json"]);
    assert!(!ok);
    let v: Value = serde_json::from_str(&out).expect("a failure still prints one JSON object");
    assert_eq!(v["ok"], false);
    assert_eq!(v["code"], "wants_rejected");
    let codes: Vec<&str> = v["detail"]["codes"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
    assert!(codes.contains(&"unknown_entity") && codes.contains(&"unknown_field"), "{codes:?}");
    assert_eq!(v["detail"]["errors"][0]["entity"], "widget", "the error keeps its fields, not just prose");

    // A fork reports the question and its evidence, so a caller can answer it.
    let ask = wants_file("json-ask", "invoice(customer=customer(name='Acme')).status='sent'\n");
    let (ok, out, _) = rwmcp(&["--app", &base, "--wants", ask.to_str().unwrap(), "--run", "--yes", "--json"]);
    assert!(!ok);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["status"], "need_think");
    assert!(v["yield_reason"].as_str().unwrap().contains("Acme"), "{v}");

    // And success says so too.
    let good = wants_file("json-good", "invoice(customer=customer(name='Globex')).exists\n");
    let (ok, out, _) = rwmcp(&["--app", &base, "--wants", good.to_str().unwrap(), "--json"]);
    assert!(ok);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], true);
    assert!(!v["plan"]["nodes"].as_array().unwrap().is_empty());
}

#[test]
fn a_parse_error_points_at_the_column() {
    let base = serve(2);
    let w = wants_file("trailing", "invoice(customer=\n");
    let (ok, _, err) = rwmcp(&["--app", &base, "--wants", w.to_str().unwrap()]);
    assert!(!ok);
    assert!(err.contains("^"), "a caret marks the spot: {err}");
    assert!(err.contains("at byte 17"), "{err}");
}

/// The fork loop, which is what an agent hits first on any ambiguous world: run, get asked, answer,
/// carry on. The answer is a rewrite of the ambiguous part, so every other want keeps its text and
/// therefore its idempotency key.
#[test]
fn a_fork_can_be_answered_without_a_model() {
    let base = serve(6); // two customers named Acme, ids 1 and 11
    let w = wants_file("fork", "invoice(customer=customer(name='Acme')).status='sent'\n");
    let w = w.to_str().unwrap();

    // Unanswered, it stops and asks, and nothing is invoiced.
    assert_eq!(code(&["--app", &base, "--wants", w, "--run", "--yes"]), 11);
    assert!(get_json(format!("{base}/oracle/state"))["invoices"].as_array().unwrap().is_empty());

    // An answer that matches no want is refused before anything runs.
    let (ok, _, err) = rwmcp(&["--app", &base, "--wants", w, "--run", "--yes", "--answer", "customer(name='Nobody')=>customer(id=11)"]);
    assert!(!ok);
    assert!(err.contains("would change nothing"), "{err}");

    // A well-formed one resolves it.
    let state = std::env::temp_dir().join(format!("rwmcp-{}-run.json", std::process::id()));
    let state = state.to_str().unwrap();
    let (ok, _, err) = rwmcp(&["--app", &base, "--wants", w, "--run", "--yes", "--answer", "customer(name='Acme')=>customer(id=11)", "--receipt-out", state]);
    assert!(ok, "{err}");
    let invoices = get_json(format!("{base}/oracle/state"))["invoices"].as_array().unwrap().clone();
    assert_eq!(invoices.len(), 1);
    assert_eq!(invoices[0]["customer_id"], 11, "it invoiced the customer that was named");
    assert_eq!(invoices[0]["status"], "sent");

    // And resuming does the work again only in name: the keys match, so no second invoice.
    assert_eq!(code(&["--app", &base, "--resume", state, "--run", "--yes"]), 0);
    assert_eq!(get_json(format!("{base}/oracle/state"))["invoices"].as_array().unwrap().len(), 1, "resume must not repeat committed effects");
}

/// A resumed run must keep the plan id it was saved under: idempotency keys are
/// `{plan_id}/{hash}`, so a fresh id would match nothing and send everything twice.
#[test]
fn a_saved_run_keeps_its_plan_id() {
    let base = serve(2);
    let w = wants_file("keep-id", "invoice(customer=customer(name='Acme')).status='sent'\n");
    let state = std::env::temp_dir().join(format!("rwmcp-{}-keepid.json", std::process::id()));
    let state = state.to_str().unwrap();
    let (ok, _, err) = rwmcp(&["--app", &base, "--wants", w.to_str().unwrap(), "--run", "--yes", "--receipt-out", state]);
    assert!(ok, "{err}");

    let saved: Value = serde_json::from_str(&std::fs::read_to_string(state).unwrap()).unwrap();
    assert!(saved["plan_id"].as_str().unwrap().starts_with("cli-"), "{saved}");
    assert!(!saved["wants"].as_array().unwrap().is_empty());
    assert_eq!(saved["receipt"]["status"], "committed");

    assert_eq!(code(&["--app", &base, "--resume", state, "--run", "--yes"]), 0);
    assert_eq!(get_json(format!("{base}/oracle/state"))["invoices"].as_array().unwrap().len(), 1);
}

/// --validate is the review `annotate-world-model` describes, made runnable. It has to be quiet on
/// a model that is right and specific on one that is wrong, or nobody will trust it.
#[test]
fn validate_is_quiet_on_a_good_world_model() {
    let doc = concat!(env!("CARGO_MANIFEST_DIR"), "/../app/static/openapi.json");
    let (ok, out, err) = rwmcp(&["--app", doc, "--validate", "--json"]);
    assert!(ok, "{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], true);
    assert!(v["errors"].as_array().unwrap().is_empty(), "our own app must validate clean: {}", v["errors"]);
    // The read-only operations are noted, not condemned.
    let codes: Vec<&str> = v["warnings"].as_array().unwrap().iter().map(|w| w["code"].as_str().unwrap()).collect();
    assert!(codes.iter().all(|c| *c == "no_postcondition"), "{codes:?}");
}

#[test]
fn validate_names_every_defect_in_a_broken_world_model() {
    let doc = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/broken-openapi.json");
    let (ok, out, _) = rwmcp(&["--app", doc, "--validate", "--json"]);
    assert!(!ok);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], false);
    let found = |code: &str, list: &str| -> bool { v[list].as_array().unwrap().iter().any(|e| e["code"] == code) };
    assert!(found("unknown_entity", "errors"), "a footprint on an undeclared entity: {v}");
    assert!(found("unknown_field", "errors"), "a post about a field the entity lacks: {v}");
    assert!(found("no_surfaces", "errors"), "an operation nothing can call: {v}");
    assert!(found("before_unknown_op", "errors"), "before pointing nowhere: {v}");
    assert!(found("unbound_footprint_param", "errors"), "the silent widening to entity:*: {v}");
    // And the bug class the review caught: two writers to one thing with no declared order.
    assert!(found("unordered_writers", "warnings"), "{v}");
    assert_eq!(code(&["--app", doc, "--validate"]), 10);
}

/// A plan that changes when the wants are listed in the other order is a plan that depends on
/// typing order. This is the check both skills end with and nothing could run.
#[test]
fn order_check_proves_the_plan_does_not_depend_on_want_order() {
    let base = serve(2);
    let w = wants_file(
        "order",
        "report(invoices=[all(invoice(customer=each(customer())))]).exists\ninvoice(customer=each(customer())).exists\ninvoice(customer=each(customer())).status='sent'\n",
    );
    let (ok, out, err) = rwmcp(&["--app", &base, "--wants", w.to_str().unwrap(), "--order-check", "--json"]);
    assert!(ok, "{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], true);
    assert!(v["differences"].as_array().unwrap().is_empty(), "{v}");
    assert_eq!(v["steps"], 31);
}

/// --init turns a plain OpenAPI document into a filled-in-the-blanks exercise.
#[test]
fn init_offers_a_block_for_every_unannotated_operation() {
    let doc = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/plain-openapi.json");
    let (ok, out, err) = rwmcp(&["--app", doc, "--init", "--json"]);
    assert!(ok, "{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["annotated"], 0);
    let todo = v["todo"].as_object().unwrap();
    assert_eq!(todo.len(), 2);
    assert_eq!(todo["createThing"]["_parameters_you_can_use"], serde_json::json!(["name", "size"]));
    assert_eq!(todo["createThing"]["writes"], serde_json::json!(["entity:new"]));
    assert_eq!(todo["listThings"]["reads"], serde_json::json!(["entity:*"]), "a GET is guessed as a read");
    assert!(todo["listThings"]["writes"].as_array().unwrap().is_empty());

    // Our own app is fully annotated, so there is nothing left to do.
    let ours = concat!(env!("CARGO_MANIFEST_DIR"), "/../app/static/openapi.json");
    let (_, out, _) = rwmcp(&["--app", ours, "--init", "--json"]);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["annotated"], 9);
    assert!(v["todo"].as_object().unwrap().is_empty());
}

/// Serve a fixture OpenAPI document and one collection, so an app that is not ours can be
/// planned against. Everything selector-related used to be hardcoded to a `customer` entity
/// fetched from `/api/customers`; this app has neither.
fn serve_second_app() -> String {
    use axum::routing::get;
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let doc: Value = serde_json::from_str(include_str!("fixtures/second-app-openapi.json")).unwrap();
            let projects = serde_json::json!([
                {"id": 1, "name": "Apollo", "stage": "live"},
                {"id": 2, "name": "Borealis", "stage": "live"},
                {"id": 3, "name": "Cassini", "stage": "draft"},
            ]);
            let app = axum::Router::new().route("/openapi.json", get(move || async move { axum::Json(doc) })).route(
                "/api/projects",
                get(move |q: axum::extract::Query<BTreeMap<String, String>>| async move {
                    let rows = projects.as_array().unwrap().clone();
                    let kept: Vec<Value> = rows.into_iter().filter(|r| q.iter().all(|(k, v)| r.get(k).and_then(|x| x.as_str()) == Some(v))).collect();
                    axum::Json(Value::Array(kept))
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            addr_tx.send(listener.local_addr().unwrap()).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    format!("http://{}", addr_rx.recv().expect("the second app bound a port"))
}

/// The generality claim, tested rather than asserted: a selector over an entity this codebase has
/// never heard of, resolved through the operation the world model names.
#[test]
fn selectors_work_on_an_app_that_is_not_ours() {
    let base = serve_second_app();
    let w = wants_file("second", "task(project=each(project())).exists\n");
    let (ok, out, err) = rwmcp(&["--app", &base, "--wants", w.to_str().unwrap(), "--json"]);
    assert!(ok, "{err}");
    let v: Value = serde_json::from_str(&out).unwrap();

    // each(project()) became one want per project, named by the field the resolver looks up by.
    let wants = v["intent"]["wants"].as_array().unwrap();
    let text = wants[0].as_str().unwrap();
    assert!(text.contains("project(name='Apollo')") && text.contains("project(name='Cassini')"), "{text}");

    // Three projects, each needing a resolve and a create.
    let ops: Vec<&str> = v["plan"]["nodes"].as_array().unwrap().iter().map(|n| n["op"].as_str().unwrap()).collect();
    assert_eq!(ops.iter().filter(|o| **o == "createTask").count(), 3);
    assert_eq!(ops.iter().filter(|o| **o == "listProjects").count(), 3);

    // And a filter the resolver accepts is passed through to it, not applied afterwards.
    let w = wants_file("second-filter", "task(project=each(project(stage='draft'))).exists\n");
    let (ok, out, err) = rwmcp(&["--app", &base, "--wants", w.to_str().unwrap(), "--json"]);
    assert!(ok, "{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    let text = v["intent"]["wants"][0].as_str().unwrap();
    assert!(text.contains("Cassini") && !text.contains("Apollo"), "only the draft project: {text}");
}

/// A surface the app never mentions is a typo. Saying so at the flag beats failing much later
/// with "no surface can do this", which points at the operation instead.
#[test]
fn an_unknown_surface_is_named_at_the_flag() {
    let base = serve(2);
    let w = wants_file("surf", "invoice(customer=customer(name='Acme')).exists\n");
    let w = w.to_str().unwrap();
    let (ok, _, err) = rwmcp(&["--app", &base, "--wants", w, "--surfaces", "api,voice"]);
    assert!(!ok);
    assert!(err.contains("'voice'") && err.contains("It offers:"), "{err}");
    assert_eq!(code(&["--app", &base, "--wants", w, "--surfaces", "api,voice"]), 10);
    assert_eq!(code(&["--app", &base, "--wants", w, "--surfaces", "api,a11y"]), 0, "real surfaces still pass");
}
