# Swarm report

Written by `cargo xtask sim report` from the run log of `cargo xtask sim run`. The method is in
[`system-simulation.md`](system-simulation.md).

Runs: 2122 (2122 passed, 0 failed, 0 with losses expected under the fsync lie).

## Dimensions

| Dimension | Kind | Runs with it on |
|---|---|---|
| `json` | workload | 1117 |
| `large_payloads` | workload | 1340 |
| `deep_nesting` | workload | 1081 |
| `bad_unicode` | workload | 1048 |
| `rootless` | workload | 1082 |
| `concurrent_reads` | workload | 1096 |
| `client_retries` | workload | 1349 |
| `multi_project` | workload | 1056 |
| `bursts` | workload | 1035 |
| `multi_day` | workload | 1092 |
| `disk_crash_points` | fault | 1065 |
| `barrier_crashes` | fault | 1055 |
| `crash_loops` | fault | 1001 |
| `crash_in_recovery` | fault | 1357 |
| `clean_stops` | fault | 1090 |
| `disk_write_errors` | fault | 1098 |
| `disk_read_faults` | fault | 1013 |
| `disk_sync_errors` | fault | 1037 |
| `disk_full` | fault | 1090 |
| `slow_sync` | fault | 1075 |
| `ch_timeouts` | fault | 1030 |
| `ch_ack_lost` | fault | 1036 |
| `ch_server_errors` | fault | 1027 |
| `ch_rejects` | fault | 1006 |
| `ch_hang_overload` | fault | 1023 |
| `ch_poison` | fault | 1102 |
| `ch_probe_errors` | fault | 1119 |
| `ch_read_errors` | fault | 1105 |
| `window_expiry` | fault | 1054 |
| `outages` | fault | 1019 |
| `long_outages` | fault | 1126 |
| `wall_clock_steps` | fault | 1118 |
| `wal_compression_off` | config | 1324 |
| `small_batches` | config | 801 |
| `week_partitions` | config | 1034 |
| `tiny_recent` | config | 1012 |
| `small_segments` | config | 1291 |
| `clustered` | config | 1118 |
| `one_connection` | config | 1009 |
| `replay_retention` | config | 775 |
| `fsync_lie` | fault | 1058 |
| `facts` | workload | 1031 |
| `frame_faults` | fault | 1140 |
| `project_rate` | config | 1070 |
| `small_budget` | config | 1026 |
| `nested_usage` | workload | 1086 |
| `mcp` | workload | 1005 |
| `mcp_double_capture` | workload | 1038 |
| `mcp_errors` | workload | 1113 |
| `mcp_mrtr` | workload | 969 |
| `mcp_inventory` | workload | 1056 |
| `mcp_deploys` | workload | 1032 |
| `mcp_content` | workload | 1053 |
| `mcp_identity` | workload | 1062 |
| `dir_sync_errors` | fault | 966 |

## Targets

A trigger raises the rate of a target and a suppressor lowers it; a dimension is listed when the 95% Wilson
intervals of the two rates do not overlap. The rates are `on` and `off` in percent.
Each cell is its own test, so of the roughly thousand dimension and target pairs a few dozen are listed by
chance; a pair that matters shows a large difference and returns in the next log.

