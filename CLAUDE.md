# CLAUDE.md — aschenbrenner-unlimited

Guidance for Claude Code when working in this repo. Read this before editing.

## What this is

The **Floor 2x / Ceiling UNLIMITED** (v4) sleeve — `2x_unlimited_stocks.pdf`. A
$40K AGI-infrastructure barbell with a **Tier-D moonshot sleeve of LEAPS call
options + config-pinned micro-caps** on top of a large-cap stock floor: **24
positions across 4 tiers** (A Floor 50% · B Asymmetric 32.5% · D Moonshot 12.5% ·
C Cash 5%). Floor ≥$80K by month 36; uncapped tail via the LEAPS. NO crypto.

It **trades both stocks and single-leg long-call options** — the only sleeve in
the family that does options buying here (no spreads, no covered calls). Module
layout mirrors the sibling Rust sleeves.

Capital-isolated paper sleeve: its own Alpaca **paper** account
(`PKL3NILMKRLVXSVV3FAWKW3AIO`, options level 3), its own gdrive vault
(`~/gdrive/vault/aschenbrenner-unlimited`), its own Telegram bot, its own config.

**Sibling sleeve:** `../aschenbrenner_portfolio` runs the stocks-only
*Floor 2x / Ceiling 10x* (floor2x_10x) strategy on a different account. The two
run independently and share only the `daily_analysis` vault + ticker universe +
contract fixtures.

## Layout (delta vs the stocks-only sibling)

```
src/
  portfolio/targets.rs   Instrument enum {Stock, Leaps{underlying,expiry_year,strike},
                         MicroCap{slot}} + 24-position 4-tier model. equity_invested()
                         (Tier A/B, carries caps) vs moonshot() (Tier D, exempt).
                         instrument_symbol(t,cfg) resolves stock ticker / OCC / pinned.
  core/alpaca.rs         + submit_option_order (single-leg buy_to_open/sell_to_close)
  core/alpaca_data.rs    + build_occ/parse_occ + third_friday + get_option_mid
  rebalance/rules.rs     tier-aware stop (Floor only) + profit-takes (A 3x/25%, B 5x/30%,
                         LEAPS +500%/half + ≤60-DTE time-stop flag, micro 10x/50%);
                         caps + deploy on the EQUITY sleeve only
  rebalance/build.rs     4-stage phasing: Floor → Asymmetric(DCA) → MoonLeaps → MoonMicro
  execute.rs             branches Stock vs LEAPS (contracts ×100, option mid + wider buffer)
```

## Hard invariants (safety — never weaken)

- **Paper-only.** `safety_check` (kill-switch + PA guard) before any order path;
  `verify_paper_account` re-checks inside every stock AND option submit.
- **No market orders.** Marketable LIMIT only (`STOCK_BUF` / `OPTION_BUF`).
- **No leverage.** Total buy notional (stocks + LEAPS contracts×mid×100) capped at
  cash − buffer (`safety::cash_available_for_buys`, enforced in `execute.rs`).
- **Options = single-leg long calls only.** No spreads, no covered calls.
- **Deterministic floor.** `rebalance::rules` is the floor; `signal_bias` only
  re-orders equity choices. Tier-D lifecycle is rule-driven, not signal-driven.
- **File vault, no DB.** `ALPACA_BASE_URL` is the paper host, no `/v2`. Secrets in
  gitignored `.env` + `alpaca.txt`.

## Execution model

`build` / `daily`(`track`) / `rebalance` / `scheduler` are **ARMED by default**
(submit); pass `dry-run` for compute-and-log. The scheduler's daily window runs
the BUILD cycle, which self-promotes to maintenance once the book is within band.

## Run / test (from `~/projects`)

```bash
cargo build -p aschenbrenner-unlimited
cargo test  -p aschenbrenner-unlimited
./target/debug/aschenbrenner-unlimited test            # offline sanity
./target/debug/aschenbrenner-unlimited status          # PA guard + balances
./target/debug/aschenbrenner-unlimited positions       # equity drift + clusters
./target/debug/aschenbrenner-unlimited build dry-run   # phased tranche (LEAPS show resolved OCC)
```

Release binary: `~/projects/target/release/aschenbrenner-unlimited`. Production
runs via `deploy/aschenbrenner-unlimited.service` (systemd user unit, ARMED).

## Sibling-repo sync obligations (lockstep)

- **Daily-analysis contract.** Vendors byte-identical fixtures under
  `tests/fixtures/`, registered in
  `../trading-agents-scheduler/scripts/check_contract_fixtures.sh`.
- **Ticker universe.** All equity names + LEAPS underlyings (NVDA/CRWV/IREN/OKLO)
  + the pinned micro-caps must be in `../trading-agents-scheduler/tickers.txt`.
- **Infra patterns** mirror the sibling sleeves by hand — port fixes both ways.

## Domain authority

`src/portfolio/targets.rs` is authoritative; `portfolio.md` is its mirror. A
target/rule change updates **both** and re-runs tests (weights normalize to
`1 - cash_buffer_pct`; equity clusters ≤ cap at target; LEAPS resolve to the
expected OCC; micro-cap pins resolve).
