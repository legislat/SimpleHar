use super::*;
use crate::engine::ReadinessSpec;
use crate::ownership::{SKIP_INVALID_NAME, SKIP_NOT_NINFER, SKIP_UNCLAIMED};

#[test]
fn a_stale_ticket_never_owns_the_port_it_renames() {
    assert_eq!(table_verdict(None, 7), TableVerdict::Unowned);
    assert_eq!(table_verdict(Some(7), 7), TableVerdict::Ours);
    assert_eq!(
        table_verdict(Some(8), 7),
        TableVerdict::Stale { found: 8 },
        "the previous ticket must not be able to kill, or confirm away, the successor"
    );
}

#[test]
fn container_rm_uses_the_shared_wsl_prefix() {
    let (program, args) = container_rm("Ubuntu", "ops", "ninfer-x");
    assert_eq!(program, "wsl.exe");
    assert_eq!(
        [vec![program], args].concat(),
        vec!["wsl.exe", "-d", "Ubuntu", "-u", "ops", "--", "docker", "rm", "-f", "ninfer-x",]
    );
}

#[test]
fn container_port_uses_the_shared_wsl_prefix() {
    let (program, args) = container_port("Ubuntu", "ops", "ninfer-x");
    assert_eq!(program, "wsl.exe");
    assert_eq!(
        [vec![program], args].concat(),
        vec!["wsl.exe", "-d", "Ubuntu", "-u", "ops", "--", "docker", "port", "ninfer-x",]
    );
}

/// R5 pin of the real-path command shape: both reconciles (boot reclaim and
/// the silent R5 probe) build their docker command through
/// [`container_ps_command`] alone, so this exact argv IS the reconcile's
/// entire docker reach. The seam pin records the verbs the probe asked the
/// seam for; this one pins the argv the probe can actually build — the
/// never-kill guarantee's other half.
#[test]
fn container_ps_is_the_reconciles_entire_docker_vocabulary() {
    let (program, args) = container_ps_command("Ubuntu", "ops");
    assert_eq!(program, "wsl.exe");
    assert_eq!(
        [vec![program], args].concat(),
        vec![
            "wsl.exe",
            "-d",
            "Ubuntu",
            "-u",
            "ops",
            "--",
            "docker",
            "ps",
            "--format",
            "{{.Names}}",
        ]
    );
}

/// Ruling 1(b) parse: a mapping off the offered port becomes evidence, the
/// offered port itself stays silent (the `$3` contract held), and docker's
/// error text never becomes invented evidence.
#[test]
fn publish_evidence_names_a_foreign_mapping_and_stays_silent_otherwise() {
    let text = "8808/tcp -> 127.0.0.1:18112\n8808/tcp -> [::]:18112\n";
    let evidence = published_elsewhere_evidence("ninfer-x", 18109, text)
        .expect("a mapping off the offered port is evidence");
    assert!(evidence.contains("docker publishes ninfer-x"), "{evidence}");
    assert!(evidence.contains("127.0.0.1:18112"), "{evidence}");
    assert!(
        evidence.contains("not the harness-offered port 18109"),
        "{evidence}"
    );
    assert!(
        published_elsewhere_evidence("ninfer-x", 18112, text).is_none(),
        "the offered port IS published: nothing to report"
    );
    assert!(published_elsewhere_evidence("ninfer-x", 18109, "").is_none());
    assert!(
        published_elsewhere_evidence("ninfer-x", 18109, "Error: No such container: ninfer-x\n")
            .is_none(),
        "docker's error text must never become invented evidence"
    );
}

