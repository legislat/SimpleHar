//! The llama "Fit" cost class (R11, operator ruling 2026-11-01 / Search#40).
//!
//! The engine's own word before it spawns: `llama-fit-params.exe` (the sibling
//! of config `llama_server_exe`) run with `--fit-print on` prints
//! `NAME model ctx compute` in MiB per device, computed by the same fit code
//! the server allocates with. This module owns the PURE half of that contract:
//! the memory-relevant arg subset, the strict stdout parser, the booking
//! formula, and the args-hash that doubles as the cache/void identity.
//! Nothing here spawns anything — the probe rides the runtime's control-command
//! seam, and the scheduler owns the cache, the warm queue and the ladder.
//!
//! Doctrine pins (DECISIONS 2026-11-01, Search#40 handoff §7):
//! - **never persisted**: the figure is recomputed per proven change; nothing in
//!   this module reads or writes yaml, and no `ModelEntry` field set from a
//!   probe ever joins `raw` (the write-back source).
//! - **Host excluded**: the `Host` row is RAM-side, never VRAM-booked.
//! - **constants are calibration, not defaults**: margin/surcharge are
//!   Wave-C-calibrated config values; an unset required constant is
//!   *unconfigured* (ladder fallback), never a silent zero that pretends to be
//!   calibrated.
//! - **max truth**: the ladder comparison lives in `ModelEntry::cost_source`;
//!   this module only produces the projection.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The sibling tool that answers `--fit-print` (the server itself never does —
/// `GOTCHAS` `[doc]`: b11669 serve logs carry zero memory lines).
pub(crate) const FIT_TOOL_NAME: &str = "llama-fit-params.exe";

/// The argv that prints the tool's version: the version-lock epoch checked at
/// boot AND at launch (cache identity: a figure belongs to the calculator that
/// minted it).
pub(crate) fn version_args() -> Vec<String> {
    vec!["--version".to_string()]
}

/// The flag the probe always ends with: print the projection, adjust nothing
/// (verified: `--fit-print` alone leaves args — and the projection — exactly
/// as given, even on a card with 1.2 GiB free).
pub(crate) const FIT_PRINT_ARGS: &[&str] = &["--fit-print", "on"];

/// The fallback deadline for one probe when `engines.llama.fit_probe_timeout_s`
/// is unset: the control-command family's own floor (live probe measured
/// ~0.66 s, so this is ~15x headroom, not a guess at a calibration constant).
pub(crate) const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a fit figure could not be produced. Every variant is a warn + ladder
/// fallback, never a fabricated number.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum FitError {
    /// `fit_probe` is on but a Wave-C-calibrated constant it demands is unset:
    /// the operator has not calibrated, so nothing may book a pretence of one.
    #[error(
        "fit_probe is on but `{key}` is not configured; the constants are Wave-C-calibrated, \
         so the row falls back to the declared/estimate ladder until it is set"
    )]
    Unconfigured {
        /// The config key the operator still owes.
        key: &'static str,
    },
    /// The tool the config names is not on disk (absence is reported, never
    /// guessed past).
    #[error("fit tool not found at `{path}`; fit figures unavailable, ladder stands")]
    ToolMissing {
        /// The resolved sibling path the config implies.
        path: String,
    },
    /// The row is not probeable (no `model:` a fit-params call can take).
    #[error("cannot build a fit probe for this row: {reason}")]
    Row {
        /// What is missing or malformed in the row.
        reason: String,
    },
    /// The probe answered, but not in the strict contract (`NAME model ctx
    /// compute` MiB per line): a malformed answer is refused outright, so a
    /// silent schema drift costs the ladder, never a wrong booking.
    #[error("fit output is not the strict NAME/model/ctx/compute contract: {reason}")]
    BadOutput {
        /// The offending detail (line excerpt or emptiness).
        reason: String,
    },
}

/// The calibration constants a booking needs, resolved from config once per
/// probe plan.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FitConstants {
    /// `engines.llama.fit_margin_gb` — per ACTIVE device (context, CUDA ctx
    /// and graphs are outside the projection).
    pub margin_gb: f64,
    /// `engines.llama.mmproj_surcharge_gb` — applied only to rows carrying
    /// `mmproj:` (fit-params rejects `--mmproj`, so vision cost is absent
    /// from its projection by construction).
    pub surcharge_gb: Option<f64>,
    /// Per-probe deadline (`engines.llama.fit_probe_timeout_s`, else
    /// [`DEFAULT_PROBE_TIMEOUT`]).
    pub probe_timeout: Duration,
}