| Target | Runs | Triggers | Suppressors |
|---|---|---|---|
| crash at before checkpoint.appended | 20 | `barrier_crashes` (2 / 0) | `ch_poison` (0 / 2) |
| crash at before insert.sent | 153 | `barrier_crashes` (15 / 0) | none |
| crash at before seal.appended | 47 | `barrier_crashes` (4 / 0) | none |
| crash at checkpoint.appended | 13 | `barrier_crashes` (1 / 0) | none |
| crash at insert.sent | 150 | `barrier_crashes` (14 / 0) | none |
| crash at quarantine.copied | 34 | `barrier_crashes` (3 / 0), `ch_poison` (3 / 0) | `disk_crash_points` (1 / 2), `ch_timeouts` (1 / 2), `long_outages` (1 / 3) |
| crash at seal.appended | 50 | `barrier_crashes` (5 / 0) | `ch_poison` (1 / 3) |
| crash during wal recovery | 1453 | `large_payloads` (75 / 57), `client_retries` (74 / 59), `crash_loops` (74 / 63), `crash_in_recovery` (97 / 18), `disk_read_faults` (73 / 65), `ch_rejects` (72 / 65), `wal_compression_off` (75 / 58), `small_segments` (77 / 56), `frame_faults` (73 / 63), `mcp_double_capture` (71 / 66) | `disk_crash_points` (64 / 73), `small_batches` (62 / 72), `tiny_recent` (63 / 74), `replay_retention` (56 / 76), `mcp` (65 / 71) |
| deduplication entry expired early | 714 | `window_expiry` (68 / 0), `small_batches` (38 / 31), `mcp_double_capture` (37 / 31) | `crash_loops` (24 / 42), `long_outages` (29 / 39) |
| directory syncs failed | 877 | `crash_loops` (46 / 38), `disk_full` (44 / 38), `small_segments` (44 / 37), `nested_usage` (44 / 38), `mcp_content` (45 / 37), `dir_sync_errors` (91 / 0) | `barrier_crashes` (38 / 44), `long_outages` (37 / 46), `wall_clock_steps` (37 / 47), `mcp_deploys` (38 / 44) |
| fact write acknowledged | 978 | `multi_day` (49 / 43), `ch_poison` (50 / 42), `ch_read_errors` (49 / 43), `facts` (95 / 0) | `window_expiry` (43 / 50), `long_outages` (43 / 50), `small_batches` (42 / 49) |
| fact write refused | 217 | `large_payloads` (13 / 5), `client_retries` (13 / 5), `crash_in_recovery` (12 / 7), `disk_read_faults` (12 / 8), `disk_sync_errors` (13 / 8), `wal_compression_off` (14 / 5), `small_segments` (15 / 3), `facts` (21 / 0) | `deep_nesting` (8 / 13), `small_batches` (6 / 13), `replay_retention` (7 / 12) |
| facts resolved against the reference fold | 987 | `multi_day` (50 / 43), `ch_poison` (51 / 42), `facts` (96 / 0) | `window_expiry` (43 / 50), `long_outages` (43 / 50), `small_batches` (42 / 49) |
| frame quarantined | 561 | `clustered` (29 / 23), `facts` (29 / 24), `frame_faults` (49 / 0) | `deep_nesting` (22 / 32), `bad_unicode` (22 / 30) |
| frame waited for its project salt | 114 | `multi_project` (7 / 4), `mcp` (11 / 0), `mcp_identity` (11 / 0) | none |
| insert deduplicated | 543 | `ch_timeouts` (46 / 7) | `crash_loops` (17 / 33), `ch_hang_overload` (21 / 30), `outages` (20 / 31), `long_outages` (21 / 30), `small_segments` (23 / 30) |
| insert landed late | 794 | `ch_timeouts` (56 / 20), `ch_poison` (46 / 28), `small_batches` (42 / 34) | `crash_loops` (26 / 48), `disk_write_errors` (35 / 41), `outages` (32 / 42), `long_outages` (31 / 45), `small_segments` (34 / 42), `mcp_inventory` (34 / 40) |
| json request acknowledged | 944 | `json` (85 / 0), `ch_timeouts` (50 / 39), `window_expiry` (48 / 41), `small_budget` (48 / 41) | `deep_nesting` (37 / 53), `ch_rejects` (40 / 48) |
| mcp aggregates compared with the reference | 767 | `mcp` (76 / 0), `mcp_inventory` (41 / 32) | `ch_rejects` (31 / 41), `ch_poison` (29 / 43) |
| mcp caller fault | 362 | `concurrent_reads` (20 / 14), `mcp` (36 / 0), `mcp_errors` (33 / 0) | `deep_nesting` (13 / 21), `week_partitions` (14 / 20) |
| mcp operation captured by a proxy | 849 | `mcp` (84 / 0), `mcp_double_capture` (46 / 34), `mcp_inventory` (44 / 36) | `deep_nesting` (36 / 44), `crash_loops` (36 / 44), `ch_rejects` (35 / 45) |
| mcp operation classified as an error | 495 | `concurrent_reads` (26 / 21), `mcp` (49 / 0), `mcp_errors` (44 / 0), `mcp_inventory` (26 / 21), `mcp_deploys` (26 / 21) | `deep_nesting` (20 / 27), `disk_read_faults` (20 / 26) |
| mcp operation compared with its exchange | 957 | `concurrent_reads` (48 / 42), `multi_project` (48 / 42), `mcp` (95 / 0), `mcp_inventory` (50 / 41) | `crash_loops` (41 / 49), `ch_rejects` (39 / 51) |
| mcp user subject hashed | 463 | `multi_project` (25 / 18), `outages` (25 / 19), `tiny_recent` (25 / 19), `mcp` (46 / 0), `mcp_identity` (44 / 0) | `crash_loops` (19 / 24), `disk_sync_errors` (19 / 24), `ch_rejects` (19 / 25), `frame_faults` (19 / 25) |
| parts merged in the store | 1792 | `small_batches` (93 / 79) | `large_payloads` (82 / 88), `deep_nesting` (82 / 87), `bad_unicode` (81 / 88), `barrier_crashes` (82 / 87), `crash_loops` (78 / 90), `crash_in_recovery` (83 / 88), `wal_compression_off` (82 / 88) |
| probe answer expired before its send | 141 | `outages` (11 / 3), `mcp` (8 / 5) | `long_outages` (4 / 10) |
| probe read present | 1289 | `ch_ack_lost` (68 / 54), `small_batches` (71 / 54) | `deep_nesting` (57 / 65), `bad_unicode` (57 / 64), `crash_loops` (45 / 75), `outages` (58 / 64), `long_outages` (51 / 71), `mcp_mrtr` (57 / 64) |
| process stop | 970 | `multi_project` (49 / 42), `crash_loops` (49 / 43), `clean_stops` (89 / 0), `frame_faults` (49 / 42), `nested_usage` (49 / 43), `mcp_deploys` (50 / 42) | `large_payloads` (43 / 50), `disk_sync_errors` (42 / 49), `small_segments` (43 / 50) |
| quarantine | 791 | `ch_ack_lost` (40 / 34), `ch_poison` (50 / 24), `facts` (41 / 33), `frame_faults` (56 / 15) | `deep_nesting` (32 / 43), `bad_unicode` (33 / 41), `crash_loops` (33 / 41), `long_outages` (33 / 42), `small_segments` (35 / 42) |
| read answered lagging | 938 | `concurrent_reads` (62 / 25), `ch_timeouts` (48 / 41), `small_batches` (64 / 32), `small_budget` (50 / 38) | `large_payloads` (41 / 50), `deep_nesting` (39 / 49), `bad_unicode` (39 / 49), `client_retries` (41 / 50), `barrier_crashes` (39 / 49), `crash_loops` (38 / 50), `crash_in_recovery` (41 / 50), `wal_compression_off` (42 / 48), `small_segments` (40 / 50), `frame_faults` (41 / 47) |
| read answered partial (quarantined) | 728 | `ch_poison` (44 / 24), `facts` (39 / 30), `frame_faults` (54 / 11) | `deep_nesting` (29 / 40), `bad_unicode` (30 / 39), `crash_loops` (30 / 38), `long_outages` (31 / 38) |
| read answered partial (store_unavailable) | 1564 | `concurrent_reads` (78 / 69), `ch_read_errors` (84 / 63), `outages` (77 / 71), `long_outages` (80 / 67), `tiny_recent` (80 / 68) | `rootless` (71 / 77), `crash_loops` (69 / 78), `disk_full` (71 / 77), `small_batches` (69 / 77), `clustered` (71 / 77) |
| read refused (store unavailable) | 1064 | `concurrent_reads` (63 / 37), `ch_read_errors` (56 / 43), `outages` (56 / 45), `long_outages` (57 / 42), `small_batches` (58 / 45), `tiny_recent` (59 / 42), `mcp` (55 / 46) | `deep_nesting` (47 / 53), `barrier_crashes` (46 / 54), `clustered` (47 / 53) |
| repeated request acknowledged | 976 | `large_payloads` (50 / 39), `client_retries` (72 / 0), `bursts` (51 / 41), `crash_in_recovery` (50 / 39), `disk_sync_errors` (51 / 42), `ch_ack_lost` (50 / 42), `wal_compression_off` (51 / 38), `small_segments` (49 / 41) | `deep_nesting` (41 / 51), `small_batches` (37 / 52), `replay_retention` (37 / 51), `dir_sync_errors` (43 / 49) |
| request refused | 1847 | `large_payloads` (92 / 79), `deep_nesting` (100 / 74), `bad_unicode` (99 / 75), `client_retries` (90 / 83), `crash_in_recovery` (90 / 83), `ch_server_errors` (89 / 85), `wal_compression_off` (90 / 82), `small_segments` (91 / 80), `fsync_lie` (89 / 85), `dir_sync_errors` (89 / 85) | `multi_day` (84 / 90), `small_batches` (81 / 90), `replay_retention` (84 / 89), `mcp_errors` (85 / 89), `mcp_deploys` (84 / 90) |
| request throttled (429) | 89 | `json` (5 / 3), `large_payloads` (6 / 1), `bad_unicode` (6 / 3), `bursts` (6 / 2), `project_rate` (8 / 0) | `multi_project` (2 / 7) |
| request with a repeated span acknowledged | 700 | `client_retries` (52 / 0), `bursts` (39 / 27), `ch_rejects` (36 / 30) | `deep_nesting` (25 / 41), `multi_project` (29 / 37), `crash_loops` (29 / 36), `disk_full` (28 / 38), `small_batches` (27 / 36), `replay_retention` (29 / 35), `nested_usage` (28 / 38), `dir_sync_errors` (30 / 36) |
| rollups compared with the reference | 1720 | `large_payloads` (83 / 77), `bursts` (85 / 77), `crash_loops` (86 / 77), `ch_hang_overload` (84 / 78), `long_outages` (85 / 77), `small_segments` (83 / 78) | `ch_ack_lost` (78 / 84), `ch_poison` (66 / 98), `small_batches` (78 / 83), `facts` (78 / 84), `nested_usage` (78 / 84) |
| sealed batch shipped again after a restart | 1769 | `ch_hang_overload` (86 / 81), `long_outages` (87 / 79), `small_batches` (94 / 77) | `large_payloads` (81 / 87), `crash_loops` (80 / 87), `small_segments` (81 / 86) |
| shipper quarantined a frame | 561 | `clustered` (29 / 23), `facts` (29 / 24), `frame_faults` (49 / 0) | `deep_nesting` (22 / 32), `bad_unicode` (22 / 30) |
| span re-read through its locator | 1758 | `concurrent_reads` (87 / 79), `long_outages` (86 / 80) | `crash_loops` (74 / 91), `small_batches` (77 / 86), `replay_retention` (79 / 85) |
| span read from the recent index | 1759 | `concurrent_reads` (87 / 79), `long_outages` (86 / 80) | `crash_loops` (74 / 91), `small_batches` (77 / 86), `replay_retention` (79 / 85) |
| span read from the store | 2066 | `bursts` (99 / 96) | `deep_nesting` (96 / 99), `bad_unicode` (96 / 99), `crash_loops` (96 / 99), `slow_sync` (96 / 99) |
| store outage | 1506 | `deep_nesting` (75 / 67), `crash_loops` (74 / 68), `ch_ack_lost` (74 / 68), `outages` (94 / 50), `long_outages` (93 / 47), `mcp_inventory` (74 / 68) | `replay_retention` (67 / 73), `frame_faults` (67 / 75), `dir_sync_errors` (67 / 74) |
| store read fault | 1722 | `concurrent_reads` (84 / 78), `ch_read_errors` (95 / 66), `outages` (91 / 72), `long_outages` (91 / 70) | `disk_full` (78 / 84), `clustered` (78 / 84), `frame_faults` (78 / 84) |