/// Ownership scope of ruling 1(b): the publish query is claim-ledger-strict,
/// so a name this harness never claimed is never even addressed, and blank
/// WSL coordinates close the door exactly as they close the launch.
#[tokio::test]
async fn a_container_the_harness_never_claimed_is_never_queried_for_evidence() {
    let dir = crate::testutil::temp_dir("publish-gate");
    let ledger = dir.join("ninfer-claims.json");
    let runtime = ProcessRuntime::new()
        .with_wsl("Ubuntu", "ops")
        .with_claims_ledger(&ledger);
    assert!(
        runtime
            .published_elsewhere("ninfer-x", 18109)
            .await
            .is_none(),
        "an unclaimed name must not be queried"
    );
    let blank = ProcessRuntime::new().with_claims_ledger(&ledger);
    assert!(
        blank.published_elsewhere("ninfer-x", 18109).await.is_none(),
        "blank coordinates must not reach wsl.exe"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Template adoption (2026-10-31): the staged path is content-addressed, lives
/// only under `~/.simplehar/autostart/`, and is a single unquoted word to the
/// distro shell — so a hostile slug or a re-render can neither escape the
/// directory nor collide with another model's staged file.
#[test]
fn a_staged_script_path_is_content_addressed_and_shell_bare() {
    let first = staged_script_path("ninfer-x", "echo a");
    let second = staged_script_path("ninfer-x", "echo b");
    assert!(
        first.starts_with("~/.simplehar/autostart/ninfer-x-"),
        "{first}"
    );
    assert!(first.ends_with(".sh"), "{first}");
    assert_ne!(
        first, second,
        "different bytes must not overwrite each other"
    );
    assert_eq!(
        first,
        staged_script_path("ninfer-x", "echo a"),
        "same bytes, same name"
    );
    for c in first.chars() {
        assert!(
            c.is_ascii_alphanumeric() || matches!(c, '~' | '/' | '.' | '_' | '-'),
            "the path must be one bare word for the distro shell: {c}"
        );
    }
    let slugged = staged_script_path("bad name;rm", "x");
    assert!(slugged.contains("bad_name_rm"), "{slugged}");
    assert!(
        !slugged.contains(' ') && !slugged.contains(';'),
        "{slugged}"
    );
    // The staging driver carries the body base64-only: a script full of shell
    // metal must not be able to contribute a single metacharacter anywhere the
    // login shell could re-tokenize it (the blob lives inside a quoted heredoc
    // in a file, never on a command line).
    assert_eq!(script_content_hash(""), script_content_hash(""));
    assert_ne!(script_content_hash("a"), script_content_hash("b"));
}

/// Ruling 19's staging door, tested directly (review 25 [2]+[3]): a name
/// that is not filesystem-bare must still prune its OWN pool — the writer
/// sanitizes, so the prune has to match the sanitized prefix or every yaml
/// edit stages a permanent file — and one slug's pool must never touch
/// another's.
#[test]
fn staged_profiles_prune_their_own_sanitized_pool_and_nobody_elses() {
    let root = crate::testutil::temp_dir("strata-stage");
    let slug = "strata iq3s";
    let stage = |i: u32| -> PathBuf {
        let text = format!("{{\"render\": {i}}}");
        let placeholder = staged_profile_name(slug, &text);
        let landed =
            stage_rendered_profile("strata", &root, slug, &text, &placeholder).expect("stages");
        assert_eq!(
            std::fs::read_to_string(&landed).expect("reads back"),
            text,
            "the staged bytes are the rendered bytes"
        );
        // Distinct mtimes keep the newest-first prune order observable.
        std::thread::sleep(std::time::Duration::from_millis(20));
        landed
    };
    let first = stage(0);
    for i in 1..12 {
        stage(i);
    }
    let last = stage(12);
    let names: Vec<String> = std::fs::read_dir(&root)
        .expect("root readable")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    let mine: Vec<&String> = names
        .iter()
        .filter(|n| n.starts_with("strata_iq3s-"))
        .collect();
    assert_eq!(
        mine.len(),
        STAGED_PROFILE_KEEP,
        "the prune must match the writer's sanitized name: {names:?}"
    );
    assert!(last.is_file(), "the newest render is never pruned");
    assert!(!first.is_file(), "the oldest render is the first to go");
    // A second slug stages beside the first and keeps its own pool.
    let other_text = "{\"other\": true}";
    let other = stage_rendered_profile(
        "strata",
        &root,
        "other",
        other_text,
        &staged_profile_name("other", other_text),
    )
    .expect("stages");
    assert!(other.is_file(), "one slug's prune never touches another's");
    // The name binds its bytes: a hand-built spec cannot stage one
    // profile under another model's name.
    let wrong = stage_rendered_profile(
        "strata",
        &root,
        slug,
        "{\"n\": 1}",
        "strata_iq3s-0000000000000000.json",
    );
    assert!(
        wrong.is_err_and(|reason| reason.contains("content address")),
        "a placeholder that is not these bytes' address must refuse"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_config_slot_rewrites_only_a_flag_that_carries_a_value() {
    let argv = vec!["py".to_string(), "--config".to_string(), "ph".to_string()];
    let rewritten = rewrite_config_slot(argv, "C:\\s.json").expect("rewrites");
    assert_eq!(rewritten[2], "C:\\s.json");
    assert!(
        rewrite_config_slot(vec!["py".to_string(), "--port".to_string()], "p").is_none(),
        "argv without --config cannot carry a staged path"
    );
    assert!(
        rewrite_config_slot(vec!["py".to_string(), "--config".to_string()], "p").is_none(),
        "a --config with no value slot would silently add a path to another flag"
    );
}

/// The spawn wiring (review 25 [2]): a spec carrying a rendered profile
/// stages under the runtime's profile root BEFORE any spawn attempt, and
/// the `--config` placeholder the argv carries is the name those bytes
/// hash to — a broken placeholder extraction would surface as the staging
/// refusal, which this test pins absent by reaching the spawn error.
#[tokio::test]
async fn a_rendered_profile_stages_under_the_profile_root_before_the_child() {
    let dir = crate::testutil::temp_dir("strata-spawn-stage");
    let profiles = dir.join("profiles");
    let text = "{\"model_name\": \"probe\"}".to_string();
    let slug = "strata iq3s";
    let placeholder = staged_profile_name(slug, &text);
    let runtime = ProcessRuntime::new().with_profiles_root(&profiles);
    let spec = LaunchSpec {
        argv: vec![
            "C:\\simpleharn-absent\\no-such-engine.exe".to_string(),
            "-m".to_string(),
            "serve.server".to_string(),
            "--engine".to_string(),
            "strata".to_string(),
            "--host".to_string(),
            "127.0.0.1".to_string(),
            "--config".to_string(),
            placeholder.clone(),
            "--port".to_string(),
            "18197".to_string(),
        ],
        cwd: dir.clone(),
        env: spec_map(&[]),
        log_name: slug.to_string(),
        readiness: ReadinessSpec::Http {
            path: "/health".to_string(),
            expect_status: 200,
        },
        container: None,
        script: None,
        rendered_profile: Some(crate::engine::RenderedProfile {
            engine: "strata",
            text: text.clone(),
        }),
        custody: None,
    };
    let tk = SpawnTicket {
        generation: 1,
        kind: ResourceKind::ChildProcess,
        pid: 0,
        port: 18_197,
    };
    let error = runtime
        .spawn(&spec, tk)
        .await
        .expect_err("the absent program cannot start");
    let SpawnError::SpawnFailed { reason } = error else {
        panic!("expected a spawn failure, got {error:?}");
    };
    assert!(
        reason.contains("cannot spawn C:\\simpleharn-absent"),
        "the run must reach the spawn step: {reason}"
    );
    assert!(
        !reason.contains("cannot stage"),
        "staging completed, so the argv placeholder matched its bytes: {reason}"
    );
    let staged = profiles.join(&placeholder);
    assert!(
        staged.is_file(),
        "the rendered bytes must sit at the name the argv carried: {}",
        staged.display()
    );
    assert_eq!(std::fs::read_to_string(&staged).expect("reads back"), text);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `TASK-BASALT` stage 1: the staging roots are per-engine — the strata
/// door of ruling 19 keeps its byte-identical tail, basalt stages beside
/// it, and the two pools can never prune each other's files.
#[test]
fn rendered_profile_roots_are_per_engine_and_distinct() {
    let strata = rendered_profiles_dir("strata");
    assert_eq!(
        strata_profiles_dir(),
        strata,
        "the strata spelling is unchanged"
    );
    let basalt = rendered_profiles_dir("basalt");
    assert!(strata.ends_with(".simplehar/strata-profiles"), "{strata:?}");
    assert!(basalt.ends_with(".simplehar/basalt-profiles"), "{basalt:?}");
    assert_ne!(strata, basalt);
}

/// Minimal `serve.server` stand-in: the exact `python -m serve.server`
/// shape the adapter launches. Reports `/health` `loaded: false` for its
/// first two requests (the readiness must wait), serves the alias on
/// `/v1/models`, and records its received argv for the spawn-path pin.
const FAKE_BASALT_SERVER: &str = r#"import http.server
import json
import os
import sys

args = sys.argv[1:]


def opt(name, default=None):
    if name in args:
        return args[args.index(name) + 1]
    for word in args:
        if word.startswith(name + "="):
            return word.split("=", 1)[1]
    return default


port = int(opt("--port"))
alias = opt("--alias", "anon")
with open(os.path.join(os.getcwd(), "argv.json"), "w", encoding="utf-8") as fh:
    json.dump(args, fh)

state = {"health": 0}


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *rest):
        pass

    def do_GET(self):
        if self.path == "/health":
            state["health"] += 1
            self._send(200, {"status": "ok", "loaded": state["health"] > 2})
        elif self.path == "/v1/models":
            self._send(200, {"object": "list", "data": [{"id": alias, "object": "model"}]})
        else:
            self._send(404, {"error": "no such path"})

    def _send(self, code, body):
        raw = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)


http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
"#;

/// The first real `python`/`py` on PATH, as an absolute path (the row
/// validation demands one), or `None` — the pin then skips itself: it is
/// a launch-path pin, not a language claim.
fn find_python() -> Option<PathBuf> {
    for program in ["python.exe", "py.exe"] {
        let located = std::process::Command::new("where.exe")
            .arg(program)
            .output()
            .ok()?;
        if !located.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&located.stdout);
        let path = text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(PathBuf::from)?;
        if !path.is_absolute() || !path.is_file() {
            continue;
        }
        let ok = std::process::Command::new(&path)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if ok {
            return Some(path);
        }
    }
    None
}

/// The stage-1 end-to-end pin, cardless: a real `python -m serve.server`
/// child (a stand-in module, no engine, no model, loopback only), spawned
/// through `ProcessRuntime` from a `BasaltAdapter` spec. Pins the whole
/// chain the scheduler will ride: exact argv with the `--config` slot
/// rewritten to the staged bytes under the BASALT root, `loaded: false`
/// held at `NotYet` then released, the served id verified and discovered,
/// and a clean kill with proven release.
#[tokio::test]
async fn a_fake_python_basalt_launch_serves_the_verified_id_end_to_end() {
    let Some(python) = find_python() else {
        eprintln!("skip: no python on PATH to run the serve.server stand-in");
        return;
    };
    let dir = crate::testutil::temp_dir("basalt-py");
    let serve = dir.join("serve");
    std::fs::create_dir_all(&serve).expect("serve pkg");
    std::fs::write(serve.join("__init__.py"), b"").expect("pkg init");
    std::fs::write(serve.join("server.py"), FAKE_BASALT_SERVER).expect("server");
    let weights = dir.join("m.basalt");
    std::fs::write(&weights, b"not a real artifact").expect("weights");
    let engine_bin = dir.join("basalt-engine.exe");
    std::fs::write(&engine_bin, b"never executed by the stand-in").expect("engine");
    let profiles = dir.join("basalt-profiles");
    let raw = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    let model = crate::model::ModelEntry {
        name: "basalt-fake".to_string(),
        engine: "basalt".to_string(),
        category: crate::config::CategoryId::Work,
        vram_gb: Some(10.0),
        ram_gb: None,
        gguf_size_gb: None,
        cost: crate::model::CostClass::Fixed,
        vram_gate: crate::model::VramGate::default(),
        load_timeout_s: None,
        min_turn_s: None,
        measured_gb: None,
        measured_min_gb: None,
        measured_offload: None,
        measured_attribution: None,
        measured_profile: None,
        measured_ram_gb: None,
        measured_ram_min_gb: None,
        fit_gb: None,
        model_ref: Some(weights.to_string_lossy().into_owned()),
        serve_model: Some("fake-alias".to_string()),
        file: None,
        raw: raw(&[
            ("basalt_python", python.to_str().expect("utf-8")),
            ("basalt_exe", engine_bin.to_str().expect("utf-8")),
            ("basalt_cwd", dir.to_str().expect("utf-8")),
        ]),
    };
    // Ephemeral loopback port: bind, read, release (the SE test rule: no
    // protected port, no fixed bind).
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral port");
        listener.local_addr().expect("addr").port()
    };
    let spec =
        crate::engine::EngineAdapter::spec(&crate::engine::basalt::BasaltAdapter, &model, port)
            .expect("fixture spec");
    let profile = spec.rendered_profile.clone().expect("rendered");
    assert_eq!(profile.engine, "basalt");
    let runtime = ProcessRuntime::new().with_rendered_profiles_root("basalt", &profiles);
    let tk = SpawnTicket {
        generation: 1,
        kind: ResourceKind::ChildProcess,
        pid: 0,
        port,
    };
    let spawned = runtime
        .spawn(&spec, tk.clone())
        .await
        .expect("the stand-in starts");
    assert_eq!(spawned.port, port);
    let staged = profiles.join(staged_profile_name("basalt-fake", &profile.text));
    assert!(
        staged.is_file(),
        "the render staged under the BASALT root: {profiles:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&staged).expect("reads back"),
        profile.text
    );
    // The child received the staged path, not the placeholder.
    let argv_path = dir.join("argv.json");
    let mut argv: Option<Vec<String>> = None;
    for _ in 0..50 {
        if let Ok(text) = std::fs::read_to_string(&argv_path) {
            argv = Some(serde_json::from_str(&text).expect("child argv json"));
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let child_argv = argv.expect("the child recorded its argv");
    let config_at = child_argv
        .iter()
        .position(|arg| arg == "--config")
        .expect("--config");
    assert_eq!(
        child_argv[config_at + 1],
        staged.to_string_lossy(),
        "the --config slot carries the staged absolute path"
    );
    assert_eq!(
        child_argv
            .get(
                child_argv
                    .iter()
                    .position(|arg| arg == "--port")
                    .expect("--port")
                    + 1
            )
            .expect("port value"),
        &port.to_string()
    );
    // Readiness: the stand-in answers `loaded: false` until its third
    // `/health`, so the chain must first wait, then verify the id.
    let mut saw_not_yet = false;
    let mut ready = false;
    for _ in 0..100 {
        match runtime.probe(port, &spec.readiness).await {
            Probe::Ready => {
                ready = true;
                break;
            }
            Probe::NotYet { .. } => {
                saw_not_yet = true;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Probe::Failed { reason } => panic!("terminal probe failure: {reason}"),
        }
    }
    assert!(ready, "the chain reached Ready inside the window");
    assert!(
        saw_not_yet,
        "`loaded: false` must have held the chain at NotYet"
    );
    assert_eq!(
        runtime.discover_model_id(port).await.as_deref(),
        Some("fake-alias"),
        "the discovered id is the alias the row served"
    );
    runtime.kill(&tk).await.expect("kill our child");
    let mut released = false;
    for _ in 0..100 {
        if runtime
            .release_confirmed(&tk)
            .await
            .expect("release evidence is checkable")
        {
            released = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(released, "death of the child is provable");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The keep-N prune matches the slug LITERALLY (review 2026-10-31, MEDIUM):
/// `valid_container_name` admits `.`, and raw interpolation made `har.a`'s
/// prune hit `har-a-<hex>.sh` and `harxa-<hex>.sh` on the distro's findutils —
/// two models sharing one keep-10 pool, the exact class the 16-hex content
/// address was added to kill. Falsifier: feed the slug to the regex unescaped
/// and the raw-`.` forms appear in the driver.
#[test]
fn the_prune_regex_escapes_the_slug_so_dots_cannot_wildcard_a_sibling_pool() {
    assert_eq!(escape_posix_regex("a.b*c[x]"), r"a\.b\*c\[x\]");
    let script = "echo body";
    let hash = fnv1a64(script.as_bytes());
    let build = |slug: &str| {
        let file_name = format!("{slug}-{:016x}.sh", hash);
        staging_driver(script, &file_name, "Zm9v")
    };
    let dotted = build("har.a");
    assert!(
        dotted.contains(r"regex '.*/har\.a-[0-9a-f]{16}\.sh'"),
        "the slug's dot must reach find as an escaped literal:\n{dotted}"
    );
    assert!(
        !dotted.contains(".*/har.a-"),
        "an unescaped dot is the any-character operator"
    );
    // The temp names slug AND hash, and both temps are removed before the
    // redirects — a pre-existing loose-mode temp must not propagate its mode
    // through `>` (which preserves an existing file's mode), and identical
    // concurrent stagings must collide loudly rather than interleave (LOW).
    let tmp_line = format!(r#"tmp="$dir/.staging.har.a-{hash:x}.tmp""#);
    assert!(dotted.contains(&tmp_line), "{dotted}");
    let pre_rm = dotted
        .find("rm -f -- \"$tmp\" \"$tmp.b64\"")
        .unwrap_or_else(|| panic!("no pre-clean in:\n{dotted}"));
    let first_write = dotted.find("cat >").expect("heredoc present");
    assert!(pre_rm < first_write, "the pre-clean must precede any write");
}

/// The staged-path rewrite anchors on `bash` AFTER the last `--` (review
/// 2026-10-31, LOW): a config distro/user literally `bash` must not move the
/// rewrite to the wrong slot, and a spec that does not run `-- bash <slot>` is
/// refused, never launched against the unwritten placeholder.
#[test]
fn the_script_slot_rewrite_anchors_past_the_wsl_separator() {
    let argv = || {
        vec![
            "wsl.exe".to_string(),
            "-d".to_string(),
            "bash".to_string(),
            "-u".to_string(),
            "ops".to_string(),
            "--".to_string(),
            "bash".to_string(),
            "PLACE".to_string(),
            "b".to_string(),
        ]
    };
    let rewritten = rewrite_script_slot(argv(), "STAGED".to_string()).expect("anchored");
    assert_eq!(rewritten[7], "STAGED");
    assert_eq!(rewritten[2], "bash", "the decoy distro word is untouched");

    let mut no_dashes = argv();
    no_dashes[5] = "-u".to_string();
    assert_eq!(
        rewrite_script_slot(no_dashes, "STAGED".to_string()),
        None,
        "no `-- bash` anchor means no rewrite, so the spawn is refused"
    );
    let mut trailing = argv();
    trailing.truncate(7);
    assert_eq!(
        rewrite_script_slot(trailing, "STAGED".to_string()),
        None,
        "bash with no slot after it must never be rewritten past the end"
    );
}

/// The staging verifier must be the same function on both sides of the pipe:
/// these vectors were captured from the operator distro's own coreutils 9.4
/// `cksum` (`printf X | cksum`), not from a re-implementation — the driver
/// compares its `cksum < file` output against exactly these pairs.
#[test]
fn the_staging_checksum_matches_the_distro_cksum() {
    assert_eq!(cksum_expectation(b""), (4_294_967_295, 0));
    assert_eq!(cksum_expectation(b"123456789"), (930_766_865, 9));
    assert_eq!(cksum_expectation(b"a"), (1_220_704_766, 1));
    assert_eq!(cksum_expectation(b"hello world"), (1_135_714_720, 11));
    assert_eq!(cksum_expectation("café".as_bytes()), (2_824_353_162, 5));
    assert_ne!(
        cksum_expectation(b"echo a").0,
        cksum_expectation(b"echo b").0,
        "one flipped byte must move the checksum"
    );
}

#[test]
fn the_reap_duty_retires_only_on_a_confirmed_reap() {
    let name = "ninfer-x".to_string();
    assert_eq!(should_reap(false, false, Some(&name)), Some(name.clone()));
    assert_eq!(
        should_reap(true, true, Some(&name)),
        None,
        "a re-kill must not re-rm a confirmed-gone container (review, 2026-10-26)"
    );
    assert_eq!(
        should_reap(true, false, Some(&name)),
        Some(name),
        "REV7-01: an unconfirmed reap re-arms the duty for the next explicit kill"
    );
    assert_eq!(
        should_reap(false, false, None),
        None,
        "process engines reap nothing"
    );
}

#[test]
fn release_evidence_rules() {
    // Process engines: exit + closed port (G12 pair). Containers: the client
    // exit proves nothing; the closed port after reaping is the evidence.
    assert!(release_ready(false, true, true));
    assert!(!release_ready(false, true, false));
    assert!(!release_ready(false, false, true));
    assert!(release_ready(true, false, true));
    assert!(!release_ready(true, false, false));
}

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn early_exit_names_the_missing_llama_model_file() {
    let dir = crate::testutil::temp_dir("early-exit");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let spec_argv = argv(&[
        "llama-server.exe",
        "-m",
        "Z:\\definitely-not-here\\doomed.gguf",
        "--host",
        "127.0.0.1",
        "--port",
        "18199",
    ]);
    let (reason, missing) = early_exit_report(&spec_argv, "exit code: 1");
    assert_eq!(
        reason,
        "model file not found: Z:\\definitely-not-here\\doomed.gguf (engine exited early: \
             exit code: 1)"
    );
    assert_eq!(
        missing.as_deref(),
        Some("Z:\\definitely-not-here\\doomed.gguf")
    );

    // A profile-shaped strata argv names its --config when the profile is gone,
    // and never the Python module token after `-m`.
    let strata = argv(&[
        "python.exe",
        "-m",
        "serve.server",
        "--engine",
        "strata",
        "--config",
        "Z:\\nope\\profile.json",
        "--port",
        "18199",
    ]);
    let (reason, missing) = early_exit_report(&strata, "exit code: 1");
    assert!(
        reason.starts_with("model file not found: Z:\\nope\\profile.json"),
        "{reason}"
    );
    assert!(
        !reason.contains("serve.server"),
        "the module is not a model path"
    );
    assert_eq!(missing.as_deref(), Some("Z:\\nope\\profile.json"));

    // An existing file must NOT be named: the reason stays the bare status.
    let real = dir.join("here.gguf");
    std::fs::write(&real, b"x").expect("seed");
    let ok_argv = argv(&["llama-server.exe", "-m", real.to_str().expect("utf8")]);
    let (reason, missing) = early_exit_report(&ok_argv, "exit code: 3");
    assert_eq!(reason, "engine process exited early (exit code: 3)");
    assert_eq!(missing, None);

    // `scheme://` sentinels name no local file: exempt, bare status.
    let sentinel = argv(&["llama-server.exe", "-m", "hf://org/repo/x.gguf"]);
    let (reason, missing) = early_exit_report(&sentinel, "0x1");
    assert_eq!(reason, "engine process exited early (0x1)");
    assert_eq!(missing, None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stderr_excerpt_is_bounded_and_char_safe() {
    assert_eq!(excerpt(b"short"), "short");
    assert_eq!(excerpt("x".repeat(400).as_bytes()).len(), 303);
    let unicode = "あ".repeat(200); // 600 bytes: must cut on a char boundary
    let cut = excerpt(unicode.as_bytes());
    assert!(cut.chars().all(|c| c == 'あ' || c == '.'), "{cut}");
    assert!(cut.len() <= 303);
}

#[test]
fn log_pruning_keeps_the_newest_per_model_and_spares_other_models() {
    let dir = crate::testutil::temp_dir("logprune");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp logs dir");
    // Six captures for model `m`, oldest first (lowest index = oldest mtime).
    for i in 0..6 {
        std::fs::write(dir.join(format!("m-{i}.log")), b"x").expect("seed client log");
        std::fs::write(dir.join(format!("m-{i}.container.log")), b"x").expect("seed container log");
        std::thread::sleep(Duration::from_millis(10));
    }
    // A different model's logs must survive untouched (the global-cap defect).
    std::fs::write(dir.join("other-0.log"), b"x").expect("seed foreign log");
    prune_model_logs(&dir, "m", 3);
    let names: Vec<String> = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    let m_clients: Vec<_> = names
        .iter()
        .filter(|n| n.starts_with("m-") && n.ends_with(".log") && !n.ends_with(".container.log"))
        .collect();
    let m_containers: Vec<_> = names
        .iter()
        .filter(|n| n.ends_with(".container.log"))
        .collect();
    assert_eq!(
        m_clients.len(),
        3,
        "client captures capped per model: {m_clients:?}"
    );
    assert_eq!(
        m_containers.len(),
        3,
        "container snapshots capped per model: {m_containers:?}"
    );
    assert!(
        names.iter().any(|n| n == "other-0.log"),
        "another model's forensic log is never evicted: {names:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// R2-08-03 falsifier: one forensic pool key, `sanitize_slug`, for both
/// mint sites. Model `qwen 7b` must land its REAL spawned capture under the
/// sanitized stem `qwen_7b-`, and its keep-N prune must bound the captures
/// AND the `qwen_7b-*.container.log` snapshots in one pool — while a
/// different slug (`qwen-7b`) keeps every file it owns. Falsifier (pre-fix
/// shape): the capture mint used the raw `spec.log_name`, so the real
/// capture landed as `qwen 7b-<epoch>.log`, the prune of that model matched
/// only raw-named files, and the nine seeded `qwen_7b-` captures survived
/// the prune untouched — every count assert below goes red.
#[tokio::test]
async fn captures_and_snapshots_of_one_model_share_one_sanitized_pool() {
    let dir = harness_logs_dir();
    std::fs::create_dir_all(&dir).expect("logs dir");
    // Sweep this test's own pool names first: a previously interrupted or
    // failing run must not decide this one's outcome from stale litter.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("qwen 7b-")
                || name.starts_with("qwen_7b-")
                || name.starts_with("qwen-7b-")
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let seeds: Vec<String> = ["qwen_7b", "qwen-7b"]
        .iter()
        .flat_map(|pool| {
            (100_000u64..100_007).flat_map(move |epoch| {
                [
                    format!("{pool}-{epoch}.log"),
                    format!("{pool}-{epoch}.container.log"),
                ]
            })
        })
        .collect();
    for name in &seeds {
        std::fs::write(dir.join(name), b"x").expect("seed forensic file");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Both real mint paths for the space-bearing model name: the spawn's
    // capture and the container snapshot.
    let runtime = ProcessRuntime::new();
    runtime
        .spawn(
            &client_spec("cmd.exe", &["/c", "exit 0"], "qwen 7b"),
            child_ticket(18_344),
        )
        .await
        .expect("cmd.exe starts as a stand-in client");
    runtime.save_container_log("qwen 7b", "pool evidence").await;

    let names: Vec<String> = std::fs::read_dir(&dir)
        .expect("read logs dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    let pool_files = |stem: &str, container: bool| -> Vec<String> {
        names
            .iter()
            .filter(|n| {
                n.starts_with(&format!("{stem}-"))
                    && n.ends_with(if container { ".container.log" } else { ".log" })
                    && n.ends_with(".container.log") == container
            })
            .cloned()
            .collect()
    };
    assert!(
        names.iter().all(|n| !n.starts_with("qwen 7b-")),
        "the raw name may never mint a forensic file: {names:?}"
    );
    // One pool, both classes: keep-N capped by the REAL prune calls.
    let captured = pool_files("qwen_7b", false);
    let snapshots = pool_files("qwen_7b", true);
    assert_eq!(
        captured.len(),
        ENGINE_LOG_KEEP,
        "the shared pool caps the model's captures: {captured:?}"
    );
    assert_eq!(
        snapshots.len(),
        ENGINE_LOG_KEEP,
        "the shared pool caps the model's snapshots: {snapshots:?}"
    );
    let real_epoch_capture = captured.iter().any(|n| {
        n.strip_prefix("qwen_7b-")
            .and_then(|rest| rest.strip_suffix(".log"))
            .and_then(|epoch| epoch.parse::<u128>().ok())
            .is_some_and(|epoch| epoch > 1_000_000_000)
    });
    assert!(
        real_epoch_capture,
        "the spawn's real capture landed under the sanitized stem: {captured:?}"
    );
    let real_epoch_snapshot = snapshots.iter().any(|n| {
        n.strip_prefix("qwen_7b-")
            .and_then(|rest| rest.strip_suffix(".container.log"))
            .and_then(|epoch| epoch.parse::<u128>().ok())
            .is_some_and(|epoch| epoch > 1_000_000_000)
    });
    assert!(
        real_epoch_snapshot,
        "the snapshot landed in the same pool: {snapshots:?}"
    );
    // A different slug keeps every one of its files.
    assert_eq!(
        pool_files("qwen-7b", false).len(),
        7,
        "another model's captures are never pruned through this pool: {names:?}"
    );
    assert_eq!(
        pool_files("qwen-7b", true).len(),
        7,
        "another model's snapshots are never pruned through this pool: {names:?}"
    );
    for name in names.iter().filter(|n| {
        n.starts_with("qwen 7b-") || n.starts_with("qwen_7b-") || n.starts_with("qwen-7b-")
    }) {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

/// The classification the two budgets depend on. A rolled generation is named
/// `harness-<epoch>.log.2` — extension `2`, so an extension-only test would let it
/// escape the harness budget, and an extension-`log` test would hand it to the
/// engine budget. Falsifier: match on `extension()` in either predicate.
#[test]
fn harness_logs_are_never_engine_logs() {
    assert!(is_harness_log(std::path::Path::new(
        "logs/harness-1793.log"
    )));
    assert!(
        is_harness_log(std::path::Path::new("logs/harness-1793.log.2")),
        "a rolled generation is still the harness's own log"
    );
    assert!(!is_engine_log(std::path::Path::new(
        "logs/harness-1793.log"
    )));
    assert!(
        !is_engine_log(std::path::Path::new("logs/harness-1793.log.2")),
        "and is never budgeted twice, once per budget"
    );
    assert!(is_engine_log(std::path::Path::new("logs/llama-1793.log")));
    assert!(!is_harness_log(std::path::Path::new("logs/notes.txt")));
}

/// The per-model engine budget never evicts the harness boot log nor another
/// model's logs. The `is_engine_log`/`is_container_log` `!is_harness_log` guard is
/// pinned directly by `harness_logs_are_never_engine_logs`; this pins that the
/// prefix-scoped prune leaves the boot log and foreign models alone while capping
/// its own model to `keep`.
/// Falsifier: widen `matches_prefix` to ignore the separator (or drop the prefix
/// entirely) and the boot log disappears with `keep = 3`.
#[test]
fn the_engine_budget_never_prunes_the_harness_boot_log() {
    let dir = crate::testutil::temp_dir("logprune-h");
    std::fs::create_dir_all(&dir).expect("temp logs dir");
    for i in 0..6 {
        std::fs::write(dir.join(format!("engine-{i}.log")), b"x").expect("seed engine log");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::write(dir.join("harness-1793.log"), b"the refusal").expect("seed boot log");
    prune_model_logs(&dir, "engine", 3);
    let remaining: Vec<String> = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        remaining.contains(&"harness-1793.log".to_string()),
        "the boot log was evicted by the engine budget: {remaining:?}"
    );
    assert_eq!(
        remaining
            .iter()
            .filter(|name| name.starts_with("engine-"))
            .count(),
        3,
        "the engine cap is still enforced: {remaining:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The mirror leg: the harness budget prunes its own logs (rolled generations
/// included) and must not touch a single engine capture.
#[test]
fn the_harness_budget_prunes_only_harness_logs() {
    let dir = crate::testutil::temp_dir("logprune-e");
    std::fs::create_dir_all(&dir).expect("temp logs dir");
    for i in 0..4 {
        std::fs::write(dir.join(format!("harness-{i}.log")), b"x").expect("seed boot log");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::write(dir.join("harness-9.log.2"), b"x").expect("seed rolled generation");
    for i in 0..2 {
        std::fs::write(dir.join(format!("engine-{i}.log")), b"x").expect("seed engine log");
        std::thread::sleep(Duration::from_millis(10));
    }
    prune_harness_log_files(&dir, 2);
    let remaining: Vec<String> = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        remaining
            .iter()
            .filter(|name| name.starts_with("harness-"))
            .count(),
        2,
        "harness cap enforced across rolled generations: {remaining:?}"
    );
    assert_eq!(
        remaining
            .iter()
            .filter(|name| name.starts_with("engine-"))
            .count(),
        2,
        "not one engine capture may be lost to the harness budget: {remaining:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn claims_with(names: &[&str]) -> ContainerClaims {
    let dir = crate::testutil::temp_dir("claims-plan");
    let mut claims = ContainerClaims::load(dir.join("ninfer-claims.json"));
    for name in names {
        assert!(claims.claim(name), "test seed name must be valid");
    }
    claims
}

fn candidate(engine: &str, name: &str) -> ContainerCandidate {
    ContainerCandidate {
        engine: engine.to_string(),
        name: name.to_string(),
    }
}

fn verdicts(plan: &[(ContainerCandidate, ReapDecision)]) -> Vec<(&str, Option<&'static str>)> {
    plan.iter()
        .map(|(c, d)| {
            (
                c.name.as_str(),
                match d {
                    ReapDecision::Reap => None,
                    ReapDecision::Skip { reason } => Some(*reason),
                },
            )
        })
        .collect()
}

#[test]
fn boot_reclaim_touches_only_catalog_owned_names() {
    // Reaping stays ownership-scoped (§5): a running container is reclaimed only
    // when the operator's own ModelConf declares that exact name — never by
    // prefix, never by wildcard. `someone-elses-container` never appears.
    let present: Vec<String> = ["SE-nin-bonsai", "ninfer-bonsai", "someone-elses-container"]
        .map(String::from)
        .to_vec();
    let claims = claims_with(&["SE-nin-bonsai"]);
    let plan = reclaim_plan(&present, &[candidate("ninfer", "SE-nin-bonsai")], &claims);
    assert_eq!(verdicts(&plan), vec![("SE-nin-bonsai", None)]);
    assert!(reclaim_plan(&present, &[], &claims).is_empty());
}

#[test]
fn boot_reclaim_skips_every_container_lacking_ownership_evidence() {
    // Chunk 10 [1] reproduced: a running container named by SOME yaml is still
    // untouchable without all three gates — each failure SKIPs with its reason.
    // (The invalid name is unclaimable, so the claim gate cannot even be reached:
    // the shape gate fires first and the container is left running.)
    let present: Vec<String> = [
        "ninfer-claimed",
        "ninfer-unclaimed",
        "llama-named",
        "_illegal",
    ]
    .map(String::from)
    .to_vec();
    let claims = claims_with(&["ninfer-claimed", "llama-named"]);
    let plan = reclaim_plan(
        &present,
        &[
            candidate("ninfer", "ninfer-claimed"),
            candidate("ninfer", "ninfer-unclaimed"),
            candidate("llama", "llama-named"),
            candidate("ninfer", "_illegal"),
        ],
        &claims,
    );
    assert_eq!(
        verdicts(&plan),
        vec![
            ("ninfer-claimed", None),
            ("ninfer-unclaimed", Some(SKIP_UNCLAIMED)),
            ("llama-named", Some(SKIP_NOT_NINFER)),
            ("_illegal", Some(SKIP_INVALID_NAME)),
        ]
    );
}

#[test]
fn boot_reclaim_never_reaps_an_id_shaped_name_even_when_a_stale_ledger_holds_it() {
    // Chunk 10 [4] on the boot path: `docker rm -f 1234abc` deletes whichever
    // *unrelated* container's id merely starts with those characters. The shape gate
    // therefore has to outrank any claim an older ledger still carries, so the seed
    // is written behind `claim` (which rightly refuses such names).
    let dir = crate::testutil::temp_dir("claims-stale");
    let path = dir.join("ninfer-claims.json");
    let stale: BTreeSet<String> = ["1234abc".to_string()].into_iter().collect();
    let bytes = serde_json::to_vec(&stale).expect("seed a stale ledger");
    std::fs::write(&path, bytes).expect("write the stale ledger");
    let claims = ContainerClaims::load(&path);
    assert!(
        claims.contains("1234abc"),
        "the hazard is a ledger that does hold the name"
    );
    let present = vec!["1234abc".to_string()];
    let plan = reclaim_plan(&present, &[candidate("ninfer", "1234abc")], &claims);
    assert_eq!(
        verdicts(&plan),
        vec![("1234abc", Some(SKIP_INVALID_NAME))],
        "an ID-shaped name is never destroyed, claim or no claim"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn boot_reclaim_never_judges_a_container_that_is_not_running() {
    // Presence in `docker ps` is the precondition: an absent declared name
    // produces no verdict at all, not even a skip line.
    let claims = claims_with(&["ninfer-a"]);
    let plan = reclaim_plan(&[], &[candidate("ninfer", "ninfer-a")], &claims);
    assert!(plan.is_empty());
}

#[test]
fn stranded_plan_reports_only_an_unclaimed_catalog_match() {
    // R5: a claimed match is a running harness engine (normal, silent); a
    // running container no catalog row names is foreign (out of report);
    // only the declared-but-unclaimed match strands VRAM.
    let present: Vec<String> = ["ninfer-kept", "ninfer-stranded", "foreign-x"]
        .map(String::from)
        .to_vec();
    let declared: Vec<String> = ["ninfer-kept".to_string(), "ninfer-stranded".to_string()].to_vec();
    let claims = claims_with(&["ninfer-kept"]);
    assert_eq!(
        stranded_plan(&present, &declared, &claims),
        ["ninfer-stranded".to_string()].into_iter().collect(),
        "the claimed name is an engine running normally; the foreign one is not ours"
    );
}

#[test]
fn stranded_plan_stands_down_when_nothing_is_declared_or_running() {
    // Empty ledger (no claim file) + no running containers = silence: the
    // plan is empty for every combination of the two empty sides.
    let empty_claims = claims_with(&[]);
    let declared = vec!["ninfer-a".to_string()];
    assert!(stranded_plan(&[], &declared, &empty_claims).is_empty());
    assert!(
        stranded_plan(&["ninfer-a".to_string()], &[], &empty_claims).is_empty(),
        "a running name no row declares is foreign, never reported"
    );
    assert!(stranded_plan(&[], &[], &empty_claims).is_empty());
}

#[tokio::test]
async fn the_reconcile_probe_names_the_stranded_container_and_asks_docker_only_for_ps() {
    // The G4 pin: the report-only reconcile's entire docker vocabulary is
    // `ps`. Every verb it asked the seam for is recorded — a `rm`, `kill`
    // or `stop` sneaking into the reconcile would surface here.
    let dir = crate::testutil::temp_dir("r5-probe");
    let ledger = dir.join("ninfer-claims.json");
    {
        let mut claims = ContainerClaims::load(&ledger);
        assert!(claims.claim("ninfer-kept"));
    }
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runtime = ProcessRuntime::new()
        .with_wsl("Ubuntu", "ops")
        .with_claims_ledger(&ledger)
        .with_docker_probe(
            Some(
                ["ninfer-kept", "ninfer-stranded", "foreign-x"]
                    .map(String::from)
                    .to_vec(),
            ),
            &calls,
        );
    let declared: Vec<String> = ["ninfer-kept".to_string(), "ninfer-stranded".to_string()].to_vec();
    let stranded = runtime
        .unclaimed_container_names(&declared)
        .await
        .expect("the probe ran");
    assert_eq!(
        stranded,
        ["ninfer-stranded".to_string()].into_iter().collect(),
        "claimed match silent, foreign name out of report"
    );
    let recorded = calls.lock().expect("probe calls lock").clone();
    assert_eq!(
        recorded,
        vec!["ps".to_string()],
        "the reconcile never reaches beyond `docker ps`"
    );
    assert!(
        recorded
            .iter()
            .all(|verb| !verb.contains("rm") && !verb.contains("kill") && !verb.contains("stop")),
        "no destructive verb may ever ride the reconcile: {recorded:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_reconcile_probe_is_silent_when_docker_cannot_answer() {
    // A failed probe is not a warning: `None` (not an empty set) is the
    // actor-side signal to stay silent AND to keep the last-reported set.
    let dir = crate::testutil::temp_dir("r5-probe-err");
    let ledger = dir.join("ninfer-claims.json");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runtime = ProcessRuntime::new()
        .with_wsl("Ubuntu", "ops")
        .with_claims_ledger(&ledger)
        .with_docker_probe(None, &calls);
    assert_eq!(
        runtime
            .unclaimed_container_names(&["ninfer-a".to_string()])
            .await,
        None,
        "a docker that cannot answer yields no report, not an empty one"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_reconcile_stands_down_silently_without_wsl_coordinates() {
    // Cardless-safe: no coordinates means no `wsl.exe`, no warn, `None`.
    // (Contrast the boot reclaim, which may name the orphan it cannot
    // reach — the R5 reconcile is the silent path by ruling.)
    let runtime = ProcessRuntime::new();
    assert_eq!(
        runtime
            .unclaimed_container_names(&["ninfer-a".to_string()])
            .await,
        None
    );
}

#[tokio::test]
async fn an_empty_ledger_with_no_running_containers_reports_nothing() {
    // Missing claim file (empty ledger) + no containers: the probe SUCCEEDS
    // with an empty set — nothing is stranded, and the actor stays quiet.
    let dir = crate::testutil::temp_dir("r5-empty");
    let ledger = dir.join("ninfer-claims.json");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runtime = ProcessRuntime::new()
        .with_wsl("Ubuntu", "ops")
        .with_claims_ledger(&ledger)
        .with_docker_probe(Some(Vec::new()), &calls);
    assert_eq!(
        runtime
            .unclaimed_container_names(&["ninfer-a".to_string()])
            .await,
        Some(BTreeSet::new())
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn container_ticket(port: u16, name: &str) -> SpawnTicket {
    SpawnTicket {
        generation: 1,
        kind: ResourceKind::Container {
            name: name.to_string(),
        },
        pid: 0,
        port,
    }
}

/// A spec whose `argv[0]` is `program`; the container identity lives on the
/// ticket, which is the only place the runtime reads it from.
fn client_spec(program: &str, args: &[&str], log_name: &str) -> LaunchSpec {
    LaunchSpec {
        argv: std::iter::once(program.to_string())
            .chain(args.iter().map(|arg| arg.to_string()))
            .collect(),
        cwd: std::env::temp_dir(),
        env: BTreeMap::new(),
        log_name: log_name.to_string(),
        readiness: ReadinessSpec::Http {
            path: "/health".to_string(),
            expect_status: 200,
        },
        container: None,
        script: None,
        rendered_profile: None,
        custody: None,
    }
}

fn child_ticket(port: u16) -> SpawnTicket {
    SpawnTicket {
        generation: 1,
        kind: ResourceKind::ChildProcess,
        pid: 0,
        port,
    }
}

/// [`client_spec`] marked for custody recording — exactly what the strata
/// adapter puts on every launch (slice 10b): the engine id says record,
/// the instance name is the model, `log_name` rides through.
fn custodied_spec(program: &str, args: &[&str], log_name: &str) -> LaunchSpec {
    LaunchSpec {
        custody: Some("strata".to_string()),
        ..client_spec(program, args, log_name)
    }
}

/// FINDINGS2-0910 [3] (the reviewer's F4): a claim minted at spawn *intent* is
/// ownership evidence for a client that never existed, and the boot reclaim
/// reads exactly that evidence as permission to `docker rm -f`. Blank WSL
/// coordinates keep every reap path from invoking `wsl.exe` (the reapers refuse
/// and warn), and the ledger is a temp file so the real one beside the test exe
/// stays untouched. Falsifier: mint the claim before the pre-reap again and this
/// pin fails on both the in-memory and the on-disk read.
#[tokio::test]
async fn a_failed_container_spawn_leaves_no_claim() {
    let dir = crate::testutil::temp_dir("claims-failed-spawn");
    let ledger = dir.join("ninfer-claims.json");
    let runtime = ProcessRuntime::new().with_claims_ledger(&ledger);
    let error = runtime
        .spawn(
            &client_spec("no-such-engine-program-se.exe", &[], "se-probe-1"),
            container_ticket(18_281, "se-probe-1"),
        )
        .await
        .expect_err("a program that does not exist cannot spawn");
    assert!(
        matches!(&error, SpawnError::SpawnFailed { reason }
                if reason.contains("no-such-engine-program-se.exe")),
        "{error}"
    );
    assert!(
        !runtime.claimed("se-probe-1"),
        "a spawn that never happened must leave no ownership evidence"
    );
    assert!(
        !ContainerClaims::load(&ledger).contains("se-probe-1"),
        "and nothing of it may reach the persisted ledger"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The belt on that suspenders, in its R2-08-04 shape: the client started
/// (suspended) — then the spawn is nevertheless reported failed (here: the job
/// object could not be created) before the seal, so the child never ran and
/// never reached the claim mint. A failed post-spawn report now leaves no
/// claim at all — nothing minted then retracted, nothing written — because the
/// ownership evidence moves only on the sealed success path.
///
/// Falsifier (pre-fix shape): the child was created RUNNING, so its first act
/// — the redirect file — appeared even though the spawn was reported failed.
/// The absence poll below goes red on that code; here it proves the seal
/// froze the child before any first act and the kill reaped it frozen.
#[tokio::test]
async fn a_post_spawn_failure_leaves_no_claim() {
    let dir = crate::testutil::temp_dir("claims-retract");
    let ledger = dir.join("ninfer-claims.json");
    let first_act = dir.join("first-act.txt");
    let runtime = ProcessRuntime::new()
        .with_claims_ledger(&ledger)
        .with_injected_job_failure();
    let error = runtime
        .spawn(
            &client_spec(
                "cmd.exe",
                &["/c", &format!("echo done > {}", first_act.display())],
                "se-probe-2",
            ),
            container_ticket(18_282, "se-probe-2"),
        )
        .await
        .expect_err("the injected job failure must fail the spawn");
    assert!(
        matches!(&error, SpawnError::SpawnFailed { reason }
                if reason.contains("injected job-object failure")),
        "{error}"
    );
    // A frozen child never acts: half a second is ample for a resumed
    // `cmd.exe` to run its redirect, and the killed-suspended child can
    // never produce the file at all.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !first_act.exists(),
        "the reported-failed child must never have run its first act"
    );
    assert!(
        !runtime.claimed("se-probe-2"),
        "the mint happens only after the seal, so the ledger never saw the name"
    );
    assert!(
        !ContainerClaims::load(&ledger).contains("se-probe-2"),
        "including on disk, where the boot reclaim reads it"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The control that keeps the two pins above from being satisfied by a claim
/// path that never works at all: a client that really started mints the claim
/// every reap depends on, in memory and on disk.
#[tokio::test]
async fn a_started_client_mints_the_claim_the_reaps_depend_on() {
    let dir = crate::testutil::temp_dir("claims-mint");
    let ledger = dir.join("ninfer-claims.json");
    let runtime = ProcessRuntime::new().with_claims_ledger(&ledger);
    let spawned = runtime
        .spawn(
            &client_spec("cmd.exe", &["/c", "exit 0"], "se-probe-3"),
            container_ticket(18_283, "se-probe-3"),
        )
        .await
        .expect("cmd.exe starts as a stand-in client");
    assert_eq!(spawned.port, 18_283);
    assert!(
        runtime.claimed("se-probe-3"),
        "a started client is exactly what the ledger records"
    );
    assert!(
        ContainerClaims::load(&ledger).contains("se-probe-3"),
        "and persists for the next boot to read"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// R2-08-04, the seal pinned through the production `spawn`: the client
/// starts suspended (so no first act can precede the job assignment), the
/// seal assigns it to the kill job and resumes it — and the resume really
/// happens: the first-act file appears, and the tracked stand-in exits and
/// releases inside the kill/poll budget. `cmd.exe` stands in for the
/// engine; its environment is the OS baseline, and the redirect target is
/// absolute, so no other variable is needed. Falsifier: keep the
/// `CREATE_SUSPENDED` flag and drop the resume arm of the seal, and the
/// child is frozen forever — the first-act poll times out red.
#[tokio::test]
async fn a_spawned_engine_resumes_and_runs() {
    let dir = crate::testutil::temp_dir("spawn-resume");
    let first_act = dir.join("first-act.txt");
    let runtime = ProcessRuntime::new();
    let ticket = child_ticket(18_301);
    let spawned = runtime
        .spawn(
            &client_spec(
                "cmd.exe",
                &["/c", &format!("echo done > {}", first_act.display())],
                "se-resume",
            ),
            ticket.clone(),
        )
        .await
        .expect("cmd.exe starts as a suspended-then-resumed engine child");
    assert_ne!(spawned.pid, 0, "a sealed child reports its pid");
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if first_act.exists() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the spawned child never ran its first act: the resume leg is dead"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    runtime
        .kill(&ticket)
        .await
        .expect("kill of the tracked stand-in");
    let mut released = false;
    for _ in 0..50 {
        if runtime
            .release_confirmed(&ticket)
            .await
            .expect("release evidence check")
        {
            released = true;
            break;
        }
    }
    assert!(
        released,
        "the resumed child exits and its release reads back as evidence"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Slice 10b, the live-side pins: a custodied launch records its identity
/// the moment the client really started (and the sidecar file sees it),
/// and a confirmed release clears the entry. `cmd.exe /c ping` stands in
/// for the strata router so the child is still live at kill time.
#[tokio::test]
async fn a_custodied_launch_records_then_a_release_clears_the_sidecar() {
    let dir = crate::testutil::temp_dir("custody-spawn");
    let sidecar = dir.join("strata-custody.json");
    let runtime = ProcessRuntime::new().with_custody_ledger(&sidecar);
    let ticket = child_ticket(18_291);
    let spawned = runtime
        .spawn(
            &custodied_spec(
                "cmd.exe",
                &["/c", "ping", "-n", "30", "127.0.0.1"],
                "se-custody",
            ),
            ticket.clone(),
        )
        .await
        .expect("cmd.exe starts as a strata-router stand-in");
    let recorded = runtime.custody_entries();
    assert_eq!(recorded.len(), 1, "one launch, one entry");
    assert_eq!(
        recorded[0],
        crate::custody::CustodyEntry {
            pid: spawned.pid,
            start_token: recorded[0].start_token,
            port: 18_291,
            instance: "se-custody".to_string(),
            engine: "strata".to_string(),
            claimed_at_ms: recorded[0].claimed_at_ms,
        },
        "pid, port, instance and engine id all ride the entry, with a real start token"
    );
    assert!(recorded[0].start_token > 0, "the token must be a real time");
    let on_disk = crate::custody::CustodyLedger::load(&sidecar);
    assert_eq!(
        on_disk.entries().cloned().collect::<Vec<_>>(),
        recorded,
        "the boot sweep reads exactly what the launch path wrote"
    );
    runtime
        .kill(&ticket)
        .await
        .expect("kill of a tracked engine");
    let mut confirmed = false;
    for _ in 0..50 {
        if runtime
            .release_confirmed(&ticket)
            .await
            .expect("release evidence check")
        {
            confirmed = true;
            break;
        }
    }
    assert!(
        confirmed,
        "the stand-in must release within the poll budget"
    );
    assert!(
        runtime.custody_entries().is_empty(),
        "a confirmed release ends the custody claim"
    );
    assert!(
        crate::custody::CustodyLedger::load(&sidecar).is_empty(),
        "including on disk: the next boot must not see a released engine"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Slice 10b's failure-side twin of the claim pins: a spawn that never
/// started a client records no custody evidence (the entry is written
/// only after every spawn-failure return), and a client that started but
/// whose spawn is nevertheless reported failed (injected job failure)
/// never reaches the write at all — since R2-08-04 the job seal precedes
/// every custody write — so a launch the harness calls failed never
/// leaves a record the boot sweep could act on.
#[tokio::test]
async fn a_failed_custodied_spawn_leaves_no_sidecar_entry() {
    let dir = crate::testutil::temp_dir("custody-failed");
    let sidecar = dir.join("strata-custody.json");
    let runtime = ProcessRuntime::new().with_custody_ledger(&sidecar);
    let error = runtime
        .spawn(
            &custodied_spec("no-such-engine-program-se.exe", &[], "se-custody-1"),
            child_ticket(18_292),
        )
        .await
        .expect_err("a program that does not exist cannot spawn");
    assert!(
        matches!(&error, SpawnError::SpawnFailed { reason }
                if reason.contains("no-such-engine-program-se.exe")),
        "{error}"
    );
    assert!(runtime.custody_entries().is_empty());
    assert!(crate::custody::CustodyLedger::load(&sidecar).is_empty());

    let runtime = ProcessRuntime::new()
        .with_custody_ledger(&sidecar)
        .with_injected_job_failure();
    let error = runtime
        .spawn(
            &custodied_spec("cmd.exe", &["/c", "exit 0"], "se-custody-2"),
            child_ticket(18_293),
        )
        .await
        .expect_err("the injected job failure must fail the spawn");
    assert!(
        matches!(&error, SpawnError::SpawnFailed { reason }
                if reason.contains("injected job-object failure")),
        "{error}"
    );
    assert!(
        runtime.custody_entries().is_empty(),
        "the seal failure precedes every custody write: nothing was recorded"
    );
    assert!(
        crate::custody::CustodyLedger::load(&sidecar).is_empty(),
        "including on disk, where the boot sweep reads it"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// R2-08-01 (HIGH): two live jobs can park two pollers on the SAME victim
/// ticket (the eviction job's victim and a same-slot reload's own reaper).
/// The old code read the table verdict under the lock, dropped the lock for
/// the TCP await, then removed `engines[port]` unguarded: a poller that
/// woke inside that gap deleted the SUCCESSOR the Dispose->Spawn gate had
/// already inserted — closing its job object (the kernel then took its
/// tree, the mechanism the replace-path comment itself names), unclaiming
/// its container on the retired ticket's `is_container` capture together
/// with the REMOVED entry's name, and forgetting its custody record on a
/// token that matched because the entry WAS the successor's own.
///
/// Determinism: the [`ReleaseGate`] seam parks one poller in the exact
/// await gap; the successor is installed while it stands there, so the
/// interleaving is ordered by the gate, never by wall-clock luck. The seam
/// adds no production await (the field does not exist outside `cfg(test)`).
/// On the unguarded removal every ownership assertion below is red.
#[tokio::test]
async fn a_blocked_release_poller_may_not_remove_the_successor_that_took_its_port() {
    let dir = crate::testutil::temp_dir("release-race");
    let ledger = dir.join("ninfer-claims.json");
    let sidecar = dir.join("strata-custody.json");
    let gate = Arc::new(ReleaseGate::default());
    let runtime = ProcessRuntime::new()
        .with_claims_ledger(&ledger)
        .with_custody_ledger(&sidecar)
        .with_release_gate(Arc::clone(&gate));
    let port = 18_296;
    let old = container_ticket(port, "se-race-old");
    runtime
        .spawn(
            &client_spec("cmd.exe", &["/c", "exit 0"], "se-race-old"),
            old.clone(),
        )
        .await
        .expect("cmd.exe starts as a stand-in client");
    runtime
        .kill(&old)
        .await
        .expect("kill of the tracked engine");

    let poller = {
        let runtime = runtime.clone();
        let ticket = old.clone();
        tokio::spawn(async move { runtime.release_confirmed(&ticket).await })
    };
    gate.arrived.notified().await;

    // The Dispose->Spawn gate: the release a FIRST poller proved let the
    // successor into the same port while this parked poller still sat in
    // its await gap. The successor is container-backed under a DIFFERENT
    // name (the stolen-claim shape) and custodied (the stolen-record shape).
    let successor = SpawnTicket {
        generation: 2,
        ..container_ticket(port, "se-race-new")
    };
    runtime
        .spawn(
            &custodied_spec(
                "cmd.exe",
                &["/c", "ping", "-n", "30", "127.0.0.1"],
                "se-race-new",
            ),
            successor.clone(),
        )
        .await
        .expect("the successor client starts");

    gate.departed.notify_one();
    assert_eq!(
        poller.await.expect("the parked poller ran to completion"),
        Ok(true),
        "the old lifetime's release stays confirmed for the retired ticket: \
             its child was killed and the port refused"
    );

    // And nothing of the successor's ownership evidence was touched:
    // entry (job object), claim, and custody record all belong to the
    // newer generation now (DOCTRINE 5: a stale ticket is refused, never
    // honoured — extended to the removal, not just the verdict read).
    {
        let mut engines = runtime.engines.lock().expect("engine table lock");
        let entry = engines
            .get_mut(&port)
            .expect("the successor keeps its table entry");
        assert_eq!(
            entry.generation, 2,
            "the removal may never take the successor's slot"
        );
        assert!(
            !entry.terminated,
            "a retired ticket's poller may not speak for the successor's lifetime"
        );
        assert_eq!(entry.container.as_deref(), Some("se-race-new"));
        assert!(
                entry.child.try_wait().expect("child status").is_none(),
                "the successor's job object stays open: dropping the entry would have let KILL_ON_JOB_CLOSE take its tree"
            );
    }
    assert!(
        runtime.claimed("se-race-new"),
        "the successor's container claim is not the retired ticket's to end"
    );
    assert!(
        runtime.claimed("se-race-old"),
        "the retired ticket's own claim stays for the boot reclaim: no removal ran here either"
    );
    let entries = runtime.custody_entries();
    assert_eq!(entries.len(), 1, "the successor's custody record survives");
    assert_eq!(entries[0].port, port, "and it is the successor's entry");

    runtime.kill(&successor).await.expect("cleanup kill");
    let _ = std::fs::remove_dir_all(&dir);
}

/// R2-08-02 (MEDIUM): a detached container's client exit and a refusing
/// port are NOT release evidence — `kill`'s `docker rm -f` is. Here the
/// reap cannot run (blank WSL coordinates make [`reap_container`] refuse
/// and report false — the stub the pin asked for, with no `wsl.exe` and no
/// docker ever invoked), so the kill records a failed reap and the release
/// must stay unconfirmed indefinitely: the release timeout keeps the claim,
/// and the next boot reclaim — which keeps it until docker confirms — stays
/// the path with authority. Unclaiming on port refusal alone would strand a
/// possibly-live container that boot reclaim then refuses forever as
/// `SKIP_UNCLAIMED`, because this harness destroyed the evidence itself.
/// The control at the end (reap verdict recorded as confirmed) keeps the
/// pin from being satisfied by a release path that never unclaims at all.
/// (The seam pins `an_explicit_rekill_rearms_only_an_unconfirmed_container_reap`
/// and `a_kill_with_a_confirmed_reap_releases_the_container_slot` now drive
/// `kill` writing that verdict itself (REV7-03), so this direct write is a
/// belt, not the only coverage of the true side.)
#[tokio::test]
async fn an_unconfirmed_container_reap_holds_the_release_of_its_port() {
    let dir = crate::testutil::temp_dir("release-reap-held");
    let ledger = dir.join("ninfer-claims.json");
    let runtime = ProcessRuntime::new().with_claims_ledger(&ledger);
    let port = 18_297;
    let ticket = container_ticket(port, "se-unreaped");
    runtime
        .spawn(
            &client_spec(
                "cmd.exe",
                &["/c", "ping", "-n", "30", "127.0.0.1"],
                "se-unreaped",
            ),
            ticket.clone(),
        )
        .await
        .expect("cmd.exe starts as a stand-in client");
    runtime
        .kill(&ticket)
        .await
        .expect("kill of the tracked engine");
    for poll in 0..5 {
        assert!(
            !runtime
                .release_confirmed(&ticket)
                .await
                .expect("evidence check runs"),
            "poll {poll}: the client exited and the port refuses, but the reap failed — \
                 unclaiming here would strand a live container boot reclaim will not touch"
        );
    }
    assert!(
        runtime.claimed("se-unreaped"),
        "the claim must ride to the release timeout and the next boot reclaim"
    );
    assert_eq!(
        runtime.engine_count(),
        1,
        "the entry stays so the port still resolves to us"
    );

    // Control: the same evidence with the reap verdict recorded as
    // confirmed (the bit `kill` writes on a docker success — written here
    // directly, because a PASSING `docker rm -f` would mean invoking the
    // operator's real WSL, which a cardless test must never do) confirms
    // the release and ends the claim, exactly as before the fix.
    {
        let mut engines = runtime.engines.lock().expect("engine table lock");
        engines
            .get_mut(&port)
            .expect("the entry stayed")
            .reap_confirmed = true;
    }
    assert!(
        runtime
            .release_confirmed(&ticket)
            .await
            .expect("evidence check runs"),
        "port refused + reap confirmed is the same evidence the boot path demands"
    );
    assert!(
        !runtime.claimed("se-unreaped"),
        "a confirmed reap ends the claim as it always did"
    );
    assert_eq!(
        runtime.engine_count(),
        0,
        "and the confirmed release cleans the table"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// What the reap seam answered so far (REV7 pins): one name per scripted,
/// `kill`-carried reap. A passive `release_confirmed` poll must never
/// extend this list, and a duty the rule retired must not consume a
/// scripted verdict either.
fn reap_asked(seam: &ReapSeam) -> Vec<String> {
    seam.asked.lock().expect("reap verdict seam lock").clone()
}

/// REV7-01 (MEDIUM) + REV7-03: a failed `docker rm -f` used to retire the
/// reap duty with `terminated`, so the port, the slot, and the claim could
/// stay burned until the next boot. Now an UNCONFIRMED reap re-arms on an
/// explicit re-kill only. The scripted seam drives `kill` through BOTH
/// verdicts (false at the first kill, true at the re-kill, a third verdict
/// left unconsumed on purpose), so the whole arc -- hold, passive polls
/// that never retry, re-arm, retirement on confirmation, release with the
/// claim ended -- runs against the production duty, claim, and generation
/// logic without ever invoking WSL.
#[tokio::test]
async fn an_explicit_rekill_rearms_only_an_unconfirmed_container_reap() {
    let dir = crate::testutil::temp_dir("release-rearm");
    let ledger = dir.join("ninfer-claims.json");
    let (runtime, seam) = ProcessRuntime::new()
        .with_claims_ledger(&ledger)
        .with_reap_seam(&[false, true, true]);
    let port = 18_298;
    let t = container_ticket(port, "se-rearm");
    runtime
        .spawn(
            &client_spec(
                "cmd.exe",
                &["/c", "ping", "-n", "30", "127.0.0.1"],
                "se-rearm",
            ),
            t.clone(),
        )
        .await
        .expect("cmd.exe starts as a stand-in client");
    runtime
        .kill(&t)
        .await
        .expect("the first kill carries the reap duty");
    assert_eq!(
        reap_asked(&seam),
        vec!["se-rearm".to_string()],
        "the first kill's duty ran once, and the seam reports it failed"
    );
    for poll in 0..3 {
        assert!(
            !runtime
                .release_confirmed(&t)
                .await
                .expect("evidence check runs"),
            "poll {poll}: the unconfirmed reap holds the release (R2-08-02)"
        );
    }
    assert_eq!(
        reap_asked(&seam),
        vec!["se-rearm".to_string()],
        "REV7-01: a passive release poll must never retry the reap"
    );
    assert!(
        runtime.claimed("se-rearm"),
        "the claim rides on through the hold"
    );

    runtime
        .kill(&t)
        .await
        .expect("an explicit re-kill re-arms the unconfirmed duty");
    assert_eq!(
        reap_asked(&seam),
        vec!["se-rearm".to_string(); 2],
        "the re-kill ran the reap again"
    );
    runtime
        .kill(&t)
        .await
        .expect("the third kill meets a CONFIRMED reap");
    assert_eq!(
            reap_asked(&seam),
            vec!["se-rearm".to_string(); 2],
            "a confirmed reap retires the duty forever (review, 2026-10-26): the third scripted verdict stayed unconsumed"
        );

    assert!(
        runtime
            .release_confirmed(&t)
            .await
            .expect("evidence check runs"),
        "kill recorded the re-kill's confirmed verdict: the same evidence now releases"
    );
    assert!(
        !runtime.claimed("se-rearm"),
        "the confirmed release ends the claim the failed reap had kept"
    );
    assert_eq!(runtime.engine_count(), 0, "and the slot is free again");
    let _ = std::fs::remove_dir_all(&dir);
}

/// REV7-03: the `reap_confirmed == true` side used to be reachable only
/// from a test hand-write. The seam makes `kill` itself record a
/// docker-confirmed reap (still without WSL), and the very next passive
/// poll releases, ends the claim, and clears the table -- the proof that
/// the true side rides on `kill`, not on a test poking the field.
#[tokio::test]
async fn a_kill_with_a_confirmed_reap_releases_the_container_slot() {
    let dir = crate::testutil::temp_dir("release-confirmed");
    let ledger = dir.join("ninfer-claims.json");
    let (runtime, seam) = ProcessRuntime::new()
        .with_claims_ledger(&ledger)
        .with_reap_seam(&[true]);
    let port = 18_299;
    let t = container_ticket(port, "se-confirmed");
    runtime
        .spawn(
            &client_spec(
                "cmd.exe",
                &["/c", "ping", "-n", "30", "127.0.0.1"],
                "se-confirmed",
            ),
            t.clone(),
        )
        .await
        .expect("cmd.exe starts as a stand-in client");
    runtime
        .kill(&t)
        .await
        .expect("the kill's scripted reap reports docker-confirmed");
    assert_eq!(
        reap_asked(&seam),
        vec!["se-confirmed".to_string()],
        "kill carried the duty for exactly this name"
    );
    assert!(
        runtime
            .release_confirmed(&t)
            .await
            .expect("evidence check runs"),
        "kill wrote the true verdict: the first poll must confirm"
    );
    assert!(
        !runtime.claimed("se-confirmed"),
        "a kill-confirmed reap ends the claim as before"
    );
    assert_eq!(runtime.engine_count(), 0, "and the release clears the slot");
    let _ = std::fs::remove_dir_all(&dir);
}

/// REV7-02 (LOW, decision: release-eligible): the SkipUnclaimed shape --
/// the duty fell to a kill but the ledger no longer holds the name -- has
/// NO ownership evidence left to preserve, and boot reclaim already
/// refuses the name, so holding the release forever would burn the slot
/// with nothing protected. The plain G12 pair decides instead. The
/// scripted verdict must stay unconsumed: an unclaimed entry never reaches
/// `docker rm`, and nothing is written to an unheld name. Recovery for the
/// rare self-inflicted missing claim is the R5 stranded report naming it
/// -- report-only by ruling.
#[tokio::test]
async fn an_unclaimed_container_slot_releases_on_plain_port_evidence() {
    let dir = crate::testutil::temp_dir("release-noclaim");
    let ledger = dir.join("ninfer-claims.json");
    let (runtime, seam) = ProcessRuntime::new()
        .with_claims_ledger(&ledger)
        .with_reap_seam(&[true]);
    let port = 18_300;
    let t = container_ticket(port, "se-noclaim");
    runtime
        .spawn(
            &client_spec(
                "cmd.exe",
                &["/c", "ping", "-n", "30", "127.0.0.1"],
                "se-noclaim",
            ),
            t.clone(),
        )
        .await
        .expect("cmd.exe starts as a stand-in client");
    // Present the SkipUnclaimed shape: the claim ledger lost the name
    // between spawn and kill -- the state a cleared or corrupted ledger
    // file loads as.
    runtime
        .claims
        .lock()
        .expect("container claim ledger lock")
        .unclaim("se-noclaim");
    runtime
        .kill(&t)
        .await
        .expect("the kill owes the duty but the ledger refuses the authority");
    assert!(
        reap_asked(&seam).is_empty(),
        "an unclaimed entry must never reach docker rm"
    );
    assert!(
        runtime
            .release_confirmed(&t)
            .await
            .expect("evidence check runs"),
        "with no claim there is no ownership evidence to preserve: the G12 pair releases"
    );
    assert_eq!(
        runtime.engine_count(),
        0,
        "the slot is not burned until boot (REV7-02)"
    );
    assert!(
        !ContainerClaims::load(&ledger).contains("se-noclaim"),
        "and the release wrote nothing for a name the ledger never held"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Slice 10b: the staging path reads its root from the compiled ninfer
/// row's `custody_exec_roots` cell — the cell 10a left readerless gains
/// its reader here, so the staging location is single-homed in the policy
/// table and cannot drift from the compiled custody root (§20-9 Fork C).
#[test]
fn the_staged_scripts_root_is_read_from_the_policy_cell() {
    let row = crate::engine::policy::engine_policy(crate::ownership::NINFER_ENGINE)
        .expect("the compiled-in table ships a ninfer row");
    assert_eq!(
        row.custody_exec_roots.first().copied(),
        Some(staged_scripts_root()),
        "the cell is the value's single home, not a re-aliased constant"
    );
    let path = staged_script_path("nin-x", "echo body");
    assert!(
        path.starts_with(&format!("~/{}/", staged_scripts_root())),
        "the staged script lives under the compiled custody root: {path}"
    );
}

/// [3]'s other half: the ticket carries the reap *duty*, the ledger carries the
/// *authority*, and only their conjunction reaps. The kill path used to reap
/// straight from the ticket's name.
#[test]
fn the_kill_reap_requires_both_the_duty_and_the_claim() {
    let duty = Some("ninfer-a".to_string());
    assert_eq!(
        reap_action(duty.clone(), true),
        ReapAction::Reap("ninfer-a".to_string()),
        "duty plus claim is the only combination that addresses a container"
    );
    assert_eq!(
        reap_action(duty.clone(), false),
        ReapAction::SkipUnclaimed("ninfer-a".to_string()),
        "a duty with no claim behind it leaves the container running"
    );
    assert_eq!(reap_action(None, true), ReapAction::None);
    assert_eq!(
        reap_action(None, false),
        ReapAction::None,
        "a re-kill never reaps, claim or no claim"
    );
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn spec_map(list: &[(&str, &str)]) -> BTreeMap<String, String> {
    list.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn upper_keys(env: &[(String, String)]) -> Vec<String> {
    env.iter().map(|(key, _)| key.to_uppercase()).collect()
}

/// A synthetic harness env carrying every baseline var (mixed spellings where
/// Windows plausibly carries them) plus the vars that must never reach an
/// engine child: the plaintext control secret, the harness log knob in both
/// spellings, and the Python hijack vars.
fn secret_bearing_source() -> Vec<(String, String)> {
    pairs(&[
        ("SystemRoot", "C:\\Windows"),
        ("windir", "C:\\Windows"),
        ("COMSPEC", "C:\\Windows\\System32\\cmd.exe"),
        ("Path", "C:\\Windows\\System32"),
        ("PATHEXT", ".COM;.EXE"),
        ("TEMP", "C:\\Temp"),
        ("TMP", "C:\\Temp"),
        ("SIMPLEHARN_TOKEN", "plaintext-secret"),
        ("SIMPLEHAR_TOKEN", "plaintext-secret"),
        ("SIMPLEHARN_LOG", "debug"),
        ("SIMPLEHAR_LOG", "debug"),
        ("PYTHONPATH", "C:\\evil\\hijack"),
        ("VIRTUAL_ENV", "C:\\evil\\venv"),
        ("CARGO_MANIFEST_DIR", "C:\\src"),
    ])
}

fn count_child_var(dump: &str, key: &str) -> usize {
    let prefix = format!("{}=", key.to_uppercase());
    dump.lines()
        .filter(|line| line.trim_start().to_uppercase().starts_with(&prefix))
        .count()
}

fn has_child_var(dump: &str, key: &str) -> bool {
    count_child_var(dump, key) > 0
}

#[test]
fn baseline_keeps_only_the_os_names_and_drops_every_secret() {
    let env = child_env_baseline_from(secret_bearing_source());
    let keys = upper_keys(&env);
    assert_eq!(
        keys.len(),
        OS_ENV_BASELINE.len(),
        "one entry per baseline name, no duplicates: {keys:?}"
    );
    let actual: BTreeSet<String> = keys.iter().cloned().collect();
    let expected: BTreeSet<String> = OS_ENV_BASELINE
        .iter()
        .map(|name| name.to_uppercase())
        .collect();
    assert_eq!(actual, expected, "baseline carries exactly the OS names");
    for secret in [
        "SIMPLEHARN_TOKEN",
        "SIMPLEHAR_TOKEN",
        "PYTHONPATH",
        "VIRTUAL_ENV",
        "CARGO_MANIFEST_DIR",
    ] {
        assert!(
            !keys.contains(&secret.to_string()),
            "secret {secret} leaked into the baseline"
        );
    }
}

#[test]
fn baseline_falls_back_for_path_temp_tmp_and_omits_the_rest() {
    let env = child_env_baseline_from(pairs(&[("COMSPEC", "C:\\Windows\\System32\\cmd.exe")]));
    let exe_dir = std::env::current_exe()
        .expect("exe path")
        .parent()
        .expect("exe directory")
        .to_string_lossy()
        .into_owned();
    let temp = std::env::temp_dir().to_string_lossy().into_owned();
    let got: BTreeMap<String, String> = env
        .into_iter()
        .map(|(k, v)| (k.to_uppercase(), v))
        .collect();
    assert_eq!(got.get("PATH").expect("Path always present"), &exe_dir);
    assert_eq!(got.get("TEMP").expect("TEMP always present"), &temp);
    assert_eq!(got.get("TMP").expect("TMP always present"), &temp);
    assert_eq!(
        got.get("COMSPEC").expect("COMSPEC copied"),
        "C:\\Windows\\System32\\cmd.exe"
    );
    for absent in ["SYSTEMROOT", "WINDIR", "PATHEXT"] {
        assert!(!got.contains_key(absent), "{absent} must never be invented");
    }
}

#[test]
fn baseline_collapses_case_duplicate_names_to_one_entry() {
    let env = child_env_baseline_from(pairs(&[
        ("PATH", "C:\\first"),
        ("Path", "C:\\second"),
        ("temp", "C:\\t1"),
        ("TEMP", "C:\\t2"),
    ]));
    let keys = upper_keys(&env);
    assert_eq!(keys.len(), env.len(), "no case-duplicates: {keys:?}");
    let path = env
        .iter()
        .find(|(k, _)| k.to_uppercase() == "PATH")
        .expect("Path present");
    assert_eq!(path.0, "Path", "baseline emits its canonical spelling");
    assert_eq!(path.1, "C:\\first", "first occurrence wins");
}

#[test]
fn spec_env_overrides_the_baseline_case_insensitively() {
    let assembled = engine_child_env(
        secret_bearing_source(),
        &spec_map(&[("PATH", "C:\\engine\\bin"), ("EXTRA_ONE", "1")]),
    );
    let keys = upper_keys(&assembled);
    assert_eq!(keys.len(), assembled.len(), "no case-duplicates: {keys:?}");
    let (name, value) = assembled
        .iter()
        .find(|(k, _)| k.to_uppercase() == "PATH")
        .expect("exactly one Path entry");
    assert_eq!(name, "PATH", "the spec entry keeps its own spelling");
    assert_eq!(
        value, "C:\\engine\\bin",
        "the spec entry replaces the baseline's"
    );
    assert!(
        assembled.contains(&("EXTRA_ONE".to_string(), "1".to_string())),
        "a fresh spec entry is appended"
    );
}

#[test]
fn engine_child_env_keeps_baseline_and_spec_and_drops_everything_else() {
    let assembled = engine_child_env(
        secret_bearing_source(),
        &spec_map(&[("PROBE_MARKER", "seen")]),
    );
    let keys = upper_keys(&assembled);
    for name in OS_ENV_BASELINE {
        assert!(
            keys.contains(&name.to_uppercase()),
            "baseline {name} missing: {keys:?}"
        );
    }
    for forbidden in [
        "SIMPLEHARN_TOKEN",
        "SIMPLEHAR_TOKEN",
        "SIMPLEHARN_LOG",
        "SIMPLEHAR_LOG",
        "PYTHONPATH",
        "VIRTUAL_ENV",
        "CARGO_MANIFEST_DIR",
    ] {
        assert!(
            !keys.contains(&forbidden.to_string()),
            "harness-only name {forbidden} must never reach a child"
        );
    }
    assert!(
        assembled.contains(&("PROBE_MARKER".to_string(), "seen".to_string())),
        "spec entry must be present"
    );
    assert_eq!(keys.len(), assembled.len(), "no case-duplicates: {keys:?}");
}

#[test]
fn assembled_env_reaches_a_real_child_without_inherited_secrets() {
    use std::os::windows::process::CommandExt as _;
    let mut source: Vec<(String, String)> = std::env::vars().collect();
    source.extend(pairs(&[
        ("SIMPLEHARN_TOKEN", "plaintext-secret"),
        ("SIMPLEHAR_TOKEN", "plaintext-secret"),
        ("SIMPLEHARN_LOG", "debug"),
        ("SIMPLEHAR_LOG", "debug"),
        ("PYTHONPATH", "C:\\evil\\hijack"),
    ]));
    let assembled = engine_child_env(source, &spec_map(&[("PROBE_MARKER", "seen")]));
    let dir = crate::testutil::temp_dir("childenv");
    let dump = dir.join("child-set.txt");
    let mut command = std::process::Command::new("cmd.exe");
    command
        .args(["/c", &format!("set > {}", dump.display())])
        .current_dir(&dir)
        .env_clear()
        .creation_flags(CREATE_NO_WINDOW);
    for (key, value) in &assembled {
        command.env(key, value);
    }
    let status = command.status().expect("cmd.exe runs");
    assert!(status.success(), "cmd /c set exited with {status}");
    let text = std::fs::read_to_string(&dump).expect("child wrote its env dump");
    assert!(
        has_child_var(&text, "PROBE_MARKER"),
        "spec entry missing from the child: {text}"
    );
    assert!(
        !has_child_var(&text, "SIMPLEHARN_TOKEN"),
        "secret reached the child: {text}"
    );
    assert!(
        !has_child_var(&text, "SIMPLEHAR_TOKEN"),
        "the deprecated token name is a secret too: it may not reach the child: {text}"
    );
    assert!(
        !has_child_var(&text, "SIMPLEHARN_LOG"),
        "harness log knob reached the child: {text}"
    );
    assert!(
        !has_child_var(&text, "SIMPLEHAR_LOG"),
        "the deprecated log name is harness-only too: it may not reach the child: {text}"
    );
    assert!(
        !has_child_var(&text, "PYTHONPATH"),
        "python hijack var reached the child: {text}"
    );
    assert_eq!(
        count_child_var(&text, "Path"),
        1,
        "exactly one Path line in the child env: {text}"
    );
    if std::env::var_os("SystemRoot").is_some() || std::env::var_os("windir").is_some() {
        assert!(
            has_child_var(&text, "SystemRoot") || has_child_var(&text, "windir"),
            "OS locators must survive into the child: {text}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Injects harness-process vars for one test and removes them on scope exit,
/// including through an assertion panic. Only names no other test in this
/// binary reads may be injected: the crate reads just `SIMPLEHARN_LOG`,
/// `SIMPLEHARN_TOKEN` and `SIMPLEHARN_CONFIG` — plus the deprecated
/// `SIMPLEHAR_LOG`/`SIMPLEHAR_TOKEN`/`SIMPLEHAR_CONFIG` fallbacks (the log
/// knob is read only inside `observability::init`, which tests never call) —
/// never `PYTHONPATH`/`VIRTUAL_ENV`, and integration suites are separate
/// processes.
/// `std::env::set_var` is a safe call in edition 2021 — an `unsafe` block here
/// would deny the build with `unused_unsafe` under `-D warnings`.
struct HarnessEnvGuard(Vec<String>);

impl HarnessEnvGuard {
    fn set(list: &[(&str, &str)]) -> Self {
        let mut keys = Vec::with_capacity(list.len());
        for (key, value) in list {
            std::env::set_var(key, value);
            keys.push((*key).to_string());
        }
        Self(keys)
    }
}

impl Drop for HarnessEnvGuard {
    fn drop(&mut self) {
        for key in &self.0 {
            std::env::remove_var(key);
        }
    }
}

/// Polls until the child's redirected `set` output exists and stops changing
/// (the file is created at redirect time, written when cmd flushes).
async fn wait_for_child_env_dump(path: &std::path::Path) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut previous = String::new();
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if !text.is_empty() && text == previous {
            return text;
        }
        previous = text;
        assert!(
            std::time::Instant::now() < deadline,
            "the child `set` dump never stabilized at {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn engine_spawn_serves_only_the_declared_environment() {
    // The production path end to end: `ProcessRuntime::spawn` (the single
    // LaunchSpec-driven spawn site) must hand the child baseline + spec only.
    // Neutralising its `env_clear()` fails the PYTHONPATH/VIRTUAL_ENV/
    // SIMPLEHAR_LOG/CARGO_ assertions below, because these markers genuinely
    // live in the harness process while the child spawns.
    let _markers = HarnessEnvGuard::set(&[
        ("PYTHONPATH", "C:\\evil\\hijack"),
        ("VIRTUAL_ENV", "C:\\evil\\venv"),
        ("SIMPLEHARN_LOG", "debug"),
        ("SIMPLEHAR_LOG", "debug"),
    ]);
    let dir = crate::testutil::temp_dir("enginespawn");
    let dump = dir.join("child-set.txt");
    let runtime = ProcessRuntime::new();
    let spec = LaunchSpec {
        argv: vec![
            "cmd.exe".to_string(),
            "/c".to_string(),
            format!("set > {}", dump.display()),
        ],
        cwd: dir.clone(),
        env: spec_map(&[("PROBE_MARKER", "seen")]),
        log_name: "engine-env-test".to_string(),
        readiness: ReadinessSpec::Http {
            path: "/health".to_string(),
            expect_status: 200,
        },
        container: None,
        script: None,
        rendered_profile: None,
        custody: None,
    };
    let tk = SpawnTicket {
        generation: 7,
        kind: ResourceKind::ChildProcess,
        pid: 0,
        port: 18_195,
    };
    let spawned = runtime
        .spawn(&spec, tk.clone())
        .await
        .expect("cmd.exe spawns as an engine child");
    assert_eq!(spawned.port, 18_195);
    let text = wait_for_child_env_dump(&dump).await;
    assert!(
        has_child_var(&text, "PROBE_MARKER"),
        "spec entry missing from the engine child: {text}"
    );
    assert!(
        has_child_var(&text, "Path"),
        "baseline Path missing from the engine child: {text}"
    );
    for forbidden in [
        "SIMPLEHARN_TOKEN",
        "SIMPLEHAR_TOKEN",
        "SIMPLEHARN_LOG",
        "SIMPLEHAR_LOG",
        "PYTHONPATH",
        "VIRTUAL_ENV",
    ] {
        assert!(
            !has_child_var(&text, forbidden),
            "engine child inherited harness {forbidden}: {text}"
        );
    }
    let inherited_toolchain: Vec<&str> = text
        .lines()
        .filter(|line| line.trim_start().to_uppercase().starts_with("CARGO_"))
        .collect();
    assert!(
        inherited_toolchain.is_empty(),
        "engine child inherited harness toolchain vars: {inherited_toolchain:?}"
    );
    if std::env::var_os("SystemRoot").is_some() || std::env::var_os("windir").is_some() {
        assert!(
            has_child_var(&text, "SystemRoot") || has_child_var(&text, "windir"),
            "OS locators must survive into the engine child: {text}"
        );
    }
    runtime.kill(&tk).await.expect("cleanup kill runs");
    let _ = std::fs::remove_dir_all(&dir);
}

/// FINDINGS2-0910 [4], the assembly rule: a control command gets exactly the OS
/// baseline. The test process genuinely carries `CARGO_*` (and a real harness run
/// carries `SIMPLEHARN_TOKEN`), so "exactly the baseline names" is a filter that
/// has to be working for this to hold.
#[test]
fn control_child_env_is_the_baseline_and_nothing_else() {
    let env = control_child_env();
    let keys = upper_keys(&env);
    let expected: BTreeSet<String> = OS_ENV_BASELINE
        .iter()
        .map(|name| name.to_uppercase())
        .collect();
    let actual: BTreeSet<String> = keys.iter().cloned().collect();
    assert_eq!(
        actual, expected,
        "one entry per baseline name, nothing else"
    );
    assert_eq!(keys.len(), env.len(), "no case-duplicates: {keys:?}");
    for name in &keys {
        assert!(
            !name.starts_with("SIMPLEHAR") && !name.starts_with("CARGO_"),
            "secret/toolchain name {name} reached the control assembly"
        );
    }
}

/// FINDINGS2-0910 [4], the production site: `run_control_command` is the ONE
/// builder both WSL control commands go through, so this drives it directly with
/// a real child — exactly how the engine-child pin drives `ProcessRuntime::spawn`.
/// The load-bearing marker is `CARGO_*`: cargo always sets those in the test
/// process, they are not on the baseline, and reading them from the harness env
/// is something no other test does (unlike `SIMPLEHARN_TOKEN`, which the api layer
/// reads). Falsifier: drop the `env_clear()` here and the toolchain assertions
/// below fail.
#[tokio::test]
async fn a_control_command_child_carries_no_harness_environment() {
    let dir = crate::testutil::temp_dir("controlenv");
    let dump = dir.join("child-set.txt");
    let args = vec!["/c".to_string(), format!("set > {}", dump.display())];
    match run_control_command("cmd.exe", &args).await {
        ControlOutcome::Ran(output) => {
            assert!(output.status.success(), "cmd /c set exited with {output:?}")
        }
        other => panic!("the control command could not run: {other:?}"),
    }
    let text = wait_for_child_env_dump(&dump).await;
    for forbidden in [
        "SIMPLEHARN_TOKEN",
        "SIMPLEHAR_TOKEN",
        "SIMPLEHARN_LOG",
        "SIMPLEHAR_LOG",
        "PYTHONPATH",
        "VIRTUAL_ENV",
    ] {
        assert!(
            !has_child_var(&text, forbidden),
            "control child inherited harness {forbidden}: {text}"
        );
    }
    let inherited_toolchain: Vec<String> = text
        .lines()
        .filter(|line| line.trim_start().to_uppercase().starts_with("CARGO_"))
        .map(|line| line.trim().to_string())
        .collect();
    assert!(
        inherited_toolchain.is_empty(),
        "control child inherited the harness toolchain environment: \
             {inherited_toolchain:?}"
    );
    assert_eq!(
        count_child_var(&text, "Path"),
        1,
        "exactly one Path line for `wsl.exe` to resolve itself: {text}"
    );
    if std::env::var_os("SystemRoot").is_some() || std::env::var_os("windir").is_some() {
        assert!(
            has_child_var(&text, "SystemRoot") || has_child_var(&text, "windir"),
            "OS locators must survive into the control child: {text}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// R11: the fit probe rides the ONE control-command family. A tool that is
/// not there answers `None` to the caller (the wrapper itself warned — a
/// spawn failure, never a fabricated projection), and a real child's stdout
/// travels verbatim toward the strict parser. `cmd.exe` is a harmless
/// stand-in: the fit tool itself is never exec'd by tests.
#[tokio::test]
async fn the_fit_probe_answers_stdout_when_the_tool_answers_and_none_when_it_cannot() {
    let runtime = ProcessRuntime::new();
    let missing = std::path::Path::new("C:\\simpleharn-absent-volume\\llama-fit-params.exe");
    assert_eq!(
        runtime
            .run_probe_command(missing, &["--version".to_string()], Duration::from_secs(2))
            .await,
        None,
        "a tool that cannot start is silence to the caller, not an empty answer"
    );
    let answer = runtime
        .run_probe_command(
            std::path::Path::new("cmd.exe"),
            &["/c".to_string(), "echo CUDA0 1 2 3".to_string()],
            Duration::from_secs(20),
        )
        .await
        .expect("cmd.exe answers on every windows host");
    assert!(
        answer.contains("CUDA0 1 2 3"),
        "stdout travels to the parser: {answer:?}"
    );
}

/// Fake status fixtures on an ephemeral port (the SE fake-server rule, same
/// posture as `serve_health` in `engine/mod.rs`): one route answers JSON,
/// one answers plain text. No engine, no protected port.
async fn serve_status_fixtures() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral");
    let port = listener.local_addr().expect("addr").port();
    let app = axum::Router::new()
        .route(
            "/v1/status",
            axum::routing::get(|| async { r#"{"engine":{"info":{"vram":67.4}}}"# }),
        )
        .route("/text", axum::routing::get(|| async { "not json at all" }));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    port
}

/// TASK-ENGINE-MEASUREMENT phase 1: `engine_status_json` is a generic
/// loopback read — an answered JSON body reaches the per-engine parser,
/// and every other outcome (body that is not JSON, nothing listening)
/// answers `None`, so the sampler falls back to the witness instead of
/// guessing (ORCHESTRATION §1, R9 `container_report` posture).
#[tokio::test]
async fn engine_status_json_answers_the_body_or_silence_never_a_guess() {
    let runtime = ProcessRuntime::new();
    let port = serve_status_fixtures().await;
    let body = runtime
        .engine_status_json(port, "/v1/status")
        .await
        .expect("the answered snapshot parses");
    assert_eq!(body["engine"]["info"]["vram"].as_f64(), Some(67.4));
    assert!(
        runtime.engine_status_json(port, "/text").await.is_none(),
        "a body that is not JSON is silence, not a snapshot"
    );
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral port");
        listener.local_addr().expect("addr").port()
    };
    assert!(
        runtime
            .engine_status_json(closed, "/v1/status")
            .await
            .is_none(),
        "an engine that never answers is silence, not an empty snapshot"
    );
}

/// `descendant_pids` lists every descendant of the asked pid at every
/// depth — a direct child is in the set (capture 2026-11-04: both
/// engines hang two levels below the harness child, so the full tree is
/// the contract). The pin spawns one plain hidden `cmd.exe` child — no
/// engine execs here, the same stand-in class as the control-command pin
/// — and asks the live table where it hangs; an absent pid answers empty,
/// never a guess.
#[tokio::test]
async fn descendant_pids_includes_the_direct_children_of_the_asked_pid() {
    use std::os::windows::process::CommandExt;
    let mut child = std::process::Command::new("cmd.exe")
        .args(["/c", "ping -n 30 127.0.0.1 >nul"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("cmd.exe stands in for a child engine");
    let children = descendant_pids_of(std::process::id());
    let found = children.contains(&child.id());
    // The child is ours (spawned above), so reaping it is ours to do.
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        found,
        "the spawned child must appear under its own parent: {children:?}"
    );
    assert!(
        descendant_pids_of(u32::MAX).is_empty(),
        "a pid with no descendants has none, never a guess"
    );
}

/// Phase-5 pin: the walk reaches the GRANDCHILD level. An outer
/// `cmd.exe` synchronously runs an inner `cmd.exe`, which itself spawns
/// `ping.exe` — the same python -> python -> engine depth as the
/// captured Strata and Basalt serves — and every stand-in stays alive
/// while the walk runs. The second generation needs a moment to exist,
/// so the pin POLLS the bounded walk with a deadline until the live tree
/// proves some listed pid is itself the parent of another listed pid,
/// which the one-generation contract could never show; no single early
/// snapshot decides the pin. All stand-ins are spawned by this test, so
/// the whole chain is its own to reap: `taskkill` over the tree it
/// created, then the handle it owns. Falsifier: restore the
/// direct-children-only walk and the pair never appears — the set is
/// just the inner cmd.exe, whose own child is missing from it — so the
/// deadline expires and the depth assertion fails red.
#[tokio::test]
async fn descendant_pids_reaches_the_grandchildren_of_the_asked_pid() {
    use std::os::windows::process::CommandExt;
    let mut outer = std::process::Command::new("cmd.exe")
        .args(["/c", "cmd /c ping -n 30 127.0.0.1 >nul"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("outer cmd.exe stands in for the harness child");
    // The inner cmd.exe and its ping take a moment to exist: poll the
    // bounded walk until the live table proves the depth pair — one
    // listed pid parenting another listed pid, re-read from the live
    // tree at that moment, never decided from one snapshot.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let (descendants, pair) = loop {
        let found = descendant_pids_of(outer.id());
        let pair = found.iter().find_map(|&pid| {
            let below = descendant_pids_of(pid);
            below
                .iter()
                .find(|&&child| found.contains(&child))
                .map(|&child| (pid, child))
        });
        if pair.is_some() || std::time::Instant::now() >= deadline {
            break (found, pair);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    // Reap the whole stand-in tree this test spawned, then its handle.
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/T", "/PID", &outer.id().to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = outer.kill();
    let _ = outer.wait();
    let Some((deep_parent, grandchild)) = pair else {
        panic!(
            "no listed pid parented another listed pid within the deadline — \
                 depth two never proven: {descendants:?}"
        );
    };
    assert!(
        descendants.contains(&grandchild),
        "the grandchild of the asked pid must be in the set: {descendants:?}"
    );
    assert!(
        !descendants.contains(&outer.id()),
        "the asked pid never enters its own descendant set: {descendants:?}"
    );
    assert!(
        descendants.len() <= DESCENDANT_CAP,
        "the walk stays bounded: {} listed over cap {DESCENDANT_CAP}",
        descendants.len()
    );
    assert!(
        descendants.contains(&deep_parent),
        "the deep parent is itself a listed descendant: {descendants:?}"
    );
}
