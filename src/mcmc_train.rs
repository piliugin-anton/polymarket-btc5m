//! Metropolis–Hastings training over strategy tunables using offline **replay-only** scores from
//! JSONL round logs (same snap replay rules as [`crate::signal_eval`] / `signal-eval`).
//! After the chain, prints **simulated realized PnL** (USDC) for the initial state and posterior mean
//! using the same STRONG autotrading sim defaults as `signal-eval` when sim CLI flags are omitted.
//!
//! Defaults (auto step, burn-in fraction, random seed printed) favor quick runs over rigorous
//! convergence. Prefer fixed `--seed` and the printed proposal `step` when you need reproducibility.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rand_distr::{Distribution, Normal};
use serde::Deserialize;

use crate::market_profile::MarketProfile;
use crate::round_log::RoundLogStrategyTunables;
use crate::signal_eval::{
    load_offline_rounds_filtered, replay_sim_realized_pnl_usdc, replay_strategy_metrics,
    tunable_overrides_global, EvalMode, ReplayStrategyMetrics, RoundAccum,
};

const DEFAULT_ITERATIONS: usize = 12_000;
const PILOT_ITERATIONS: usize = 800;
const TARGET_ACC_LOW: f64 = 0.22;
const TARGET_ACC_HIGH: f64 = 0.32;
/// Per-dimension proposal scales for θ; global `step` multiplies these.
const PROP_DIM_SCALE: [f64; 4] = [0.06, 0.05, 3.0, 0.08];
const PRIOR_MU: [f64; 4] = [1.0, 1.0, 5.0, 0.6];
const PRIOR_SIGMA: [f64; 4] = [0.35, 0.25, 15.0, 0.22];

/// Clamps to the same bounds applied inside [`crate::strategy::evaluate_manual_signal`].
fn sanitize_tunables_like_eval(t: &RoundLogStrategyTunables) -> RoundLogStrategyTunables {
    RoundLogStrategyTunables {
        strong_gap_mult: if t.strong_gap_mult.is_finite() {
            t.strong_gap_mult.clamp(0.55, 1.15)
        } else {
            1.0
        },
        max_spread_mult: if t.max_spread_mult.is_finite() {
            t.max_spread_mult.clamp(1.0, 1.35)
        } else {
            1.0
        },
        min_top_ask_shares: if t.min_top_ask_shares.is_finite() {
            t.min_top_ask_shares.clamp(2.0, 50.0)
        } else {
            5.0
        },
        watch_ratio: if t.watch_ratio.is_finite() {
            t.watch_ratio.clamp(0.25, 0.90)
        } else {
            0.60
        },
    }
}

fn tunable_to_array(t: &RoundLogStrategyTunables) -> [f64; 4] {
    [
        t.strong_gap_mult,
        t.max_spread_mult,
        t.min_top_ask_shares,
        t.watch_ratio,
    ]
}

fn array_to_tunable(a: [f64; 4]) -> RoundLogStrategyTunables {
    RoundLogStrategyTunables {
        strong_gap_mult: a[0],
        max_spread_mult: a[1],
        min_top_ask_shares: a[2],
        watch_ratio: a[3],
    }
}

fn log_prior(theta: &RoundLogStrategyTunables) -> f64 {
    let s = sanitize_tunables_like_eval(theta);
    let x = tunable_to_array(&s);
    let mut sum = 0.0;
    for i in 0..4 {
        let z = (x[i] - PRIOR_MU[i]) / PRIOR_SIGMA[i];
        sum -= 0.5 * z * z;
    }
    sum
}

fn log_target(
    rep: &ReplayStrategyMetrics,
    mode: EvalMode,
    temperature: f64,
    watch_weight: f64,
    theta: &RoundLogStrategyTunables,
) -> f64 {
    if temperature <= 0.0 || !temperature.is_finite() {
        return f64::NEG_INFINITY;
    }
    let w = match mode {
        EvalMode::StrongOnly => 0.0,
        EvalMode::WatchAsHint => watch_weight,
    };
    let score = rep.strong_correct as f64 + w * rep.watch_correct as f64;
    score / temperature + log_prior(theta)
}

