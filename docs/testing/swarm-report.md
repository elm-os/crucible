# Swarm report

Written by `cargo xtask sim report` from the run log of `cargo xtask sim run`. The method is in
[`system-simulation.md`](system-simulation.md).

Runs: 1212 (1212 passed, 0 failed, 0 with losses expected under the fsync lie).

## Dimensions

| Dimension | Kind | Runs with it on |
|---|---|---|
| `json` | workload | 613 |
| `large_payloads` | workload | 588 |
| `deep_nesting` | workload | 635 |
| `bad_unicode` | workload | 660 |
| `rootless` | workload | 611 |
| `concurrent_reads` | workload | 620 |
| `client_retries` | workload | 649 |
| `multi_project` | workload | 574 |
| `bursts` | workload | 569 |
| `multi_day` | workload | 621 |
| `disk_crash_points` | fault | 582 |
| `barrier_crashes` | fault | 586 |
| `crash_loops` | fault | 639 |
| `crash_in_recovery` | fault | 605 |
| `clean_stops` | fault | 603 |
| `disk_write_errors` | fault | 594 |
| `disk_read_faults` | fault | 640 |
| `disk_sync_errors` | fault | 623 |
| `disk_full` | fault | 594 |
| `slow_sync` | fault | 585 |
| `ch_timeouts` | fault | 650 |
| `ch_ack_lost` | fault | 638 |
| `ch_server_errors` | fault | 603 |
| `ch_rejects` | fault | 670 |
| `ch_hang_overload` | fault | 587 |
| `ch_poison` | fault | 595 |
| `ch_probe_errors` | fault | 592 |
| `ch_read_errors` | fault | 653 |
| `window_expiry` | fault | 575 |
| `outages` | fault | 645 |
| `long_outages` | fault | 611 |
| `wall_clock_steps` | fault | 590 |
| `wal_compression_off` | config | 650 |
| `small_batches` | config | 619 |
| `week_partitions` | config | 659 |
| `tiny_recent` | config | 573 |
| `small_segments` | config | 615 |
| `clustered` | config | 594 |
| `one_connection` | config | 597 |
| `replay_retention` | config | 644 |
| `fsync_lie` | fault | 569 |

## Targets

A trigger raises the rate of a target and a suppressor lowers it; a dimension is listed when the 95% Wilson
intervals of the two rates do not overlap. The rates are `on` and `off` in percent.
Each cell is its own test, so of the roughly thousand dimension and target pairs a few dozen are listed by
chance; a pair that matters shows a large difference and returns in the next log.

| Target | Runs | Triggers | Suppressors |
|---|---|---|---|
| crash at before checkpoint.appended | 13 | `barrier_crashes` (2 / 0) | none |
| crash at before insert.sent | 90 | `barrier_crashes` (15 / 0) | none |
| crash at before seal.appended | 25 | `barrier_crashes` (4 / 0) | none |
| crash at checkpoint.appended | 22 | `barrier_crashes` (4 / 0) | none |
| crash at insert.sent | 81 | `barrier_crashes` (14 / 0) | none |
| crash at quarantine.copied | 20 | `barrier_crashes` (3 / 0), `ch_poison` (3 / 0) | none |
| crash at seal.appended | 32 | `barrier_crashes` (5 / 0) | none |
| crash during wal recovery | 716 | `crash_in_recovery` (98 / 21), `disk_read_faults` (65 / 53) | `disk_crash_points` (55 / 63) |
| deduplication entry expired early | 418 | `window_expiry` (73 / 0) | `barrier_crashes` (30 / 38), `crash_loops` (26 / 44), `long_outages` (25 / 44) |
| insert deduplicated | 323 | `concurrent_reads` (30 / 23), `multi_project` (31 / 23), `bursts` (31 / 23), `ch_timeouts` (48 / 2), `ch_ack_lost` (31 / 22), `ch_poison` (31 / 22), `small_batches` (31 / 23) | `crash_loops` (20 / 34), `long_outages` (21 / 33), `one_connection` (22 / 31) |
| insert landed late | 428 | `bursts` (40 / 31), `crash_in_recovery` (40 / 31), `ch_timeouts` (55 / 13), `ch_poison` (46 / 25), `small_batches` (41 / 30) | `crash_loops` (26 / 45), `long_outages` (29 / 42), `one_connection` (31 / 40) |
| json request acknowledged | 527 | `json` (86 / 0) | `deep_nesting` (36 / 52), `concurrent_reads` (39 / 49) |
| probe answer expired before its send | 60 | `outages` (9 / 0), `replay_retention` (7 / 3) | none |
| probe read present | 670 | `ch_ack_lost` (63 / 46), `small_batches` (66 / 44) | `crash_loops` (45 / 67), `long_outages` (45 / 66), `fsync_lie` (51 / 59) |
| process stop | 544 | `clean_stops` (90 / 0), `ch_server_errors` (51 / 39) | `ch_probe_errors` (41 / 49), `week_partitions` (41 / 50) |
| quarantine | 181 | `ch_poison` (30 / 0), `wal_compression_off` (18 / 12), `tiny_recent` (19 / 11) | `crash_loops` (11 / 20), `ch_hang_overload` (10 / 19), `long_outages` (10 / 20) |
| read answered lagging | 536 | `concurrent_reads` (64 / 24), `small_batches` (62 / 26) | `json` (39 / 50), `crash_loops` (40 / 49) |
| read answered partial (quarantined) | 148 | `bursts` (15 / 10), `multi_day` (15 / 9), `ch_poison` (25 / 0), `wal_compression_off` (15 / 9), `small_batches` (15 / 9), `tiny_recent` (16 / 9) | `crash_loops` (9 / 16), `ch_hang_overload` (8 / 16), `long_outages` (8 / 17) |
| read answered partial (store_unavailable) | 912 | `concurrent_reads` (81 / 69), `ch_rejects` (79 / 71), `ch_read_errors` (84 / 65), `outages` (80 / 69), `long_outages` (79 / 71), `tiny_recent` (80 / 71) | `crash_loops` (71 / 80), `small_batches` (69 / 82) |
| read refused (store unavailable) | 560 | `concurrent_reads` (59 / 33), `multi_project` (53 / 40), `ch_read_errors` (53 / 38), `outages` (56 / 35), `long_outages` (50 / 42), `small_batches` (59 / 33), `replay_retention` (51 / 41) | `clustered` (42 / 51) |
| repeated request acknowledged | 526 | `client_retries` (81 / 0), `ch_hang_overload` (48 / 39) | `deep_nesting` (35 / 53), `multi_project` (37 / 49), `disk_read_faults` (38 / 49), `ch_timeouts` (40 / 48), `ch_server_errors` (38 / 48) |
| request refused | 1034 | `deep_nesting` (100 / 69), `bad_unicode` (100 / 68), `barrier_crashes` (89 / 82), `slow_sync` (89 / 82), `wall_clock_steps` (88 / 82), `small_segments` (88 / 82) | `crash_loops` (81 / 90) |
| sealed batch shipped again after a restart | 976 | `barrier_crashes` (85 / 77), `outages` (84 / 77), `long_outages` (85 / 76), `small_batches` (93 / 67) | none |
| span read from the recent index | 1085 | none | `crash_loops` (82 / 98), `small_batches` (86 / 93) |
| span read from the store | 1197 | none | `bad_unicode` (98 / 100) |
| store outage | 858 | `multi_project` (76 / 66), `crash_loops` (76 / 65), `outages` (95 / 44), `long_outages` (93 / 48) | `ch_ack_lost` (65 / 77), `ch_read_errors` (67 / 75), `week_partitions` (66 / 77) |
