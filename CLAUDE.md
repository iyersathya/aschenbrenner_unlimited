# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with this repository.

## What this is

The **Floor 2x / Ceiling UNLIMITED** (v4) sleeve — `2x_unlimited_stocks.pdf`. A
$40K AGI-infrastructure barbell with a **Tier-D moonshot sleeve of LEAPS call
options + config-pinned micro-caps** on top of a large-cap stock floor: **24
positions across 4 tiers** (A Floor 50% · B Asymmetric 32.5% · D Moonshot 12.5% ·
C Cash 5%). Floor ≥$80K by month 36; uncapped tail via the LEAPS. NO crypto.

It **trades both stocks and single-leg long-call options** — the only sleeve in
the family that does options buying here (no spreads, no covered calls).

Capital-isolated paper sleeve: its own Alpaca **paper** account
(`PKL3NILMKRLVXSVV3FAWKW3AIO`, options level 3), its own gdrive vault
(`~/gdrive/vault/aschenbrenner-unlimited`), its own Telegram bot, its own config.

**Sibling sleeve:** `../aschenbrenner_portfolio` runs the stocks-only
*Floor 2x / Ceiling 10x* strategy on a different account. The two run independently
and share only the `daily_analysis` vault + ticker universe + contract fixtures.

## Run / test (from `~/projects`)

```bash
cargo build -p aschenbrenner-unlimited
cargo test  -p aschenbrenner-unlimited
cargo test  -p aschenbrenner-unlimited portfolio::targets::tests::leaps_resolve_to_expected_occ

./target/debug/aschenbrenner-unlimited test            # offline sanity (no network)
./target/debug/aschenbrenner-unlimited config          # resolved config + vault counts
./target/debug/aschenbrenner-unlimited status          # PA guard + balances
./target/debug/aschenbrenner-unlimited positions       # equity drift + clusters
./target/debug/aschenbrenner-unlimited build dry-run   # phased tranche (LEAPS show OCC)
./target/debug/aschenbrenner-unlimited daily dry-run   # maintenance rebalance, no orders
./target/debug/aschenbrenner-unlimited signals         # daily-analysis overlay for all names
./target/debug/aschenbrenner-unlimited signals GEV CEG # specific tickers
./target/debug/aschenbrenner-unlimited review          # quarterly checkpoint digest (advisory)
./target/debug/aschenbrenner-unlimited cancel          # cancel all open orders
./target/debug/aschenbrenner-unlimited notify "text"   # send Telegram test message
./target/debug/aschenbrenner-unlimited scheduler dry-run  # blocking daily loop, no orders
```

Release binary: `~/projects/target/release/aschenbrenner-unlimited`.
Production runs via `deploy/aschenbrenner-unlimited.service` (systemd user unit, ARMED).

**ARMED by default:** `build`, `daily`/`track`/`rebalance`, `scheduler` submit orders.
Pass `dry-run` to compute-and-log only. The scheduler's daily window always runs the
BUILD cycle, which self-promotes to maintenance once every position is within band.

## Module layout and data flow

```
main.rs → run.rs          CLI dispatch, verb routing
  ├── core/config.rs      AppConfig singleton (defaults → .env → env vars)
  ├── core/safety.rs      kill-switch + PA guard + cash_available_for_buys
  ├── core/alpaca.rs      Alpaca trading client (submit_stock_order / submit_option_order)
  ├── core/alpaca_data.rs Market data: get_stock_price, get_option_mid, build_occ/parse_occ,
  │                       third_friday (LEAPS expiry resolver)
  ├── core/vault.rs       File vault (atomic JSON writes, high-water marks, NAV history)
  ├── core/delivery.rs    Telegram notifier
  │
  ├── portfolio/targets.rs   Authoritative 24-position model: Tier enum, Instrument enum,
  │                          TARGETS const, instrument_symbol(), invested_target_weight()
  ├── portfolio/state.rs     PortfolioState (assembled from Alpaca account + positions)
  ├── portfolio/metrics.rs   position_metrics(), cluster_weights(), glide_path_status()
  │
  ├── signals/daily_analysis.rs  Reader for trading-agents-scheduler JSON output
  ├── rebalance/signal_bias.rs   SignalBias: re-orders rule-authorized candidates only
  ├── rebalance/rules.rs         plan_rebalance() — deterministic 5-rule maintenance engine
  ├── rebalance/build.rs         plan_build() — 4-stage phased initial accumulation
  │
  ├── execute.rs          execute_actions(): Action → marketable-LIMIT order (stocks + LEAPS)
  ├── lifecycle/daily.rs  Daily cycle: safety → snapshot → high-water → plan → execute → persist
  ├── lifecycle/review.rs Quarterly review digest (advisory)
  └── scheduler.rs        Blocking daily loop: 10:00 PT window + quarterly on first weekday of quarter
```