impl FitConstants {
    /// Resolves the raw config triple. `margin_gb` is REQUIRED whenever the
    /// probe is on (there is no honest zero-default for "context and graphs
    /// are free"); the surcharge is required per booking that needs it (see
    /// [`book_gb`]). Timeout has a mechanism default, not a calibration one.
    ///
    /// # Errors
    ///
    /// [`FitError::Unconfigured`] when `margin_gb` is unset.
    pub(crate) fn resolve(
        margin_gb: Option<f64>,
        surcharge_gb: Option<f64>,
        timeout_s: Option<u64>,
    ) -> Result<Self, FitError> {
        let margin_gb = margin_gb.ok_or(FitError::Unconfigured {
            key: "engines.llama.fit_margin_gb",
        })?;
        Ok(Self {
            margin_gb,
            surcharge_gb,
            probe_timeout: timeout_s.map_or(DEFAULT_PROBE_TIMEOUT, Duration::from_secs),
        })
    }
}

/// One parsed stdout row: device name plus the three MiB columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FitRow {
    /// Device name as printed (`CUDA0`, `Host`, ...).
    pub name: String,
    /// Model weights MiB.
    pub model_mib: u64,
    /// Context (KV cache) MiB.
    pub ctx_mib: u64,
    /// Compute-buffer MiB.
    pub compute_mib: u64,
}

impl FitRow {
    /// Whether this row contributes to the VRAM booking. The `Host` row is
    /// RAM-side and excluded by the ruling (it counts to RAM health
    /// separately, never to a VRAM charge).
    #[must_use]
    pub(crate) fn is_device(&self) -> bool {
        self.name != "Host"
    }

    /// The row's total MiB across the three projected columns. Saturating, so
    /// a tool answer carrying absurd columns fails the caller's plausibility
    /// (an over-GiB-scale figure) instead of wrapping to a small booking.
    #[must_use]
    fn total_mib(&self) -> u64 {
        self.model_mib
            .saturating_add(self.ctx_mib)
            .saturating_add(self.compute_mib)
    }
}

/// Resolves the calculator the config implies: the sibling
/// `llama-fit-params.exe` of `llama_server_exe` (config is king — the tool is
/// version-locked to the binary about to serve because both paths derive from
/// the one configured path).
#[must_use]
pub(crate) fn resolve_tool(llama_server_exe: &Path) -> PathBuf {
    llama_server_exe.with_file_name(FIT_TOOL_NAME)
}

/// The yaml keys whose flags the projection must see, in [`super::llama::FLAG_TABLE`]
/// order. This is the exact set the policy row declares as memory-relevant —
/// the long-form spellings of the set Search#40 probed live (`-c -ngl -b -ub
/// -fa -ctk -ctv -np`). Deliberately ABSENT: `model` (handled as `-m` first),
/// `mmproj` and `jinja` (fit-params rejects them outright), the `fit*` knobs
/// (they steer the server's adjustment, not the deterministic projection), and
/// anything no row knob carries (`-ot` is not a harness-spelled flag at all —
/// the `extra_args` allow-list never lets it in).
const FIT_MEMORY_KEYS: &[&str] = &[
    "context",
    "ngl",
    "batch",
    "ubatch",
    "flash_attn",
    "cache_type_k",
    "cache_type_v",
    "parallel",
];

