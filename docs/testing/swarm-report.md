# Swarm report

Written by `cargo xtask sim report` from the run log of `cargo xtask sim run`. The method is in
[`system-simulation.md`](system-simulation.md).

Runs: 2122 (2122 passed, 0 failed, 0 with losses expected under the fsync lie).

## Dimensions

| Dimension | Kind | Runs with it on |
|---|---|---|
| `json` | workload | 1036 |
| `large_payloads` | workload | 1271 |
| `deep_nesting` | workload | 1126 |
| `bad_unicode` | workload | 1081 |
| `rootless` | workload | 1136 |
| `concurrent_reads` | workload | 1015 |
| `client_retries` | workload | 1289 |
| `multi_project` | workload | 1053 |
| `bursts` | workload | 1092 |
| `multi_day` | workload | 1041 |
| `disk_crash_points` | fault | 1041 |
| `barrier_crashes` | fault | 1010 |
| `crash_loops` | fault | 1031 |
| `crash_in_recovery` | fault | 1348 |
| `clean_stops` | fault | 1036 |
| `disk_write_errors` | fault | 1071 |
| `disk_read_faults` | fault | 1097 |
| `disk_sync_errors` | fault | 1106 |
| `disk_full` | fault | 1075 |
| `slow_sync` | fault | 1120 |
| `ch_timeouts` | fault | 1063 |
| `ch_ack_lost` | fault | 1066 |
| `ch_server_errors` | fault | 1153 |
| `ch_rejects` | fault | 1099 |
| `ch_hang_overload` | fault | 1032 |
| `ch_poison` | fault | 1006 |
| `ch_probe_errors` | fault | 1035 |
| `ch_read_errors` | fault | 1003 |
| `window_expiry` | fault | 1078 |
| `outages` | fault | 962 |
| `long_outages` | fault | 1099 |
| `wall_clock_steps` | fault | 1076 |
| `wal_compression_off` | config | 1288 |
| `small_batches` | config | 804 |
| `week_partitions` | config | 1070 |
| `tiny_recent` | config | 1132 |
| `small_segments` | config | 1315 |
| `clustered` | config | 1046 |
| `one_connection` | config | 1051 |
| `replay_retention` | config | 838 |
| `fsync_lie` | fault | 1043 |
| `facts` | workload | 1067 |
| `frame_faults` | fault | 1029 |
| `project_rate` | config | 1124 |
| `small_budget` | config | 1071 |
| `nested_usage` | workload | 1029 |
| `mcp` | workload | 1011 |
| `mcp_double_capture` | workload | 1077 |
| `mcp_errors` | workload | 1017 |
| `mcp_mrtr` | workload | 1113 |
| `mcp_inventory` | workload | 1017 |
| `mcp_deploys` | workload | 1092 |
| `mcp_content` | workload | 1131 |
| `mcp_identity` | workload | 1026 |
| `dir_sync_errors` | fault | 987 |

## Targets

A trigger raises the rate of a target and a suppressor lowers it; a dimension is listed when the 95% Wilson
intervals of the two rates do not overlap. The rates are `on` and `off` in percent.
Each cell is its own test, so of the roughly thousand dimension and target pairs a few dozen are listed by
chance; a pair that matters shows a large difference and returns in the next log.

