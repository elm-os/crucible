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
| crash at before checkpoint.appended | 19 | `barrier_crashes` (2 / 0) | `long_outages` (0 / 2) |
| crash at before insert.sent | 169 | `barrier_crashes` (16 / 0) | none |
| crash at before seal.appended | 44 | `barrier_crashes` (4 / 0) | none |
| crash at checkpoint.appended | 14 | `barrier_crashes` (1 / 0) | none |
| crash at insert.sent | 149 | `barrier_crashes` (14 / 0) | none |
| crash at quarantine.copied | 32 | `barrier_crashes` (3 / 0), `ch_poison` (3 / 0) | `disk_crash_points` (1 / 2), `ch_timeouts` (1 / 2), `long_outages` (1 / 2) |
| crash at seal.appended | 47 | `barrier_crashes` (4 / 0) | `disk_crash_points` (1 / 3) |
| crash during wal recovery | 1452 | `large_payloads` (75 / 57), `client_retries` (74 / 60), `crash_loops` (75 / 63), `crash_in_recovery` (97 / 18), `disk_read_faults` (73 / 65), `ch_rejects` (72 / 65), `ch_probe_errors` (71 / 65), `wal_compression_off` (75 / 58), `small_segments` (77 / 56), `frame_faults` (73 / 63), `mcp_double_capture` (71 / 65) | `disk_crash_points` (64 / 73), `small_batches` (61 / 73), `tiny_recent` (63 / 73), `replay_retention` (57 / 75) |
| deduplication entry expired early | 712 | `window_expiry` (68 / 0), `small_batches` (38 / 31), `mcp_double_capture` (37 / 30) | `crash_loops` (24 / 42), `long_outages` (29 / 39) |
| directory syncs failed | 878 | `crash_loops` (46 / 38), `small_segments` (44 / 37), `nested_usage` (44 / 38), `mcp_content` (45 / 37), `dir_sync_errors` (91 / 0) | `barrier_crashes` (38 / 44), `long_outages` (37 / 46), `wall_clock_steps` (37 / 47), `mcp_deploys` (38 / 44) |
| fact write acknowledged | 981 | `multi_day` (49 / 43), `ch_poison` (51 / 42), `facts` (95 / 0) | `window_expiry` (43 / 50), `long_outages` (43 / 50), `small_batches` (42 / 49) |
| fact write refused | 225 | `large_payloads` (13 / 6), `client_retries` (13 / 6), `crash_in_recovery` (13 / 7), `disk_write_errors` (13 / 8), `disk_read_faults` (13 / 8), `disk_sync_errors` (14 / 8), `wal_compression_off` (14 / 5), `small_segments` (15 / 3), `facts` (22 / 0) | `deep_nesting` (7 / 14), `long_outages` (9 / 13), `small_batches` (6 / 13), `replay_retention` (8 / 12) |
| facts resolved against the reference fold | 989 | `ch_poison` (51 / 42), `facts` (96 / 0) | `window_expiry` (43 / 50), `long_outages` (43 / 51), `small_batches` (42 / 49) |
| frame quarantined | 564 | `disk_sync_errors` (29 / 24), `clustered` (30 / 23), `facts` (30 / 23), `frame_faults` (49 / 0) | `deep_nesting` (21 / 32), `bad_unicode` (22 / 31), `long_outages` (24 / 30) |
| frame waited for its project salt | 117 | `multi_project` (7 / 4), `mcp` (12 / 0), `mcp_identity` (11 / 0) | `bad_unicode` (4 / 7) |
| insert deduplicated | 537 | `ch_timeouts` (45 / 7) | `crash_loops` (17 / 32), `ch_hang_overload` (21 / 29), `outages` (20 / 31), `long_outages` (21 / 31), `small_segments` (22 / 30), `one_connection` (22 / 29), `mcp_inventory` (22 / 28) |
| insert landed late | 785 | `ch_timeouts` (55 / 20), `ch_poison` (45 / 28), `small_batches` (43 / 34) | `large_payloads` (34 / 41), `crash_loops` (26 / 47), `disk_write_errors` (34 / 40), `outages` (32 / 42), `long_outages` (30 / 45), `small_segments` (34 / 42), `mcp_inventory` (34 / 40) |
| json request acknowledged | 948 | `json` (85 / 0), `ch_timeouts` (50 / 39), `window_expiry` (48 / 42), `small_budget` (48 / 42) | `deep_nesting` (37 / 53), `ch_rejects` (40 / 49) |
| mcp aggregates compared with the reference | 775 | `mcp` (77 / 0), `mcp_inventory` (40 / 33) | `ch_rejects` (31 / 41), `ch_poison` (30 / 44) |
| mcp caller fault | 371 | `concurrent_reads` (20 / 15), `mcp` (37 / 0), `mcp_errors` (33 / 0) | `deep_nesting` (13 / 22), `week_partitions` (15 / 20) |
| mcp operation captured by a proxy | 858 | `multi_project` (43 / 37), `mcp` (85 / 0), `mcp_double_capture` (46 / 35), `mcp_inventory` (44 / 37) | `deep_nesting` (36 / 45), `crash_loops` (36 / 44), `ch_rejects` (35 / 45) |
| mcp operation classified as an error | 497 | `mcp` (49 / 0), `mcp_errors` (45 / 0), `mcp_inventory` (26 / 21), `mcp_deploys` (26 / 21) | `deep_nesting` (20 / 27), `disk_read_faults` (20 / 26), `week_partitions` (21 / 26) |
| mcp operation compared with its exchange | 959 | `mcp` (95 / 0), `mcp_inventory` (50 / 41) | `crash_loops` (41 / 49), `ch_rejects` (39 / 51) |
| mcp user subject hashed | 468 | `multi_project` (25 / 19), `outages` (25 / 20), `tiny_recent` (25 / 19), `mcp` (47 / 0), `mcp_identity` (44 / 0) | `crash_loops` (19 / 25), `disk_sync_errors` (19 / 25), `ch_rejects` (19 / 25) |
| parts merged in the store | 1783 | `small_batches` (94 / 78), `mcp` (87 / 82) | `large_payloads` (82 / 88), `bad_unicode` (81 / 87), `rootless` (82 / 86), `barrier_crashes` (81 / 87), `crash_loops` (78 / 90), `crash_in_recovery` (82 / 88), `wal_compression_off` (81 / 88), `small_segments` (82 / 87) |
| probe answer expired before its send | 136 | `outages` (11 / 2), `mcp` (8 / 5) | `long_outages` (4 / 10) |
| probe read present | 1283 | `ch_ack_lost` (67 / 54), `small_batches` (71 / 54), `mcp` (64 / 57) | `large_payloads` (57 / 66), `deep_nesting` (56 / 65), `bad_unicode` (57 / 64), `crash_loops` (45 / 74), `long_outages` (51 / 71), `mcp_mrtr` (57 / 63) |
| process stop | 976 | `multi_project` (49 / 43), `clean_stops` (90 / 0), `frame_faults` (49 / 42), `nested_usage` (49 / 43), `mcp_deploys` (50 / 42) | `disk_sync_errors` (43 / 49) |
| quarantine | 792 | `multi_day` (40 / 34), `ch_ack_lost` (40 / 34), `ch_poison` (49 / 24), `replay_retention` (42 / 35), `facts` (42 / 33), `frame_faults` (56 / 15) | `deep_nesting` (32 / 43), `bad_unicode` (33 / 41), `crash_loops` (33 / 41), `long_outages` (32 / 43), `small_segments` (34 / 42) |
| read answered lagging | 988 | `concurrent_reads` (65 / 27), `small_batches` (67 / 34), `small_budget` (53 / 41) | `large_payloads` (43 / 53), `deep_nesting` (42 / 52), `bad_unicode` (41 / 52), `client_retries` (43 / 52), `barrier_crashes` (41 / 52), `crash_loops` (39 / 53), `crash_in_recovery` (43 / 53), `wal_compression_off` (44 / 51), `small_segments` (42 / 53) |
| read answered partial (quarantined) | 657 | `multi_day` (34 / 27), `ch_poison` (39 / 22), `small_batches` (35 / 29), `replay_retention` (35 / 28), `frame_faults` (49 / 10) | `large_payloads` (29 / 35), `deep_nesting` (24 / 38), `bad_unicode` (26 / 36), `crash_loops` (27 / 34), `long_outages` (27 / 35), `small_segments` (28 / 36) |
| read answered partial (store_unavailable) | 1513 | `concurrent_reads` (78 / 64), `ch_read_errors` (79 / 63), `outages` (75 / 68), `long_outages` (78 / 64), `tiny_recent` (79 / 64) | `crash_loops` (67 / 75), `disk_full` (69 / 74), `small_batches` (65 / 75), `clustered` (69 / 74) |
| read refused (store unavailable) | 1054 | `concurrent_reads` (63 / 36), `ch_read_errors` (56 / 43), `outages` (56 / 44), `long_outages` (56 / 42), `small_batches` (58 / 44), `tiny_recent` (58 / 42), `mcp` (54 / 45) | `barrier_crashes` (46 / 54), `small_segments` (47 / 54), `nested_usage` (46 / 53) |
| repeated request acknowledged | 998 | `large_payloads` (51 / 40), `client_retries` (74 / 0), `bursts` (52 / 43), `crash_in_recovery` (51 / 40), `disk_sync_errors` (52 / 42), `ch_ack_lost` (52 / 43), `wal_compression_off` (52 / 39), `small_segments` (50 / 42) | `deep_nesting` (42 / 53), `bad_unicode` (43 / 51), `disk_crash_points` (44 / 50), `disk_full` (44 / 50), `small_batches` (38 / 53), `replay_retention` (38 / 52) |
| request refused | 1849 | `large_payloads` (92 / 79), `deep_nesting` (100 / 74), `bad_unicode` (99 / 75), `client_retries` (90 / 83), `crash_in_recovery` (89 / 83), `ch_server_errors` (90 / 85), `wal_compression_off` (90 / 82), `small_segments` (92 / 80), `dir_sync_errors` (89 / 85) | `multi_day` (84 / 90), `small_batches` (82 / 91), `replay_retention` (84 / 89), `mcp_deploys` (83 / 91) |
| request throttled (429) | 101 | `json` (6 / 3), `large_payloads` (7 / 1), `bad_unicode` (6 / 3), `bursts` (7 / 3), `project_rate` (9 / 0) | `multi_project` (2 / 8) |
| request with a repeated span acknowledged | 722 | `client_retries` (54 / 0), `bursts` (40 / 28) | `deep_nesting` (26 / 43), `multi_project` (30 / 38), `crash_loops` (30 / 38), `disk_full` (29 / 39), `small_batches` (29 / 37), `nested_usage` (29 / 39), `dir_sync_errors` (30 / 37) |
| rollups compared with the reference | 1731 | `large_payloads` (84 / 78), `bursts` (85 / 78), `crash_loops` (86 / 78), `ch_timeouts` (84 / 79), `ch_hang_overload` (85 / 78), `long_outages` (86 / 77), `small_segments` (84 / 78) | `ch_ack_lost` (79 / 84), `ch_poison` (67 / 98), `small_batches` (79 / 83), `facts` (79 / 84), `nested_usage` (79 / 84) |
| sealed batch shipped again after a restart | 1761 | `ch_timeouts` (85 / 81), `ch_hang_overload` (86 / 81), `long_outages` (87 / 78), `small_batches` (95 / 76) | `large_payloads` (81 / 87), `bad_unicode` (81 / 85), `crash_loops` (80 / 86), `small_segments` (81 / 86) |
| shipper quarantined a frame | 564 | `disk_sync_errors` (29 / 24), `clustered` (30 / 23), `facts` (30 / 23), `frame_faults` (49 / 0) | `deep_nesting` (21 / 32), `bad_unicode` (22 / 31), `long_outages` (24 / 30) |
| span re-read through its locator | 1836 | `concurrent_reads` (89 / 84) | `json` (84 / 89), `crash_loops` (77 / 95), `small_batches` (82 / 89), `facts` (84 / 89) |
| span read from the recent index | 1838 | `concurrent_reads` (89 / 84) | `json` (84 / 89), `crash_loops` (77 / 95), `small_batches` (82 / 89), `replay_retention` (84 / 88) |
| span read from the store | 2065 | `bursts` (99 / 96) | `deep_nesting` (96 / 99), `bad_unicode` (96 / 99), `crash_loops` (96 / 99), `slow_sync` (96 / 98) |
| store outage | 1510 | `deep_nesting` (75 / 67), `crash_loops` (74 / 68), `ch_ack_lost` (75 / 68), `outages` (94 / 50), `long_outages` (93 / 47), `mcp_inventory` (74 / 68) | `multi_day` (68 / 74), `replay_retention` (67 / 73), `frame_faults` (68 / 75), `dir_sync_errors` (68 / 74) |
| store read fault | 1679 | `concurrent_reads` (84 / 74), `ch_read_errors` (92 / 65), `outages` (89 / 70), `long_outages` (89 / 68), `mcp` (82 / 77) | `disk_full` (76 / 82), `clustered` (76 / 82), `frame_faults` (77 / 82) |