/// The probe argv for one row: `-m` first, then the memory-relevant
/// [`FIT_MEMORY_KEYS`] flags resolved through the same `FLAG_TABLE` the serve
/// argv uses (one source for every spelling — the probe projects what the
/// launch will actually run), the `cpu_offload` mirror (`-ngl 0` forced, yaml
/// `ngl` suppressed, exactly like [`super::llama::build_args`]), and
/// `--fit-print on` last. Never `--host`/`--port`/`--jinja`/`--mmproj`: the
/// server-forced pair is irrelevant to a projection, and fit-params rejects
/// the template/vision pair.
///
/// Every memory key's value faces the serve path's shape floor
/// ([`super::llama::table_value_rejection`]) before it joins the probe argv
/// (R2-09-05): the probe must project the launch that can actually run, and
/// an option-shaped value the serve path refuses may never be rendered to the
/// calculator unvalidated.
///
/// # Errors
///
/// [`FitError::Row`] when the row has no `model:` value, or when a
/// memory-relevant key carries a value the serve-path shape floor refuses.
pub(crate) fn probe_args(raw: &BTreeMap<String, String>) -> Result<Vec<String>, FitError> {
    let value = |key: &str| raw.get(key).map(String::as_str).filter(|v| !v.is_empty());
    let Some(model) = value("model") else {
        return Err(FitError::Row {
            reason: "`model:` is absent or empty, so there is nothing to project".to_string(),
        });
    };
    let cpu_offload = raw
        .get("cpu_offload")
        .is_some_and(|v| super::llama::jinja_on(v));
    let mut args = vec!["-m".to_string(), model.to_string()];
    for (key, flag) in super::llama::FLAG_TABLE {
        if !FIT_MEMORY_KEYS.contains(key) || (*key == "ngl" && cpu_offload) {
            continue;
        }
        if let Some(v) = value(key) {
            if let Some(refusal) = super::llama::table_value_rejection(key, flag, v) {
                return Err(FitError::Row { reason: refusal });
            }
            args.extend([flag.to_string(), v.to_string()]);
        }
    }
    if cpu_offload {
        args.extend(["-ngl".to_string(), "0".to_string()]);
    }
    args.extend(FIT_PRINT_ARGS.iter().map(|word| word.to_string()));
    Ok(args)
}

/// The cache/void identity for a probe: a hash of the exact probe argv. The
/// cache is in-process only, so the std hasher is enough — a figure is valid
/// precisely while this argv (and the tool that ran it) are unchanged.
#[must_use]
pub(crate) fn args_hash(args: &[String]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for arg in args {
        arg.hash(&mut hasher);
        // One terminator per element so two arg lists cannot collide by
        // shifting characters across a boundary.
        0xFFu8.hash(&mut hasher);
    }
    hasher.finish()
}

/// Strict parser for `--fit-print` stdout: every non-blank line must match
/// `NAME model ctx compute` with single spaces and three unsigned columns
/// (CRLF and a TRAILING SPACE tolerated — the live 2026-11-02 capture proved
/// llama-fit-params appends a space to every row, so a parser that refused it
/// would refuse every real projection on this box). Any deviation — an empty
/// answer, a word in a numeric column, a missing or fifth column — refuses
/// the WHOLE output: there is no partial reading of a projection, and a
/// parser that invents numbers is the exact failure class the whole ruling
/// exists to retire.
///
/// # Errors
///
/// [`FitError::BadOutput`] for an empty or malformed answer.
pub(crate) fn parse_output(stdout: &str) -> Result<Vec<FitRow>, FitError> {
    let mut rows = Vec::new();
    for (index, line) in stdout.lines().enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        // Trailing whitespace is padding the probe emits, never a column;
        // trimming the END only keeps the strict single-space contract for
        // everything a real field could hide behind.
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let bad = |reason: String| FitError::BadOutput {
            reason: format!("line {}: `{line}` {reason}", index + 1),
        };
        let mut fields = line.split(' ');
        let name = fields
            .next()
            .filter(|n| !n.is_empty())
            .ok_or_else(|| bad("has no device name".to_string()))?;
        let mut column = |what: &str| -> Result<u64, FitError> {
            fields
                .next()
                .and_then(|raw| raw.parse::<u64>().ok())
                .ok_or_else(|| bad(format!("has no {what} integer column")))
        };
        let model_mib = column("model")?;
        let ctx_mib = column("ctx")?;
        let compute_mib = column("compute")?;
        if fields.next().is_some() {
            return Err(bad("carries extra columns".to_string()));
        }
        rows.push(FitRow {
            name: name.to_string(),
            model_mib,
            ctx_mib,
            compute_mib,
        });
    }
    if rows.is_empty() {
        return Err(FitError::BadOutput {
            reason: "the probe printed nothing".to_string(),
        });
    }
    Ok(rows)
}