| Target | Runs | Triggers | Suppressors |
|---|---|---|---|
| crash at before checkpoint.appended | 20 | `barrier_crashes` (2 / 0) | `long_outages` (0 / 2), `one_connection` (0 / 2) |
| crash at before insert.sent | 167 | `barrier_crashes` (17 / 0), `small_batches` (11 / 6) | `deep_nesting` (6 / 10), `client_retries` (6 / 11), `one_connection` (6 / 10) |
| crash at before seal.appended | 54 | `barrier_crashes` (5 / 0), `small_batches` (4 / 2), `small_budget` (4 / 2) | none |
| crash at checkpoint.appended | 27 | `barrier_crashes` (3 / 0) | `long_outages` (0 / 2) |
| crash at insert.sent | 120 | `barrier_crashes` (12 / 0), `replay_retention` (8 / 4) | `crash_loops` (3 / 8), `small_segments` (4 / 8) |
| crash at quarantine.copied | 37 | `barrier_crashes` (4 / 0), `ch_poison` (4 / 0), `small_batches` (3 / 1) | none |
| crash at seal.appended | 53 | `barrier_crashes` (5 / 0), `small_batches` (4 / 2) | none |
| crash during wal recovery | 1454 | `large_payloads` (77 / 56), `client_retries` (74 / 59), `crash_loops` (75 / 62), `crash_in_recovery` (98 / 17), `ch_read_errors` (71 / 66), `wal_compression_off` (74 / 60), `small_segments` (74 / 60), `nested_usage` (72 / 66) | `deep_nesting` (65 / 73), `window_expiry` (65 / 72), `small_batches` (60 / 74), `replay_retention` (58 / 75) |
| deduplication entry expired early | 767 | `window_expiry` (71 / 0), `clustered` (40 / 33), `nested_usage` (40 / 32) | `large_payloads` (32 / 43), `bad_unicode` (33 / 39), `rootless` (33 / 39), `client_retries` (33 / 40), `crash_loops` (27 / 44), `crash_in_recovery` (33 / 42), `disk_full` (31 / 41), `long_outages` (29 / 44), `mcp_double_capture` (33 / 39) |
| directory syncs failed | 877 | `concurrent_reads` (45 / 38), `multi_project` (44 / 38), `dir_sync_errors` (89 / 0) | `barrier_crashes` (37 / 45), `wall_clock_steps` (37 / 46) |
| fact write acknowledged | 1007 | `facts` (94 / 0), `mcp` (53 / 42), `mcp_double_capture` (50 / 44), `mcp_content` (51 / 44), `mcp_identity` (51 / 44) | `disk_full` (44 / 51) |
| fact write refused | 228 | `large_payloads` (14 / 6), `client_retries` (13 / 7), `crash_in_recovery` (12 / 8), `disk_write_errors` (13 / 9), `window_expiry` (13 / 9), `wal_compression_off` (15 / 4), `small_segments` (15 / 4), `facts` (21 / 0), `mcp` (13 / 9) | `deep_nesting` (8 / 14), `small_batches` (7 / 13), `replay_retention` (7 / 13) |
| facts resolved against the reference fold | 1009 | `facts` (95 / 0), `mcp` (53 / 43), `mcp_double_capture` (51 / 44), `mcp_content` (51 / 44), `mcp_identity` (51 / 44) | `disk_full` (44 / 51) |
| frame quarantined | 452 | `small_batches` (25 / 19), `frame_faults` (44 / 0) | `deep_nesting` (18 / 25), `small_segments` (19 / 25) |
| frame waited for its project salt | 130 | `facts` (8 / 4), `mcp` (13 / 0), `mcp_identity` (13 / 0) | none |
| insert deduplicated | 554 | `ch_timeouts` (47 / 6), `ch_poison` (32 / 21), `small_batches` (31 / 23) | `crash_loops` (21 / 31), `ch_rejects` (23 / 29), `ch_hang_overload` (23 / 29), `outages` (23 / 29), `long_outages` (19 / 34) |
| insert landed late | 783 | `ch_timeouts` (56 / 17), `ch_poison` (50 / 25), `small_batches` (42 / 34) | `large_payloads` (34 / 41), `client_retries` (34 / 41), `crash_loops` (29 / 44), `outages` (33 / 40), `long_outages` (27 / 47) |
| json request acknowledged | 854 | `json` (82 / 0), `multi_day` (44 / 37), `disk_sync_errors` (44 / 37), `project_rate` (44 / 37), `mcp_content` (43 / 37) | `deep_nesting` (34 / 47), `bad_unicode` (36 / 45), `client_retries` (38 / 44), `ch_server_errors` (36 / 45) |
| mcp aggregates compared with the reference | 774 | `multi_project` (40 / 33), `replay_retention` (41 / 33), `facts` (40 / 33), `project_rate` (40 / 33), `mcp` (77 / 0), `mcp_inventory` (40 / 33), `mcp_identity` (40 / 33) | `clean_stops` (33 / 39), `ch_poison` (28 / 44), `week_partitions` (34 / 39), `fsync_lie` (32 / 41) |
| mcp caller fault | 342 | `replay_retention` (21 / 13), `mcp` (34 / 0), `mcp_errors` (34 / 0), `mcp_deploys` (19 / 13) | `deep_nesting` (13 / 20), `client_retries` (13 / 21), `ch_timeouts` (14 / 19), `ch_probe_errors` (13 / 19), `ch_read_errors` (14 / 18), `one_connection` (14 / 19), `fsync_lie` (13 / 19) |
| mcp operation captured by a proxy | 854 | `replay_retention` (44 / 38), `facts` (44 / 36), `project_rate` (43 / 37), `mcp` (84 / 0), `mcp_identity` (44 / 37) | `deep_nesting` (37 / 44), `client_retries` (37 / 45), `clean_stops` (37 / 43), `slow_sync` (37 / 44), `week_partitions` (36 / 45), `one_connection` (36 / 44), `fsync_lie` (36 / 45) |
| mcp operation classified as an error | 456 | `bad_unicode` (25 / 18), `disk_write_errors` (25 / 18), `replay_retention` (25 / 19), `mcp` (45 / 0), `mcp_errors` (45 / 0), `mcp_deploys` (24 / 18) | `deep_nesting` (19 / 25), `client_retries` (19 / 26), `ch_timeouts` (19 / 24), `ch_probe_errors` (18 / 25), `one_connection` (19 / 24), `fsync_lie` (18 / 25) |
| mcp operation compared with its exchange | 954 | `bad_unicode` (48 / 41), `replay_retention` (49 / 42), `facts` (49 / 41), `project_rate` (48 / 41), `mcp` (94 / 0), `mcp_identity` (49 / 42) | `client_retries` (42 / 50), `clean_stops` (42 / 48), `disk_full` (42 / 48), `slow_sync` (42 / 49), `week_partitions` (41 / 49), `one_connection` (42 / 48), `fsync_lie` (39 / 51) |
| mcp user subject hashed | 469 | `barrier_crashes` (26 / 18), `wall_clock_steps` (25 / 19), `facts` (27 / 17), `mcp` (46 / 0), `mcp_identity` (46 / 0) | `ch_probe_errors` (20 / 25), `one_connection` (18 / 26), `fsync_lie` (19 / 25) |
| parts merged in the store | 1800 | `small_batches` (94 / 79), `replay_retention` (90 / 81), `small_budget` (87 / 82) | `large_payloads` (83 / 88), `deep_nesting` (82 / 88), `client_retries` (81 / 91), `crash_loops` (80 / 89), `crash_in_recovery` (82 / 89), `ch_hang_overload` (82 / 87), `wall_clock_steps` (82 / 87), `wal_compression_off` (82 / 89), `small_segments` (81 / 91) |
| probe answer expired before its send | 132 | `crash_loops` (8 / 5), `outages` (11 / 2) | `long_outages` (4 / 9) |
| probe read present | 1274 | `barrier_crashes` (64 / 56), `ch_timeouts` (64 / 56), `ch_ack_lost` (67 / 53), `small_batches` (75 / 51), `nested_usage` (63 / 57) | `large_payloads` (56 / 66), `deep_nesting` (57 / 64), `client_retries` (57 / 64), `crash_loops` (48 / 71), `crash_in_recovery` (58 / 64), `disk_read_faults` (56 / 64), `long_outages` (47 / 74) |
| process stop | 949 | `bursts` (48 / 41), `crash_loops` (51 / 39), `clean_stops` (92 / 0) | `mcp` (41 / 48) |
| quarantine | 715 | `multi_day` (37 / 30), `ch_poison` (49 / 20), `small_batches` (38 / 31), `frame_faults` (51 / 17) | `large_payloads` (31 / 38), `deep_nesting` (30 / 38), `crash_in_recovery` (31 / 38), `ch_hang_overload` (31 / 37), `outages` (30 / 37), `long_outages` (29 / 39) |
| read answered lagging | 921 | `concurrent_reads` (63 / 25), `small_batches` (61 / 33), `replay_retention` (51 / 39), `small_budget` (53 / 33), `dir_sync_errors` (47 / 41) | `large_payloads` (39 / 49), `deep_nesting` (40 / 48), `bad_unicode` (40 / 47), `client_retries` (40 / 48), `crash_loops` (38 / 48), `crash_in_recovery` (39 / 51), `wal_compression_off` (40 / 48), `small_segments` (39 / 51) |
| read answered partial (quarantined) | 626 | `multi_day` (32 / 27), `ch_poison` (40 / 20), `small_batches` (35 / 26), `frame_faults` (49 / 11) | `large_payloads` (27 / 33), `deep_nesting` (26 / 33), `crash_in_recovery` (27 / 34), `disk_full` (26 / 33), `outages` (26 / 32), `long_outages` (26 / 33) |
| read answered partial (store_unavailable) | 1529 | `concurrent_reads` (78 / 67), `ch_read_errors` (83 / 62), `outages` (80 / 65), `long_outages` (79 / 64), `tiny_recent` (80 / 63) | `crash_loops` (67 / 77), `small_batches` (65 / 76), `mcp_double_capture` (69 / 75) |
| read refused (store unavailable) | 952 | `concurrent_reads` (58 / 33), `ch_ack_lost` (48 / 41), `ch_read_errors` (56 / 35), `outages` (53 / 38), `long_outages` (55 / 34), `small_batches` (51 / 41), `tiny_recent` (50 / 39), `replay_retention` (52 / 40), `small_budget` (48 / 41) | `deep_nesting` (42 / 48), `wal_compression_off` (42 / 49), `small_segments` (41 / 50) |
| repeated request acknowledged | 910 | `large_payloads` (48 / 36), `client_retries` (71 / 0), `crash_in_recovery` (47 / 36), `wal_compression_off` (48 / 34), `small_segments` (46 / 38) | `deep_nesting` (37 / 50), `bad_unicode` (38 / 48), `small_batches` (36 / 47), `replay_retention` (39 / 46), `mcp` (39 / 47) |
| request refused | 1858 | `large_payloads` (91 / 83), `deep_nesting` (100 / 73), `bad_unicode` (99 / 75), `client_retries` (89 / 85), `disk_crash_points` (90 / 86), `disk_write_errors` (90 / 85), `ch_probe_errors` (90 / 85), `wal_compression_off` (91 / 82), `tiny_recent` (90 / 85), `small_segments` (91 / 82), `frame_faults` (90 / 85), `mcp` (91 / 85), `dir_sync_errors` (90 / 86) | `slow_sync` (85 / 90), `replay_retention` (84 / 90) |
| request throttled (429) | 141 | `json` (9 / 5), `large_payloads` (10 / 2), `deep_nesting` (9 / 4), `bursts` (10 / 3), `project_rate` (13 / 0), `mcp` (9 / 5) | `multi_project` (2 / 11), `small_batches` (5 / 8) |
| request with a repeated span acknowledged | 731 | `large_payloads` (37 / 30), `client_retries` (57 / 0), `ch_ack_lost` (37 / 31), `wal_compression_off` (37 / 30) | `deep_nesting` (27 / 43), `bad_unicode` (31 / 38), `small_batches` (31 / 37) |
| rollups compared with the reference | 1719 | `ch_hang_overload` (85 / 77), `long_outages` (87 / 75) | `ch_poison` (62 / 98) |
| sealed batch shipped again after a restart | 1742 | `barrier_crashes` (87 / 78), `ch_hang_overload` (85 / 79), `long_outages` (87 / 77), `small_batches` (94 / 75), `replay_retention` (87 / 79), `small_budget` (85 / 79) | `large_payloads` (79 / 87), `client_retries` (80 / 86), `crash_loops` (80 / 84), `disk_read_faults` (80 / 84), `wal_compression_off` (79 / 86), `small_segments` (79 / 86) |
| shipper quarantined a frame | 452 | `small_batches` (25 / 19), `frame_faults` (44 / 0) | `deep_nesting` (18 / 25), `small_segments` (19 / 25) |
| span re-read through its locator | 1779 | `concurrent_reads` (87 / 81) | `crash_loops` (76 / 91), `small_batches` (78 / 87) |
| span read from the recent index | 1781 | `concurrent_reads` (87 / 81) | `crash_loops` (76 / 91), `small_batches` (79 / 87) |
| span read from the store | 2059 | `bursts` (98 / 96) | `deep_nesting` (96 / 98), `client_retries` (96 / 99), `crash_loops` (95 / 99) |
| store outage | 1444 | `crash_loops` (72 / 64), `disk_full` (73 / 63), `outages` (93 / 47), `long_outages` (93 / 41) | `multi_day` (64 / 72), `ch_poison` (65 / 71) |
| store read fault | 1628 | `disk_full` (81 / 72), `ch_read_errors` (95 / 60), `outages` (90 / 66), `long_outages` (90 / 63) | `mcp_double_capture` (74 / 80) |