fn eval_log_target(
    rounds: &HashMap<String, RoundAccum>,
    mode: EvalMode,
    temperature: f64,
    watch_weight: f64,
    theta: &RoundLogStrategyTunables,
) -> (f64, ReplayStrategyMetrics) {
    let overrides = tunable_overrides_global(theta);
    let rep = replay_strategy_metrics(rounds, mode, &overrides);
    (
        log_target(&rep, mode, temperature, watch_weight, theta),
        rep,
    )
}

fn propose(
    theta: &RoundLogStrategyTunables,
    step: f64,
    rng: &mut StdRng,
    std_n: &Normal<f64>,
) -> RoundLogStrategyTunables {
    if !(step > 0.0 && step.is_finite()) {
        return *theta;
    }
    let mut a = tunable_to_array(theta);
    for i in 0..4 {
        a[i] += step * PROP_DIM_SCALE[i] * std_n.sample(rng);
    }
    array_to_tunable(a)
}

/// Returns (acceptance_rate, final_theta_after_chain).
fn mcmc_chain_acceptance(
    rng: &mut StdRng,
    rounds: &HashMap<String, RoundAccum>,
    mode: EvalMode,
    temperature: f64,
    watch_weight: f64,
    start: RoundLogStrategyTunables,
    n: usize,
    step: f64,
    trace_out: &mut Option<Vec<RoundLogStrategyTunables>>,
) -> (f64, RoundLogStrategyTunables) {
    let std_n = Normal::new(0.0, 1.0).expect("standard normal");
    let (mut logp, _) = eval_log_target(
        rounds,
        mode,
        temperature,
        watch_weight,
        &start,
    );
    let mut theta = start;
    let mut accepts = 0usize;
    for _ in 0..n {
        let prop = propose(&theta, step, rng, &std_n);
        if !tunable_to_array(&prop).iter().all(|x| x.is_finite()) {
            continue;
        }
        let (logp2, _) = eval_log_target(
            rounds,
            mode,
            temperature,
            watch_weight,
            &prop,
        );
        let log_ratio = (logp2 - logp).min(700.0);
        if log_ratio >= 0.0 || rng.random::<f64>() < log_ratio.exp() {
            theta = prop;
            logp = logp2;
            accepts += 1;
        }
        if let Some(v) = trace_out.as_mut() {
            v.push(theta);
        }
    }
    (accepts as f64 / n.max(1) as f64, theta)
}

fn tune_step(
    rng: &mut StdRng,
    rounds: &HashMap<String, RoundAccum>,
    mode: EvalMode,
    temperature: f64,
    watch_weight: f64,
    start: RoundLogStrategyTunables,
) -> (f64, f64) {
    let mut step = 1.0_f64;
    let mut last_acc = 0.0_f64;
    for _ in 0..10 {
        last_acc = mcmc_chain_acceptance(
            rng,
            rounds,
            mode,
            temperature,
            watch_weight,
            start,
            PILOT_ITERATIONS,
            step,
            &mut None,
        )
        .0;
        if last_acc >= TARGET_ACC_LOW && last_acc <= TARGET_ACC_HIGH {
            break;
        }
        if last_acc > TARGET_ACC_HIGH {
            step *= 1.15;
        } else {
            step /= 1.15;
        }
        step = step.clamp(0.03125, 64.0);
    }
    (step, last_acc)
}

fn default_burn_in(iterations: usize) -> usize {
    ((iterations as f64) * 0.25).floor() as usize
}

fn burn_in_effective(
    iterations: usize,
    burn_in_opt: Option<usize>,
    burn_in_pct_opt: Option<f64>,
) -> Result<usize> {
    if burn_in_opt.is_some() && burn_in_pct_opt.is_some() {
        bail!("use only one of --burn-in and --burn-in-pct");
    }
    if let Some(n) = burn_in_opt {
        if n >= iterations {
            bail!("--burn-in must be less than --iterations");
        }
        return Ok(n);
    }
    if let Some(p) = burn_in_pct_opt {
        if !(p > 0.0 && p < 1.0 && p.is_finite()) {
            bail!("--burn-in-pct must be in (0, 1)");
        }
        return Ok(((iterations as f64) * p).floor() as usize);
    }
    Ok(default_burn_in(iterations).min(10_000).min(iterations.saturating_sub(1)))
}