/// The booking figure in GiB for one parsed projection:
/// `sum of NON-Host rows (model + ctx + compute, MiB→GiB) + surcharge when
/// the row carries mmproj + margin × active-device count` (DECISIONS 2026-11-01,
/// exact ruling wording). A projection with no device row books nothing real
/// and is refused like any other nonsense figure.
///
/// # Errors
///
/// [`FitError::BadOutput`] when no device row exists, [`FitError::Unconfigured`]
/// when the row needs the mmproj surcharge and the constant is unset.
pub(crate) fn book_gb(
    rows: &[FitRow],
    has_mmproj: bool,
    constants: &FitConstants,
) -> Result<f64, FitError> {
    let devices: Vec<&FitRow> = rows.iter().filter(|row| row.is_device()).collect();
    if devices.is_empty() {
        return Err(FitError::BadOutput {
            reason: "only the Host row was printed; nothing was projected onto a device"
                .to_string(),
        });
    }
    let surcharge = if has_mmproj {
        constants.surcharge_gb.ok_or(FitError::Unconfigured {
            key: "engines.llama.mmproj_surcharge_gb",
        })?
    } else {
        0.0
    };
    let sum_mib: u64 = devices
        .iter()
        .fold(0u64, |sum, row| sum.saturating_add(row.total_mib()));
    Ok(sum_mib as f64 / 1024.0 + surcharge + constants.margin_gb * devices.len() as f64)
}

