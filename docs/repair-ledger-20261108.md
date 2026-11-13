# Repair Ledger — 2026-11-08

Baseline: `aa705ea` → `5b5f4a7`

This ledger is the compact, repository-tracked index of reviewed findings and their disposition. Detailed evidence remains in the workspace review report.
Current HEAD: `e4150b411727a99c63936eb90e1a9fc3451ea169` (1.0.13 single-file); latest verification: core 823 / server 15 / Tauri 34 / frontend 82; test CI `42572822638`.

## Fixed

| ID | Issue | Commit | Verification |
|---|---|---|---|
| ACCOUNT_LOAD_CORRUPT | Corrupt/unreadable accounts file treated as empty on write | `e4150b411727a99c63936eb90e1a9fc3451ea169` | `corrupt_accounts_file_is_not_treated_as_empty_for_updates`; core suite |
| DEDUP_DELETE_BODY | Dedup physically removed JSONL despite soft-delete UI promise | `e4150b411727a99c63936eb90e1a9fc3451ea169` | soft-delete only; command checks client not running |
| TOKEN_DETAIL_LOOP | Detail retry could re-fetch indefinitely after success | `e4150b411727a99c63936eb90e1a9fc3451ea169` | retry marker reset after success |
| IDE_LINK_CHANNEL | CodeBuddy IDE list/copy channel unavailable | `e4150b411727a99c63936eb90e1a9fc3451ea169` | backend IDE store list + frontend client routing |
| PINNED_CORRUPT | Corrupt pinned JSON replaced with empty object | `e4150b411727a99c63936eb90e1a9fc3451ea169` | parse failure returns skipped, no write |
| PINNED_STALE_MEMBERS | Pinned mapping included stale/superseded members | `e4150b411727a99c63936eb90e1a9fc3451ea169` | active-only + dedup |
| JETBRAINS_FAIL_OPEN | Unmappable running IDE allowed credential write | `e4150b411727a99c63936eb90e1a9fc3451ea169` | unknown config dir blocks switch |
| JETBRAINS_PRODUCTS | RustRover missing; PyCharm CE label mismatch | `e4150b411727a99c63936eb90e1a9fc3451ea169` | product table + longest-prefix order |
| DB_COPY_SNAPSHOT | Copy backup copied DB/WAL/SHM sequentially | `e4150b411727a99c63936eb90e1a9fc3451ea169` | SQLite Backup API; snapshot opens and row count verified |
| TOKEN_IDE_ATTRIBUTION | IDE requests attributed to lead copy's account | `e4150b411727a99c63936eb90e1a9fc3451ea169` | per-member account attribution |
| LIGHTWEIGHT_ACTIVE_ONLY | Lightweight check sized all historical sessions | `e4150b411727a99c63936eb90e1a9fc3451ea169` | `wanted` active-member set |
| LIGHTWEIGHT_UNKNOWN | Unreadable directory counted as 0 bytes | `e4150b411727a99c63936eb90e1a9fc3451ea169` | `None` → `u64::MAX` (conservative degrade) |
| EQUIVALENT_NOT_LINKED | Equivalent target reported `AlreadyLinked` without membership | `e4150b411727a99c63936eb90e1a9fc3451ea169` | reuses target UUID and commits members/baseline |
| TOKEN_SCAN_PERF | Full JSON parse + large files bypassing cache | `e4150b411727a99c63936eb90e1a9fc3451ea169` | `read_until`/`from_slice`, metadata prefilter, record-based cache, frontend 30s single-flight |
| UI_RECENT_USED | Recent-used order stale after CLI/IDE switch | `e4150b411727a99c63936eb90e1a9fc3451ea169` | refresh account store on success |
| TOKEN_SINGLE_DATE | Single-account trend lacked x-axis date | `e4150b411727a99c63936eb90e1a9fc3451ea169` | `date: String(point.key)` |
| SESSIONS_ACCOUNT_STATE | Account load failure shown as empty account library | `e4150b411727a99c63936eb90e1a9fc3451ea169` | loading/error/retry states |
| LEGACY_OKLCH_VARS | Theme variables had no legacy fallback | `e4150b411727a99c63936eb90e1a9fc3451ea169` | `@supports not (color: oklch(...))` + `check-legacy-colors.py` |
| LEGACY_COLOR_MIX | Hand-written relationship graph color-mix had no fallback | `e4150b411727a99c63936eb90e1a9fc3451ea169` | solid/transparent fallback + `@supports` override |
| MEMORY_UX | Missing exit hint; “written” shown when unchanged; candidate count | `e4150b411727a99c63936eb90e1a9fc3451ea169` | dialog copy and valid-UID gate |
| TOKEN_SCAN_CACHE_LRU | Large token files bypassed file cache | `e4150b411727a99c63936eb90e1a9fc3451ea169` | 100k records / 32MB per-file / 128MB total LRU |
| TOKEN_TIMESTAMP_INDEX | mtime cannot represent record age | `e4150b411727a99c63936eb90e1a9fc3451ea169` + `e4150b411727a99c63936eb90e1a9fc3451ea169` | record max timestamp index; Windows replace; test isolation |
| TOKEN_RANGE_UI | UI range was local-only | `e4150b411727a99c63936eb90e1a9fc3451ea169` + `e4150b411727a99c63936eb90e1a9fc3451ea169` | range passed to backend; labeled “统计范围” |
| PORTABILITY_GATE | Open-source cross-platform assumptions not enforced | `e4150b411727a99c63936eb90e1a9fc3451ea169` | `check-portability.py` + CONTRIBUTING rules |
| CODEQL_ALL_LANGUAGES | CodeQL JS/rust/actions init intermittently failed | `e4150b411727a99c63936eb90e1a9fc3451ea169` | run `42482716521`: Python/JS/Rust/Actions all success |
| TAURI_CSP | CSP was null | `e4150b411727a99c63936eb90e1a9fc3451ea169` | default-src self + IPC; Tauri hash injection; 8s launch alive |
| TOKEN_UI_RANGE_SEMANTICS | Range changed total semantics silently | `e4150b411727a99c63936eb90e1a9fc3451ea169` | non-all range label shows 范围内; cache hint |
| TOKEN_SCAN_COUNTS | Skipped files counted as parsed | `e4150b411727a99c63936eb90e1a9fc3451ea169` | filesParsed/filesSkipped separated |
| TOKEN_INDEX_REPLACE_SAFETY | Windows index replace could lose cache | `e4150b411727a99c63936eb90e1a9fc3451ea169` | .bak + rollback rename; LRU order cleanup |
| TOKEN_CACHE_BUDGET | Fixed 128MB budget unsuitable for low-memory machines | `e4150b411727a99c63936eb90e1a9fc3451ea169` | `WB_TOKEN_CACHE_MB` 16–512 |
| TOKEN_DETAIL_ISOLATION | Detail request rescanned all sources | `e4150b411727a99c63936eb90e1a9fc3451ea169` | `/api/token-stats/requests` scans CLI only |
| TOKEN_TODAY_RANGE | days=1 fell back to all-time | `e4150b411727a99c63936eb90e1a9fc3451ea169` | backend accepts 1/7/30/90 |
| TOKEN_SOURCE_CACHE_ERROR_TTL | One bad line disabled source cache | `e4150b411727a99c63936eb90e1a9fc3451ea169` | error results cached for 60s only |
| TOKEN_SCAN_PROFILING | No per-source timing diagnostics | `e4150b411727a99c63936eb90e1a9fc3451ea169` | `WB_TOKEN_PROFILE=1` files/parsed/skipped/errors/elapsed |
| TOKEN_RANGE_LABEL | days=1 labeled “today” but is rolling 24h | `e4150b411727a99c63936eb90e1a9fc3451ea169` | label changed to 近24小时 |
| TOKEN_BAD_LINE_INDEX | Files with safe non-metadata bad lines never indexed | `e4150b411727a99c63936eb90e1a9fc3451ea169` | unsafe metadata errors block index; plain bad lines do not |
| SOURCE_CACHE_ORDER | Replacing a source cache key left stale LRU order | `e4150b411727a99c63936eb90e1a9fc3451ea169` | old order entry removed before reinsert |
| SYNC_PROFILE | Serial sync had no timing diagnostics | `e4150b411727a99c63936eb90e1a9fc3451ea169` | `WB_SYNC_PROFILE=1` item/total elapsed |
| DEMO_TOKEN_REQUESTS | Demo mode missing get_token_requests branch | `e4150b411727a99c63936eb90e1a9fc3451ea169` | detail command returns demo rows; regression test |
| DEMO_JETBRAINS_STATUS | Demo mode missing get_jetbrains_status branch | `e4150b411727a99c63936eb90e1a9fc3451ea169` | status payload + regression test |
| DEMO_COMMAND_GATE | No guard against demo command/branch drift | `e4150b411727a99c63936eb90e1a9fc3451ea169` | `check-demo-commands.py` in quality-gates; 37/37 |
| SNAPSHOT_24H | 24h active-session compression not executed | `e4150b411727a99c63936eb90e1a9fc3451ea169` | manifest 1601 files: 1424 compacted, 177 skipped-recent, 0 failed; 29,009,817,510 -> 18,721,280,532 bytes, saved 10,288,536,978; SHA256 all match |
| TOKEN_PATH_PRIVACY | Encoded Windows absolute path leaked as project label | `e4150b411727a99c63936eb90e1a9fc3451ea169` | drive/Users/home encoded paths fall back to 未知项目; real response test |
| TOKEN_SOURCE_CACHE_INIT | SOURCE_CACHE never initialized, so cache/TTL/LRU were dead code | `e4150b411727a99c63936eb90e1a9fc3451ea169` | `source_cache()` get_or_init; cache-poisoning test |
| TOKEN_REFRESH_BYPASS | Manual refresh could not bypass backend source cache | `e4150b411727a99c63936eb90e1a9fc3451ea169` | `refresh=true` bypass; UI passes it; test |
| TOKEN_ATTRIBUTION_NOTE | UI implied all unlinked usage was safely unknown | `e4150b411727a99c63936eb90e1a9fc3451ea169` | copy now states heuristic/linked distinction |
| WEBUI_TOKEN_REDIRECT | Startup token stayed in URL after cookie exchange | `e4150b411727a99c63936eb90e1a9fc3451ea169` | 302 to clean URL + no-store + tests |
| TOKEN_REFRESH_CACHE_KEY | Refresh reload created unbounded frontend cache entries | `e4150b411727a99c63936eb90e1a9fc3451ea169` | cache key stable; inflight key includes reload |
| TOKEN_FIRST_SCAN_HINT | First scan can take minutes with no elapsed e4150b411727a99c63936eb90e1a9fc3451ea169k | `e4150b411727a99c63936eb90e1a9fc3451ea169` | after 3s show elapsed + first-scan hint |
| TOKEN_PARALLEL_SCAN | First scan parsed files strictly sequentially | `e4150b411727a99c63936eb90e1a9fc3451ea169` | bounded 4-thread parse, ordered aggregation; 278.97s→103.79s real data |

## Remaining

| ID | Issue | Status / next step |
|---|---|---|
| TOKEN_JSONL_ATTRIBUTION | JSONL history attributed by mtime-first file | partially fixed: linked groups use earliest active canonical account (e4150b411727a99c63936eb90e1a9fc3451ea169); unlinked copies still heuristic |
| SYNC_DB_IDEMPOTENCY | Recovery infers DB applied from millisecond timestamp | design review needed: add explicit journal marker/full-row postcondition; no reproducible failure yet |
| IDE_REAL_E2E | CodeBuddy IDE channel not tested on real IDE matrix | install-matrix verification |
| RELEASE_SIGNING | No updater private key / latest.json / authorization | blocked on owner |

## Verification baseline

- core 823 passed / 7 ignored
- server 15
- frontend 82
- tsc, clippy, Tauri check and release build pass
- test CI `42572822638` success; CodeQL four languages green