fn quantile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let n = sorted.len();
    let idx = ((n.saturating_sub(1)) as f64 * p).clamp(0.0, (n - 1) as f64);
    let lo = idx.floor() as usize;
    let hi = idx.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    let t = idx - lo as f64;
    sorted[lo] * (1.0 - t) + sorted[hi] * t
}

fn default_chain_start() -> RoundLogStrategyTunables {
    RoundLogStrategyTunables {
        strong_gap_mult: 1.0,
        max_spread_mult: 1.0,
        min_top_ask_shares: 5.0,
        watch_ratio: 0.6,
    }
}

#[derive(Debug, Deserialize)]
struct ModelJsonPartial {
    #[serde(default, alias = "version")]
    _version: u32,
    strong_gap_mult: Option<f64>,
    max_spread_mult: Option<f64>,
    min_top_ask_shares: Option<f64>,
    watch_ratio: Option<f64>,
}

fn tunables_from_model_json(text: &str) -> Result<RoundLogStrategyTunables> {
    let m: ModelJsonPartial = serde_json::from_str(text).context("parse model JSON")?;
    let d = default_chain_start();
    Ok(RoundLogStrategyTunables {
        strong_gap_mult: m.strong_gap_mult.unwrap_or(d.strong_gap_mult),
        max_spread_mult: m.max_spread_mult.unwrap_or(d.max_spread_mult),
        min_top_ask_shares: m.min_top_ask_shares.unwrap_or(d.min_top_ask_shares),
        watch_ratio: m.watch_ratio.unwrap_or(d.watch_ratio),
    })
}

fn try_infer_model_id(day: &str, paths: &[PathBuf]) -> Result<String> {
    if paths.len() != 1 {
        bail!(
            "expected exactly one JSONL for model_id inference; got {} (use --profile)",
            paths.len()
        );
    }
    let stem = paths[0]
        .file_stem()
        .and_then(|s| s.to_str())
        .with_context(|| format!("bad path {:?}", paths[0]))?;
    let rest = stem
        .strip_prefix(day)
        .with_context(|| format!("stem {:?} does not start with day {}", stem, day))?;
    if rest.is_empty() {
        bail!(
            "round log stem {:?} is day-only; use a profile-specific file (e.g. …-sol-5m.jsonl) or pass --profile / --write-model-path",
            stem
        );
    }
    let suffix = rest.strip_prefix('-').unwrap_or(rest);
    if suffix.is_empty() {
        bail!("could not parse profile suffix from stem {stem:?}");
    }
    let p = MarketProfile::parse_cli_token(suffix)?;
    Ok(crate::signal::model_id(&p))
}

fn resolve_model_id_for_outputs(
    day: Option<&str>,
    profile: Option<&str>,
    paths: &[PathBuf],
) -> Result<String> {
    if let Some(d) = day {
        if let Ok(id) = try_infer_model_id(d, paths) {
            return Ok(id);
        }
    }
    if let Some(p) = profile {
        let mp = MarketProfile::parse_cli_token(p).map_err(|e| anyhow!("{e:#}"))?;
        return Ok(crate::signal::model_id(&mp));
    }
    if let Some(d) = day {
        try_infer_model_id(d, paths)
    } else {
        bail!(
            "for --write-model / --install-model pass --day with a profile-specific log stem (…-sol-5m.jsonl), or set --profile, or pass --write-model-path for export-only"
        );
    }
}