**Critical data-flow invariant:** `rebalance::rules` and `rebalance::build` are pure functions
over their inputs (no IO). `execute` calls them, then prices/sizes and optionally submits.
The signal never authorizes — it only re-orders.

## Tier model and instrument types

`portfolio/targets.rs` is authoritative; `portfolio.md` is its human mirror. **Both must be kept
in lockstep** — a target/rule change updates both and re-runs tests.

| Tier | Label | Weight | Rules |
|------|-------|--------|-------|
| A — Floor | `Tier::Floor` | 50% | -25% flags review; -35% advisory cut; 3x profit-take (trim 25%) |
| B — Asymmetric | `Tier::Asymmetric` | 32.5% | **No stop**; 5x profit-take (trim 30%) |
| D — Moonshot | `Tier::Moonshot` | 12.5% | LEAPS: +500% trim 50%, ≤60 DTE time-stop flag; micro-cap: 10x trim 50% |
| C — Cash | `Tier::Cash` | 5% | Deploy on cohort >15% drawdown or single name >25% drawdown |

**Equity cluster cap:** 30% of NAV across any correlated cluster (Tier A/B only). Tier D is
exempt from cluster and position caps.

`Instrument` variants:
- `Stock` — whole-share marketable LIMIT with `STOCK_BUF` (0.3%)
- `Leaps { underlying, expiry_year, strike }` — OCC symbol resolved via `build_occ` +
  `third_friday(expiry_year, 1)` (Jan 3rd Friday); option mid + `OPTION_BUF` (2%), qty in
  whole contracts (×100 notional)
- `MicroCap { slot }` — resolved from `MICROCAP_1` / `MICROCAP_2` config; `None` if unset

## Config (`.env` keys)

All env keys parsed by `core/config.rs`. Important ones not obvious from code:

| Key | Default | Notes |
|-----|---------|-------|
| `ALPACA_API_KEY_ID` / `ALPACA_API_SECRET_KEY` | (empty → advisory mode) | Absent keys = no orders |
| `ALPACA_BASE_URL` | `https://paper-api.alpaca.markets` | Must be paper host; must NOT end in `/v2` |
| `VAULT_PATH` | `~/gdrive/vault/aschenbrenner-unlimited` | File vault root |
| `DAILY_ANALYSIS_PATH` | `<vault>/../daily_analysis` | Override to point at scheduler output |
| `MICROCAP_1` / `MICROCAP_2` | `IONQ` / `ASTS` | Tier-D micro-cap slot picks |
| `SIGNAL_BIAS_ENABLED` | `true` | Set `false` to make all rebalancing fully deterministic |
| `BUILD_DAILY_BUDGET` | `2000.0` | Max $ to DCA into Tier B per day during phased build |
| `REBALANCE_BAND_PCT` | `0.03` | Ignore drift < 3% of NAV (low-churn) |
| `MAX_ORDERS_PER_DAY` | `10` | Hard daily order cap |
| `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` | (empty → disabled) | Delivery |
| `KILL_FILE` | `<project>/aschenbrenner-unlimited.HALT` | Touch to halt all cycles |

## Hard invariants (safety — never weaken)

- **Paper-only.** `safety_check` (kill-switch + PA guard) before any order path;
  `verify_paper_account` re-checks inside every stock AND option submit.
- **No market orders.** Marketable LIMIT only (`STOCK_BUF` 0.3% / `OPTION_BUF` 2%).
- **No leverage.** Total buy notional (stocks + LEAPS contracts×mid×100) capped at
  cash − buffer (`safety::cash_available_for_buys`, enforced in `execute.rs`).
- **Options = single-leg long calls only.** No spreads, no covered calls.
- **Deterministic floor.** `rebalance::rules` is the floor; `signal_bias` only
  re-orders equity choices. Tier-D lifecycle is rule-driven, not signal-driven.
- **File vault, no DB.** Persistence is atomic JSON writes (temp file + rename) to vault.
- `ALPACA_BASE_URL` is the paper host; must not end in `/v2` (clients append it).

## Sibling-repo sync obligations (lockstep)

- **Daily-analysis contract.** Vendors byte-identical fixtures under `tests/fixtures/`,
  registered in `../trading-agents-scheduler/scripts/check_contract_fixtures.sh`.
  Three canonical fixtures: `daily_analysis_contract_v1_buy.json`,
  `daily_analysis_contract_v1_fail.json`, `daily_analysis_contract_v1_buy_quant.json`.
  A schema change requires regenerating these AND every consumer's copy.
- **Ticker universe.** All equity names + LEAPS underlyings (NVDA/CRWV/IREN/OKLO)
  + the pinned micro-caps must be in `../trading-agents-scheduler/tickers.txt`.
- **Infra patterns** mirror the sibling sleeves by hand — port fixes both ways.
