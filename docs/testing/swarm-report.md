# Swarm report

Written by `cargo xtask sim report` from the run log of `cargo xtask sim run`. The method is in
[`system-simulation.md`](system-simulation.md).

Runs: 600 (600 passed, 0 failed, 0 with losses expected under the fsync lie).

## Dimensions

| Dimension | Kind | Runs with it on |
|---|---|---|
| `json` | workload | 336 |
| `large_payloads` | workload | 282 |
| `deep_nesting` | workload | 351 |
| `bad_unicode` | workload | 279 |
| `rootless` | workload | 294 |
| `concurrent_reads` | workload | 270 |
| `client_retries` | workload | 279 |
| `multi_project` | workload | 330 |
| `bursts` | workload | 321 |
| `multi_day` | workload | 312 |
| `disk_crash_points` | fault | 273 |
| `barrier_crashes` | fault | 285 |
| `crash_loops` | fault | 315 |
| `crash_in_recovery` | fault | 300 |
| `clean_stops` | fault | 309 |
| `disk_write_errors` | fault | 276 |
| `disk_read_faults` | fault | 294 |
| `disk_sync_errors` | fault | 279 |
| `disk_full` | fault | 303 |
| `slow_sync` | fault | 345 |
| `ch_timeouts` | fault | 264 |
| `ch_ack_lost` | fault | 273 |
| `ch_server_errors` | fault | 288 |
| `ch_rejects` | fault | 336 |
| `ch_hang_overload` | fault | 261 |
| `ch_poison` | fault | 315 |
| `ch_probe_errors` | fault | 273 |
| `ch_read_errors` | fault | 309 |
| `window_expiry` | fault | 279 |
| `outages` | fault | 333 |
| `long_outages` | fault | 312 |
| `wall_clock_steps` | fault | 294 |
| `wal_compression_off` | config | 348 |
| `small_batches` | config | 303 |
| `week_partitions` | config | 291 |
| `tiny_recent` | config | 321 |
| `small_segments` | config | 288 |
| `clustered` | config | 315 |
| `one_connection` | config | 258 |
| `replay_retention` | config | 306 |
| `fsync_lie` | fault | 288 |

## Targets

A trigger raises the rate of a target and a suppressor lowers it; a dimension is listed when the 95% Wilson
intervals of the two rates do not overlap. The rates are `on` and `off` in percent.
Each cell is its own test, so of the roughly thousand dimension and target pairs a few dozen are listed by
chance; a pair that matters shows a large difference and returns in the next log.

| Target | Runs | Triggers | Suppressors |
|---|---|---|---|
| crash at before checkpoint.appended | 9 | `barrier_crashes` (3 / 0) | none |
| crash at before insert.sent | 37 | `barrier_crashes` (13 / 0) | none |
| crash at before seal.appended | 18 | `json` (5 / 1), `barrier_crashes` (6 / 0), `small_batches` (5 / 1) | none |
| crash at checkpoint.appended | 5 | none | none |
| crash at insert.sent | 39 | `barrier_crashes` (14 / 0), `wal_compression_off` (9 / 3) | none |
| crash at quarantine.copied | 8 | `barrier_crashes` (3 / 0) | none |
| crash at seal.appended | 17 | `barrier_crashes` (6 / 0) | none |
| crash during wal recovery | 337 | `crash_in_recovery` (98 / 14), `disk_read_faults` (64 / 49) | none |
| deduplication entry expired early | 177 | `json` (36 / 21), `window_expiry` (63 / 0), `wal_compression_off` (34 / 23) | `deep_nesting` (24 / 37), `crash_loops` (19 / 41), `tiny_recent` (23 / 37) |
| insert deduplicated | 131 | `ch_timeouts` (44 / 4), `small_batches` (28 / 16) | `long_outages` (15 / 29), `wall_clock_steps` (14 / 29) |
| insert landed late | 190 | `disk_write_errors` (38 / 27), `slow_sync` (37 / 24), `ch_timeouts` (51 / 16), `ch_poison` (42 / 20), `small_batches` (40 / 23) | `crash_loops` (24 / 40), `long_outages` (21 / 43) |
| json request acknowledged | 292 | `json` (87 / 0), `ch_rejects` (55 / 40), `ch_hang_overload` (57 / 42), `window_expiry` (58 / 41), `outages` (54 / 42), `wal_compression_off` (54 / 41) | `deep_nesting` (42 / 59), `wall_clock_steps` (41 / 56) |
| parts merged in the store | 444 | `small_batches` (81 / 67) | `long_outages` (68 / 80) |
| probe answer expired before its send | 26 | `outages` (8 / 0) | none |
| probe read present | 307 | `ch_ack_lost` (58 / 46), `small_batches` (65 / 37) | `deep_nesting` (46 / 58), `bad_unicode` (44 / 58), `crash_loops` (40 / 64), `long_outages` (38 / 66) |
| process stop | 277 | `json` (51 / 39), `clean_stops` (90 / 0), `ch_hang_overload` (53 / 41), `ch_probe_errors` (53 / 40) | `disk_sync_errors` (39 / 52), `clustered` (39 / 54) |
| quarantine | 98 | `disk_full` (21 / 11), `slow_sync` (20 / 11), `ch_poison` (31 / 0) | `deep_nesting` (12 / 22), `crash_loops` (8 / 25) |
| read answered lagging | 246 | `concurrent_reads` (66 / 21), `small_batches` (57 / 24), `week_partitions` (47 / 35) | `deep_nesting` (33 / 53), `bad_unicode` (34 / 47) |
| read answered partial (quarantined) | 86 | `ch_poison` (27 / 0) | `deep_nesting` (10 / 20), `crash_loops` (8 / 22) |
| read answered partial (store_unavailable) | 444 | `concurrent_reads` (81 / 68), `ch_server_errors` (80 / 68), `ch_read_errors` (81 / 66), `long_outages` (82 / 65) | `large_payloads` (68 / 80), `crash_in_recovery` (68 / 80), `small_batches` (68 / 80) |
| read refused (store unavailable) | 240 | `concurrent_reads` (57 / 26), `ch_read_errors` (48 / 32), `outages` (47 / 31), `long_outages` (49 / 30), `small_batches` (53 / 27) | `deep_nesting` (34 / 48), `crash_in_recovery` (34 / 46) |
| repeated request acknowledged | 223 | `client_retries` (80 / 0), `ch_timeouts` (44 / 32), `ch_rejects` (43 / 30) | `wall_clock_steps` (29 / 45) |
| request refused | 496 | `deep_nesting` (100 / 58), `bad_unicode` (100 / 68), `replay_retention` (88 / 77) | `concurrent_reads` (73 / 91), `ch_server_errors` (76 / 89), `window_expiry` (76 / 88) |
| sealed batch shipped again after a restart | 479 | `disk_crash_points` (85 / 75), `disk_write_errors` (86 / 75), `long_outages` (85 / 75), `small_batches` (94 / 66) | `replay_retention` (75 / 85) |
| span read from the recent index | 546 | none | `crash_loops` (86 / 96), `small_batches` (87 / 95) |
| span read from the store | 590 | none | none |
| store outage | 432 | `crash_loops` (77 / 66), `clean_stops` (77 / 67), `outages` (94 / 45), `long_outages` (95 / 47) | `disk_full` (67 / 77) |
| store read fault | 475 | `crash_loops` (84 / 74), `ch_server_errors` (84 / 74), `ch_read_errors` (93 / 64), `outages` (89 / 66), `long_outages` (92 / 65) | `replay_retention` (73 / 86) |