fn signal_model_dir(cli_override: Option<PathBuf>) -> PathBuf {
    cli_override
        .or_else(|| {
            std::env::var_os("POLYMARKET_SIGNAL_MODEL_DIR")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from("./models"))
}

fn print_mcmc_train_usage() {
    let it = DEFAULT_ITERATIONS;
    eprintln!(
        "\
Usage: mcmc-train [OPTIONS]

Offline Metropolis–Hastings on strategy tunables (JSONL round logs under --dir).

Options:
  --dir PATH              Round log directory (default ./data/rounds)
  --day YYYY-MM-DD        Filter logs by calendar day
  --profile TOKEN         e.g. sol-5m — required when multiple logs match --day
  --model-dir PATH        Model output dir (default: POLYMARKET_SIGNAL_MODEL_DIR or ./models)
  --write-model           Write signal-compatible JSON (default path: MODEL_DIR/trained/DAY_MODELID.json)
  --write-model-path PATH Full path for export (skips default trained/ name; --day optional)
  --install-model         Also write MODEL_DIR/MODELID.json (live signal filename)
  --warm-start            If MODEL_DIR/MODELID.json exists, use as MCMC initial state
  --init-model PATH       Initial state from JSON (overrides --warm-start); file must exist
  --mode strong-only|watch-as-hint
  --iterations N          (default {it})
  --burn-in N | --burn-in-pct P
  --seed U64
  --temperature FLOAT
  --watch-weight FLOAT
  --no-auto-step          Use fixed --step
  --step FLOAT
  --json                  Machine-readable summary on stdout

Also prints simulated realized PnL (USDC) for the initial state and posterior mean (same defaults as signal-eval sim).

Model dir for export/warm-start/install follows POLYMARKET_SIGNAL_MODEL_DIR when set (see .env.example).
",
    );
}

pub fn run_cli(args: &[String]) -> Result<()> {
    if matches!(args.first().map(|s| s.as_str()), Some("-h") | Some("--help")) {
        print_mcmc_train_usage();
        return Ok(());
    }

    let mut dir = PathBuf::from("./data/rounds");
    let mut day: Option<String> = None;
    let mut profile: Option<String> = None;
    let mut model_dir_cli: Option<PathBuf> = None;
    let mut write_model = false;
    let mut write_model_path: Option<PathBuf> = None;
    let mut install_model = false;
    let mut warm_start = false;
    let mut init_model: Option<PathBuf> = None;
    let mut mode = EvalMode::StrongOnly;
    let mut iterations = DEFAULT_ITERATIONS;
    let mut burn_in: Option<usize> = None;
    let mut burn_in_pct: Option<f64> = None;
    let mut seed: Option<u64> = None;
    let mut temperature = 1.0_f64;
    let mut watch_weight = 0.25_f64;
    let mut auto_step = true;
    let mut step_manual = 1.0_f64;
    let mut json_out = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    bail!("--dir requires a path");
                };
                dir = PathBuf::from(p);
            }
            "--day" => {
                i += 1;
                let Some(d) = args.get(i) else {
                    bail!("--day requires YYYY-MM-DD");
                };
                day = Some(d.clone());
            }
            "--profile" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    bail!("--profile requires a token (e.g. sol-5m)");
                };
                profile = Some(p.clone());
            }
            "--model-dir" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    bail!("--model-dir requires a path");
                };
                model_dir_cli = Some(PathBuf::from(p));
            }
            "--write-model" => {
                write_model = true;
            }
            "--write-model-path" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    bail!("--write-model-path requires a path");
                };
                write_model_path = Some(PathBuf::from(p));
                write_model = true;
            }
            "--install-model" => {
                install_model = true;
            }
            "--warm-start" => {
                warm_start = true;
            }
            "--init-model" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    bail!("--init-model requires a path");
                };
                init_model = Some(PathBuf::from(p));
            }
            "--mode" => {
                i += 1;
                let Some(m) = args.get(i) else {
                    bail!("--mode requires strong-only|watch-as-hint");
                };
                mode = EvalMode::parse(m).with_context(|| format!("unknown mode {m}"))?;
            }
            "--iterations" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--iterations requires a positive integer");
                };
                iterations = x.parse().context("iterations")?;
                if iterations < 2 {
                    bail!("--iterations must be at least 2");
                }
            }
            "--burn-in" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--burn-in requires a non-negative integer");
                };
                burn_in = Some(x.parse().context("burn-in")?);
            }
            "--burn-in-pct" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--burn-in-pct requires a float in (0,1)");
                };
                burn_in_pct = Some(x.parse().context("burn-in-pct")?);
            }
            "--seed" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--seed requires u64");
                };
                seed = Some(x.parse().context("seed")?);
            }
            "--temperature" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--temperature requires a positive float");
                };
                temperature = x.parse().context("temperature")?;
            }
            "--watch-weight" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--watch-weight requires a non-negative float");
                };
                watch_weight = x.parse().context("watch-weight")?;
            }
            "--no-auto-step" => {
                auto_step = false;
            }
            "--step" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--step requires a positive float");
                };
                step_manual = x.parse().context("step")?;
                if step_manual <= 0.0 || !step_manual.is_finite() {
                    bail!("--step must be finite and > 0");
                }
            }
            "--json" => {
                json_out = true;
            }
            other => bail!("unknown argument: {other}"),
        }
        i += 1;
    }

    let model_base = signal_model_dir(model_dir_cli.clone());
    let (rounds, paths) =
        load_offline_rounds_filtered(&dir, day.as_deref(), profile.as_deref())?;
    if rounds.is_empty() {
        bail!("no JSONL round data under {} (use --dir / --day)", dir.display());
    }

    let need_model_id = install_model || (write_model && write_model_path.is_none());
    if (write_model && write_model_path.is_none()) && day.is_none() {
        bail!("--write-model without --write-model-path requires --day");
    }
    let model_id_for_io: Option<String> = if need_model_id {
        Some(resolve_model_id_for_outputs(
            day.as_deref(),
            profile.as_deref(),
            &paths,
        )?)
    } else {
        None
    };

    let start = if let Some(ref p) = init_model {
        let text =
            std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?;
        sanitize_tunables_like_eval(&tunables_from_model_json(&text)?)
    } else if warm_start {
        match resolve_model_id_for_outputs(day.as_deref(), profile.as_deref(), &paths) {
            Ok(id) => {
                let wp = model_base.join(format!("{id}.json"));
                if wp.is_file() {
                    let text = std::fs::read_to_string(&wp)
                        .with_context(|| format!("read {}", wp.display()))?;
                    sanitize_tunables_like_eval(&tunables_from_model_json(&text)?)
                } else {
                    eprintln!(
                        "mcmc-train: --warm-start: no file at {}; using default initial state",
                        wp.display()
                    );
                    default_chain_start()
                }
            }
            Err(_) => {
                eprintln!(
                    "mcmc-train: --warm-start skipped (could not resolve model_id; use --day with …-asset-tf.jsonl or --profile)"
                );
                default_chain_start()
            }
        }
    } else {
        default_chain_start()
    };

    let seed_eff = seed.unwrap_or_else(rand::random::<u64>);
    let mut rng = StdRng::seed_from_u64(seed_eff);

    let (step_eff, pilot_acc) = if auto_step {
        tune_step(
            &mut rng,
            &rounds,
            mode,
            temperature,
            watch_weight,
            start,
        )
    } else {
        (step_manual, f64::NAN)
    };

    let burn = burn_in_effective(iterations, burn_in, burn_in_pct)?;
    if burn >= iterations {
        bail!("effective burn-in must be less than iterations");
    }

    let mut trace: Option<Vec<RoundLogStrategyTunables>> = Some(Vec::with_capacity(iterations));
    let (acc_main, _) = mcmc_chain_acceptance(
        &mut rng,
        &rounds,
        mode,
        temperature,
        watch_weight,
        start,
        iterations,
        step_eff,
        &mut trace,
    );

    let post: Vec<RoundLogStrategyTunables> = trace
        .take()
        .unwrap_or_default()
        .into_iter()
        .skip(burn)
        .collect();
    if post.is_empty() {
        bail!("burn-in consumed entire chain");
    }

    let n = post.len() as f64;
    let mean = |idx: usize| post.iter().map(|t| tunable_to_array(t)[idx]).sum::<f64>() / n;

    let mut dim0: Vec<f64> = post.iter().map(|t| t.strong_gap_mult).collect();
    let mut dim1: Vec<f64> = post.iter().map(|t| t.max_spread_mult).collect();
    let mut dim2: Vec<f64> = post.iter().map(|t| t.min_top_ask_shares).collect();
    let mut dim3: Vec<f64> = post.iter().map(|t| t.watch_ratio).collect();
    dim0.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    dim1.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    dim2.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    dim3.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let q = |v: &[f64], p: f64| quantile(v, p);

    let mean_sg = mean(0);
    let mean_ms = mean(1);
    let mean_mtas = mean(2);
    let mean_wr = mean(3);

    let mean_tun = sanitize_tunables_like_eval(&RoundLogStrategyTunables {
        strong_gap_mult: mean_sg,
        max_spread_mult: mean_ms,
        min_top_ask_shares: mean_mtas,
        watch_ratio: mean_wr,
    });
    let pnl_start =
        replay_sim_realized_pnl_usdc(&rounds, mode, &tunable_overrides_global(&start));
    let pnl_mean =
        replay_sim_realized_pnl_usdc(&rounds, mode, &tunable_overrides_global(&mean_tun));

    if write_model || install_model {
        let sources: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let export_obj = serde_json::json!({
            "version": 1_u32,
            "strong_gap_mult": mean_sg,
            "max_spread_mult": mean_ms,
            "min_top_ask_shares": mean_mtas,
            "watch_ratio": mean_wr,
            "trained_day": day,
            "model_id": model_id_for_io,
            "seed": seed_eff,
            "iterations": iterations,
            "burn_in": burn,
            "acceptance_rate": acc_main,
            "source_jsonl": sources,
            "sim_realized_pnl_usdc_initial": pnl_start,
            "sim_realized_pnl_usdc": pnl_mean,
        });
        let pretty = serde_json::to_string_pretty(&export_obj)?;

        if write_model {
            let export_path = if let Some(ref p) = write_model_path {
                p.clone()
            } else {
                let day_s = day.as_deref().expect("validated with need_model_id");
                let id = model_id_for_io.as_ref().expect("model id");
                let trained = model_base.join("trained");
                trained.join(format!("{day_s}_{id}.json"))
            };
            if let Some(parent) = export_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&export_path, &pretty)
                .with_context(|| format!("write {}", export_path.display()))?;
            if !json_out {
                eprintln!("mcmc-train: wrote model {}", export_path.display());
            }
        }
        if install_model {
            let id = model_id_for_io.as_ref().expect("model id for install");
            let live = model_base.join(format!("{id}.json"));
            if let Some(parent) = live.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&live, &pretty)
                .with_context(|| format!("write {}", live.display()))?;
            if !json_out {
                eprintln!("mcmc-train: installed live model {}", live.display());
            }
        }
    }

    if json_out {
        let pilot_val = if pilot_acc.is_finite() {
            serde_json::Value::from(pilot_acc)
        } else {
            serde_json::Value::Null
        };
        let obj = serde_json::json!({
            "seed": seed_eff,
            "step": step_eff,
            "pilot_iterations": if auto_step { PILOT_ITERATIONS } else { 0 },
            "pilot_acceptance": pilot_val,
            "burn_in": burn,
            "iterations": iterations,
            "acceptance_rate": acc_main,
            "temperature": temperature,
            "watch_weight": watch_weight,
            "mode": match mode {
                EvalMode::StrongOnly => "strong-only",
                EvalMode::WatchAsHint => "watch-as-hint",
            },
            "posterior_mean": {
                "strong_gap_mult": mean_sg,
                "max_spread_mult": mean_ms,
                "min_top_ask_shares": mean_mtas,
                "watch_ratio": mean_wr,
            },
            "quantiles": {
                "strong_gap_mult": { "p05": q(&dim0, 0.05), "p50": q(&dim0, 0.5), "p95": q(&dim0, 0.95) },
                "max_spread_mult": { "p05": q(&dim1, 0.05), "p50": q(&dim1, 0.5), "p95": q(&dim1, 0.95) },
                "min_top_ask_shares": { "p05": q(&dim2, 0.05), "p50": q(&dim2, 0.5), "p95": q(&dim2, 0.95) },
                "watch_ratio": { "p05": q(&dim3, 0.05), "p50": q(&dim3, 0.5), "p95": q(&dim3, 0.95) },
            },
            "sim_realized_pnl_usdc_initial": pnl_start,
            "sim_realized_pnl_usdc_posterior_mean": pnl_mean,
        });
        println!("{}", serde_json::to_string(&obj)?);
    } else {
        let pilot_s = if pilot_acc.is_finite() {
            format!("{pilot_acc:.4}")
        } else {
            "n/a".to_string()
        };
        eprintln!(
            "mcmc-train: seed={seed_eff}  step={step_eff:.6}  pilot_iters={}  pilot_acceptance={}  burn_in={burn}  main_acceptance={:.4}",
            if auto_step { PILOT_ITERATIONS } else { 0 },
            pilot_s,
            acc_main
        );
        println!("posterior mean (post burn-in, n={}):", post.len());
        println!("  STRATEGY_STRONG_GAP_MULT={mean_sg:.6}");
        println!("  STRATEGY_MAX_SPREAD_MULT={mean_ms:.6}");
        println!("  STRATEGY_MIN_TOP_ASK_SHARES={mean_mtas:.6}");
        println!("  STRATEGY_WATCH_RATIO={mean_wr:.6}");
        let mode_s = match mode {
            EvalMode::StrongOnly => "strong-only",
            EvalMode::WatchAsHint => "watch-as-hint",
        };
        println!("sim realized PnL (USDC, signal-eval default autotrading sim, {mode_s}):");
        println!("  initial MCMC state: {pnl_start:.4}");
        println!("  posterior mean:     {pnl_mean:.4}");
        println!("marginal quantiles (p05 / p50 / p95):");
        println!(
            "  strong_gap_mult: {:.4} / {:.4} / {:.4}",
            q(&dim0, 0.05),
            q(&dim0, 0.5),
            q(&dim0, 0.95)
        );
        println!(
            "  max_spread_mult: {:.4} / {:.4} / {:.4}",
            q(&dim1, 0.05),
            q(&dim1, 0.5),
            q(&dim1, 0.95)
        );
        println!(
            "  min_top_ask_shares: {:.4} / {:.4} / {:.4}",
            q(&dim2, 0.05),
            q(&dim2, 0.5),
            q(&dim2, 0.95)
        );
        println!(
            "  watch_ratio: {:.4} / {:.4} / {:.4}",
            q(&dim3, 0.05),
            q(&dim3, 0.5),
            q(&dim3, 0.95)
        );
        eprintln!("re-run with: --seed {seed_eff} --no-auto-step --step {step_eff:.6} …");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn tunables_from_model_json_merges_defaults() {
        let t = tunables_from_model_json(r#"{"version":1,"strong_gap_mult":0.9}"#).unwrap();
        assert!((t.strong_gap_mult - 0.9).abs() < 1e-9);
        assert!((t.max_spread_mult - 1.0).abs() < 1e-9);
        assert!((t.min_top_ask_shares - 5.0).abs() < 1e-9);
        assert!((t.watch_ratio - 0.6).abs() < 1e-9);
    }

    #[test]
    fn write_model_creates_trained_file() {
        let base = std::env::temp_dir().join(format!("mcmc_wm_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("rounds")).unwrap();
        let rounds_path = base.join("rounds/2026-05-10-sol-5m.jsonl");
        let mut f = std::fs::File::create(&rounds_path).unwrap();
        writeln!(
            f,
            r#"{{"t":"open","v":1,"cid":"c1","slug":"s","ws":1000,"we":1300,"ptb":100.0,"up":"u","down":"d","asset":"SOL","strong_gap_mult":1.0,"max_spread_mult":1.0,"min_top_ask_shares":5.0,"watch_ratio":0.6}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"t":"snap","v":1,"cid":"c1","ts":1100,"spot":100.5,"ptb":100.0,"sig":0,"sent":3,"ubu":0.4,"uba":0.41,"dbu":0.58,"dba":0.59,"ubas":10.0,"dbas":10.0,"secs":200,"act":50.0}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"t":"close","v":1,"cid":"c1","ts_close":1300,"spot_last":100.6,"win":"up","src":"approx_spot"}}"#
        )
        .unwrap();

        let model_dir = base.join("models");
        let args: Vec<String> = [
            "--dir",
            base.join("rounds").to_str().unwrap(),
            "--day",
            "2026-05-10",
            "--profile",
            "sol-5m",
            "--model-dir",
            model_dir.to_str().unwrap(),
            "--iterations",
            "200",
            "--burn-in",
            "50",
            "--seed",
            "42",
            "--write-model",
            "--no-auto-step",
            "--step",
            "0.4",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        run_cli(&args).unwrap();

        let out = model_dir.join("trained/2026-05-10_sol_5m.json");
        assert!(out.is_file(), "missing {}", out.display());
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(v.get("version").and_then(|x| x.as_u64()), Some(1));
        assert!(v.get("strong_gap_mult").and_then(|x| x.as_f64()).is_some());

        run_cli(&args).unwrap();
        let v2: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(v, v2);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn warm_start_reads_model_json() {
        let base = std::env::temp_dir().join(format!("mcmc_ws_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("rounds")).unwrap();
        let rounds_path = base.join("rounds/2026-05-10-sol-5m.jsonl");
        let mut f = std::fs::File::create(&rounds_path).unwrap();
        writeln!(
            f,
            r#"{{"t":"open","v":1,"cid":"c1","slug":"s","ws":1000,"we":1300,"ptb":100.0,"up":"u","down":"d","asset":"SOL","strong_gap_mult":1.0,"max_spread_mult":1.0,"min_top_ask_shares":5.0,"watch_ratio":0.6}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"t":"snap","v":1,"cid":"c1","ts":1100,"spot":100.5,"ptb":100.0,"sig":0,"sent":3,"ubu":0.4,"uba":0.41,"dbu":0.58,"dba":0.59,"ubas":10.0,"dbas":10.0,"secs":200,"act":50.0}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"t":"close","v":1,"cid":"c1","ts_close":1300,"spot_last":100.6,"win":"up","src":"approx_spot"}}"#
        )
        .unwrap();

        let model_dir = base.join("models");
        std::fs::create_dir_all(&model_dir).unwrap();
        std::fs::write(
            model_dir.join("sol_5m.json"),
            r#"{"version":1,"strong_gap_mult":0.95,"max_spread_mult":1.02,"min_top_ask_shares":8.0,"watch_ratio":0.55}"#,
        )
        .unwrap();

        let args: Vec<String> = [
            "--dir",
            base.join("rounds").to_str().unwrap(),
            "--day",
            "2026-05-10",
            "--profile",
            "sol-5m",
            "--model-dir",
            model_dir.to_str().unwrap(),
            "--iterations",
            "120",
            "--burn-in",
            "30",
            "--seed",
            "7",
            "--warm-start",
            "--no-auto-step",
            "--step",
            "0.5",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        run_cli(&args).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn zero_step_always_accepts() {
        let rounds: HashMap<String, RoundAccum> = HashMap::new();
        let mut rng = StdRng::seed_from_u64(99);
        let start = RoundLogStrategyTunables {
            strong_gap_mult: 1.0,
            max_spread_mult: 1.0,
            min_top_ask_shares: 5.0,
            watch_ratio: 0.6,
        };
        let (acc, end) = mcmc_chain_acceptance(
            &mut rng,
            &rounds,
            EvalMode::StrongOnly,
            1.0,
            0.0,
            start,
            50,
            0.0,
            &mut None,
        );
        assert!((acc - 1.0).abs() < 1e-9);
        assert_eq!(end, start);
    }
}