/// Parse-and-book in one step: the strict parser feeding [`book_gb`].
///
/// # Errors
///
/// Any [`FitError::BadOutput`] / [`FitError::Unconfigured`] the parts report.
pub(crate) fn parse_and_book(
    stdout: &str,
    has_mmproj: bool,
    constants: &FitConstants,
) -> Result<f64, FitError> {
    book_gb(&parse_output(stdout)?, has_mmproj, constants)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// The exact captured stdout of the live probe, trailing space per line
    /// included (Search#40 §2 bytes, re-confirmed 2026-11-02 when the harness
    /// refused every projection for exactly this space): the parse fixture and
    /// its 31 717 MiB device sum are pinned together.
    const CAPTURED: &str = "CUDA0 21484 8853 1380 \nHost 994 0 296 \n";

    fn constants(margin: f64, surcharge: Option<f64>) -> FitConstants {
        FitConstants {
            margin_gb: margin,
            surcharge_gb: surcharge,
            probe_timeout: DEFAULT_PROBE_TIMEOUT,
        }
    }

    #[test]
    fn the_captured_fixture_parses_and_books_the_device_sum_excluding_host() {
        let rows = parse_output(CAPTURED).expect("the captured answer is contract-perfect");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "CUDA0");
        assert!(!rows[1].is_device(), "Host is RAM-side, never a device");
        let gib = book_gb(&rows, false, &constants(0.0, None)).expect("margin 0 is legal");
        assert_eq!(gib, 31_717.0 / 1024.0, "31 717 MiB, Host excluded");
        assert!((gib - 31.0).abs() < 0.1, "~31.0 GiB as ruled, was {gib}");
    }

    #[test]
    fn margin_scales_with_active_device_count_and_surcharge_with_mmproj() {
        let two_devices =
            parse_output("CUDA0 1000 100 100\nCUDA1 2000 200 200\nCPU 50 0 0\nHost 4 0 0\n")
                .expect("three non-Host device rows");
        let gib = book_gb(&two_devices, false, &constants(1.5, None)).expect("books");
        // (1000+100+100 + 2000+200+200 + 50) MiB = 3650 MiB over THREE devices.
        assert_eq!(gib, 3650.0 / 1024.0 + 1.5 * 3.0);
        let vision = book_gb(
            &parse_output(CAPTURED).expect("device row"),
            true,
            &constants(0.5, Some(1.25)),
        )
        .expect("a vision row books the surcharge");
        assert_eq!(vision, 31_717.0 / 1024.0 + 1.25 + 0.5);
    }

    /// TASK-R11-FIT calibration pin (C1 capture 2026-11-02 at 805c7d0,
    /// `handoff/wave-verify/logs/`): the captured rows booked with the
    /// Wave-C card constants from `wave-card-20261102-230736.json`
    /// (`fit_margin_gb` 3, `mmproj_surcharge_gb` 2) equal EXACTLY the figure
    /// the live `C1-evidence-20261102-230736.json` row recorded as `fit_gb`.
    /// The device count is ONE (only `CUDA0` is active; the `Host` row is
    /// RAM-side and excluded) and the row carries `mmproj:`
    /// (`ModelConf/qwen3.8-27b-turbo-mtp-256k.yaml`), so the ruling's formula
    /// reads: 31 717 MiB / 1024 + surcharge 2.0 + margin 3.0 x 1 device
    /// = 30.9736328125 + 2.0 + 3.0 = 35.9736328125 GiB. Every figure is a
    /// binary fraction, so the equality below is exact, not approximate.
    /// The calibration verdict: the same capture's NVML `observed_used_gb`
    /// is 35.61735153198242, i.e. the projection ran 0.35628128051758 GiB
    /// ABOVE the quiescent reading - conservative. One sample is not grounds
    /// to shrink either constant; a change needs a multi-sample card window
    /// (the operator ruling 2026-11-01 set that bar for these numbers).
    #[test]
    fn the_captured_c1_row_books_the_recorded_projection_with_the_wave_card_constants() {
        let rows = parse_output(CAPTURED).expect("the captured answer is contract-perfect");
        assert_eq!(
            rows.iter().filter(|row| row.is_device()).count(),
            1,
            "CUDA0 is the only ACTIVE device; Host is RAM-side"
        );
        let booked = book_gb(&rows, true, &constants(3.0, Some(2.0)))
            .expect("the C1 row carries mmproj, so it books the surcharge");
        assert_eq!(
            booked, 35.9736328125,
            "exactly the fit_gb the C1 evidence row carries"
        );
        let observed = 35.61735153198242;
        assert!(
            booked > observed,
            "the projection must stay above the quiescent figure"
        );
        assert!(
            (booked - observed - 0.35628128051758).abs() < 1e-9,
            "the C1 headroom is pinned at 0.35628128051758 GiB"
        );
    }

    #[test]
    fn a_vision_row_without_the_surcharge_constant_refuses_rather_than_underbooks() {
        let rows = parse_output("CUDA0 100 0 0\n").expect("device row");
        let error = book_gb(&rows, true, &constants(0.0, None)).expect_err("unset constant");
        assert!(matches!(error, FitError::Unconfigured { .. }), "{error}");
    }

    #[test]
    fn malformed_answers_are_refused_whole_not_partially_read() {
        for bad in [
            "",
            "\n\n",
            "CUDA0 21484 8853",
            "CUDA0 21484 8853 1380 7",
            "CUDA0 21484 -5 1380",
            "CUDA0 two 8853 1380",
            " 994 0 296",
            "CUDA0 21484 8853 1380\nHost nope 0 296",
        ] {
            assert!(
                parse_output(bad).is_err(),
                "must refuse, must never invent: {bad:?}"
            );
        }
    }

    #[test]
    fn a_host_only_answer_books_nothing_and_refuses() {
        let error = parse_and_book("Host 994 0 296\n", false, &constants(0.0, None))
            .expect_err("RAM is not VRAM");
        assert!(matches!(error, FitError::BadOutput { .. }), "{error}");
    }

    #[test]
    fn crlf_answers_parse_like_lf_ones() {
        let rows = parse_output("CUDA0 1 2 3\r\n").expect("windows line endings");
        assert_eq!(rows[0].total_mib(), 6);
    }

    #[test]
    fn the_live_probe_trailing_space_is_padding_not_a_fifth_column() {
        // Byte-for-byte the line the harness refused on 2026-11-02, which kept
        // every llama row on the estimate ladder: a real capture must parse.
        let rows = parse_output("CUDA0 21484 8853 1380 \r\nHost 994 0 296 \r\n")
            .expect("the live probe pads every row with a space");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].total_mib(),
            31_717,
            "the CUDA0 row is the whole device sum"
        );
        // A run of spaces INSIDE the row still breaks the strict contract,
        // and a whitespace-only line stays blank rather than a phantom row.
        assert!(parse_output("CUDA0  21484 8853 1380").is_err());
        assert!(parse_output("   \nCUDA0 1 2 3\n").is_ok_and(|rows| rows.len() == 1));
    }

    #[test]
    fn the_probe_takes_only_the_memory_relevant_flags_and_ends_with_fit_print() {
        let args = probe_args(&raw(&[
            ("model", "F:/models/turbo.gguf"),
            ("mmproj", "F:/models/vision.gguf"),
            ("context", "262144"),
            ("ngl", "99"),
            ("batch", "2048"),
            ("ubatch", "512"),
            ("flash_attn", "on"),
            ("cache_type_k", "q8_0"),
            ("cache_type_v", "q8_0"),
            ("parallel", "1"),
            ("jinja", "true"),
            ("fit", "true"),
            ("fit_target", "0.9"),
            ("fit_ctx", "2048"),
            ("extra_args", "--embedding --pooling last"),
        ]))
        .expect("probeable row");
        assert_eq!(
            args,
            vec![
                "-m",
                "F:/models/turbo.gguf",
                "-c",
                "262144",
                "-ngl",
                "99",
                "-b",
                "2048",
                "--ubatch-size",
                "512",
                "--flash-attn",
                "on",
                "--cache-type-k",
                "q8_0",
                "--cache-type-v",
                "q8_0",
                "--parallel",
                "1",
                "--fit-print",
                "on",
            ],
            "vision/template/server-only words never reach the calculator"
        );
    }

    #[test]
    fn cpu_offload_mirrors_the_real_launch_ngl_suppression() {
        let args = probe_args(&raw(&[
            ("model", "m.gguf"),
            ("ngl", "99"),
            ("cpu_offload", "true"),
        ]))
        .expect("offloaded row");
        let joined = args.join(" ");
        assert!(joined.contains("-ngl 0"), "{joined}");
        assert!(
            !joined.contains(" 99"),
            "yaml ngl suppressed like build_args"
        );
    }

    #[test]
    fn an_unprobeable_row_refuses() {
        assert!(matches!(
            probe_args(&raw(&[("context", "4096")])),
            Err(FitError::Row { .. })
        ));
    }

    /// R2-09-05 falsifier: the serve path refuses an option-shaped table value
    /// (`build_args` + the shape floor). The probe must refuse the same value,
    /// naming the key, the flag and the detail — never hand `--api-key` to the
    /// calculator as the value of `-c`. The clean-row leg stays pinned to the
    /// same argv `the_probe_takes_only_the_memory_relevant_flags...` expects.
    #[test]
    fn the_probe_refuses_a_value_the_serve_path_refuses() {
        let error = probe_args(&raw(&[("model", "m"), ("context", "--api-key")]))
            .expect_err("an option-shaped value may not reach the probe");
        let FitError::Row { reason } = error else {
            panic!("a refused row value is a Row error, got {error:?}");
        };
        assert!(reason.contains("context"), "names the key: {reason}");
        assert!(reason.contains("-c"), "names the flag: {reason}");
        assert!(reason.contains("--api-key"), "names the value: {reason}");
        assert!(
            reason.contains("option-shaped"),
            "carries the floor's detail: {reason}"
        );
        let clean = probe_args(&raw(&[("model", "m"), ("context", "4096")])).expect("clean row");
        assert_eq!(clean, vec!["-m", "m", "-c", "4096", "--fit-print", "on"]);
    }

    /// The exact-set pin the brief demands: every FIT key answers to a
    /// `FLAG_TABLE` spelling, and no key fit-params rejects or merely steers
    /// the server's adjustment is in the list.
    #[test]
    fn the_memory_flag_set_is_all_and_only_the_policy_row_declares() {
        for key in FIT_MEMORY_KEYS {
            assert!(
                super::super::llama::FLAG_TABLE
                    .iter()
                    .any(|(k, _)| k == key),
                "fit flag `{key}` must come from FLAG_TABLE, never be invented"
            );
        }
        for forbidden in ["mmproj", "jinja", "fit", "fit_target", "fit_ctx", "model"] {
            assert!(
                !FIT_MEMORY_KEYS.contains(&forbidden),
                "fit-params rejects or does not consume `{forbidden}`"
            );
        }
    }

    #[test]
    fn the_hash_is_the_args_identity() {
        let base = probe_args(&raw(&[("model", "m"), ("context", "262144")])).expect("probe");
        let edited = probe_args(&raw(&[("model", "m"), ("context", "131072")])).expect("probe");
        let same = probe_args(&raw(&[("model", "m"), ("context", "262144")])).expect("probe");
        assert_eq!(
            args_hash(&base),
            args_hash(&same),
            "same args, same identity"
        );
        assert_ne!(
            args_hash(&base),
            args_hash(&edited),
            "a ctx change is a proven change: the cache voids"
        );
    }

    #[test]
    fn the_tool_is_the_config_sibling() {
        let exe = Path::new(r"D:\llama\llama-server.exe");
        assert_eq!(
            resolve_tool(exe),
            PathBuf::from(r"D:\llama\llama-fit-params.exe")
        );
    }

    #[test]
    fn an_unset_margin_is_unconfigured_not_zero_defaulted() {
        let error = FitConstants::resolve(None, Some(1.0), None).expect_err("no fake default");
        assert!(matches!(error, FitError::Unconfigured { .. }), "{error}");
        let ok = FitConstants::resolve(Some(2.0), None, Some(4)).expect("margin set");
        assert_eq!(ok.probe_timeout, Duration::from_secs(4));
    }
}
