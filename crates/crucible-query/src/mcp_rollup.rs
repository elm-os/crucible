//! The MCP analytics over the aggregating tables (contract 9.9): servers, tools, one tool, error classes,
//! clients by name, deploys and compare. These are the reads of the 300 ms dashboard target: each reads
//! `mcp_tool_rollup`, `mcp_deploys` and `mcp_tool_inventory` only.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crucible_plan::{
    McpRead, McpSelection, Step, mcp_clients, mcp_compare_tools, mcp_deploys, mcp_error_classes,
    mcp_inventory, mcp_servers, mcp_tool_clients, mcp_tool_series, mcp_tools,
};
use crucible_records::{CaptureSource, ErrorClass, TableId, TimeWindow, Timestamp};

use crate::context::{Meta, QueryServiceError, ReadContext};
use crate::mcp::{
    CaptureSources, GroupLayout, McpQuery, McpTotals, Tally, fields, select_among, series_buckets,
};
use crate::service::{QueryService, Read};

/// The tables of the rollup reads, for the quarantine rule.
const ROLLUP: [TableId; 3] = [
    TableId::McpToolRollup,
    TableId::McpDeploys,
    TableId::McpToolInventory,
];

/// One deploy of a server (contract 9.7).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpDeploy {
    /// The server.
    pub server_name: Arc<str>,
    /// The deploy id; empty when the server reported neither a deploy id nor a version.
    pub deploy_id: Arc<str>,
    /// The server version the deploy reported.
    pub server_version: Arc<str>,
    /// Who observed it.
    pub capture_source: CaptureSource,
    /// First seen.
    pub first_seen: Timestamp,
    /// Last seen.
    pub last_seen: Timestamp,
}

/// One server.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpServer {
    /// The server.
    pub server_name: Arc<str>,
    /// Its calls.
    pub totals: McpTotals,
    /// Distinct self-reported client names that called it.
    pub clients: u64,
    /// The version of its most recently seen deploy, if a deploy was seen in the window.
    pub version: Option<Arc<str>>,
    /// Deploys seen in the window.
    pub deploys: u64,
}

/// The servers of a side.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpServers {
    /// The servers, most calls first.
    pub servers: Vec<McpServer>,
    /// The capture source of each server.
    pub capture_sources: CaptureSources,
    /// The response metadata.
    pub meta: Meta,
}

/// One tool of a server.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpTool {
    /// The server.
    pub server_name: Arc<str>,
    /// The tool.
    pub tool_name: Arc<str>,
    /// Its calls.
    pub totals: McpTotals,
    /// The schema hash of its most recently listed definition, when the inventory has one in the window.
    pub schema_hash: Option<u64>,
    /// Listed in the inventory with no call in the window (contract 3.6).
    pub unused: bool,
}

/// The tools of a side.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpTools {
    /// The called tools, most calls first, then the unused ones by server and name.
    pub tools: Vec<McpTool>,
    /// The capture source of each server.
    pub capture_sources: CaptureSources,
    /// The response metadata.
    pub meta: Meta,
}

/// One bucket of a tool's calls over time.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpPoint {
    /// The bucket's first instant.
    pub bucket: Timestamp,
    /// The calls in the bucket.
    pub totals: McpTotals,
}

/// The calls of a client.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpClient {
    /// The self-reported client name; empty when the client sent none.
    pub client_name: Arc<str>,
    /// Its calls.
    pub totals: McpTotals,
}

/// One tool in detail.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpToolDetail {
    /// The tool.
    pub tool_name: Arc<str>,
    /// Its calls in the window.
    pub totals: McpTotals,
    /// The bucket width of `series`.
    pub step: Step,
    /// Every bucket of the window, empty ones with no calls.
    pub series: Vec<McpPoint>,
    /// Calls per error class other than `none`, most first.
    pub error_classes: Vec<(ErrorClass, u64)>,
    /// The clients with the most calls, at most [`TOP_CLIENTS`].
    pub clients: Vec<McpClient>,
    /// The deploys of the tool's servers seen in the window, oldest first: the markers of the series.
    pub deploys: Vec<McpDeploy>,
    /// The capture source of each server.
    pub capture_sources: CaptureSources,
    /// The response metadata.
    pub meta: Meta,
}

/// The most clients the tool detail lists.
pub const TOP_CLIENTS: usize = 10;

/// The errors of one method and target in one class.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpTargetErrors {
    /// The method.
    pub method: Arc<str>,
    /// The tool or prompt name, or the resource template; empty for methods without one.
    pub target_name: Arc<str>,
    /// The error class.
    pub error_class: ErrorClass,
    /// The calls.
    pub calls: u64,
}

/// The error classes of a side (the gated section of `errors`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpErrorClasses {
    /// Every call of the read.
    pub totals: McpTotals,
    /// Calls per error class other than `none`, most first.
    pub classes: Vec<(ErrorClass, u64)>,
    /// Calls per method, target and error class other than `none`, most first.
    pub targets: Vec<McpTargetErrors>,
    /// The capture source of each server.
    pub capture_sources: CaptureSources,
    /// The response metadata.
    pub meta: Meta,
}

/// The clients of a side by self-reported name (the gated section of `clients`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpClients {
    /// The clients, most calls first.
    pub clients: Vec<McpClient>,
    /// The capture source of each server.
    pub capture_sources: CaptureSources,
    /// The response metadata.
    pub meta: Meta,
}

/// The deploys seen in a window.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpDeploys {
    /// The deploys of each server's chosen capture source, newest first.
    pub deploys: Vec<McpDeploy>,
    /// The capture source of each server, chosen among the deploys' own.
    pub capture_sources: CaptureSources,
    /// The response metadata.
    pub meta: Meta,
}

/// One side of a compare: a deploy of the server inside the request's window, or a window of its own.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CompareSide {
    /// The calls of one deploy.
    Deploy(String),
    /// Every call in a window.
    Window(TimeWindow),
}

/// A compare of one server's tools between two deploys or two windows.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpCompareQuery {
    /// The request: its server is the one compared, and its window holds both deploys.
    pub query: McpQuery,
    /// The side compared against.
    pub baseline: CompareSide,
    /// The side compared.
    pub candidate: CompareSide,
}

/// One tool in a compare.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpToolDelta {
    /// The tool.
    pub tool_name: Arc<str>,
    /// Its calls on the baseline side.
    pub baseline: McpTotals,
    /// Its calls on the candidate side.
    pub candidate: McpTotals,
}

/// The answer to a compare.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpCompare {
    /// Each tool called on either side, by name.
    pub tools: Vec<McpToolDelta>,
    /// Tools listed on the candidate side only.
    pub added: Vec<Arc<str>>,
    /// Tools listed on the baseline side only.
    pub removed: Vec<Arc<str>>,
    /// Tools listed on both sides whose latest schema hash differs.
    pub changed: Vec<Arc<str>>,
    /// The capture source of the server.
    pub capture_sources: CaptureSources,
    /// The response metadata.
    pub meta: Meta,
}

fn by_calls<T>(items: &mut [T], totals: impl Fn(&T) -> &McpTotals, key: impl Fn(&T) -> String) {
    items.sort_by(|a, b| {
        totals(b)
            .calls
            .cmp(&totals(a).calls)
            .then_with(|| key(a).cmp(&key(b)))
    });
}

fn classes(tallies: BTreeMap<ErrorClass, u64>) -> Vec<(ErrorClass, u64)> {
    let mut classes: Vec<(ErrorClass, u64)> = tallies
        .into_iter()
        .filter(|(class, calls)| *class != ErrorClass::None && *calls > 0)
        .collect();
    classes.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    classes
}

fn read_for<'a>(query: &'a McpQuery, selection: &'a McpSelection) -> McpRead<'a> {
    McpRead {
        side: query.side,
        selection,
        include_caller_faults: query.include_caller_faults,
        server: query.server.as_deref(),
    }
}

impl Read<'_> {
    /// The deploys of a window and the selection among their own capture sources.
    async fn deploys_of(
        &mut self,
        query: &McpQuery,
        window: TimeWindow,
    ) -> Result<(Vec<McpDeploy>, McpSelection), QueryServiceError> {
        let plan = mcp_deploys(
            self.ctx.scope,
            window,
            &self.clamp,
            self.ctx.caller,
            query.server.as_deref(),
        );
        let mut found = Vec::new();
        for row in self.rows(plan).await? {
            let f = fields(&row);
            found.push(McpDeploy {
                server_name: f.text(0)?,
                deploy_id: f.text(1)?,
                server_version: f.text(2)?,
                capture_source: f.capture_source(3)?,
                first_seen: f.time(4)?,
                last_seen: f.time(5)?,
            });
        }
        let selection = select_among(
            query.source,
            found
                .iter()
                .map(|d| (Arc::clone(&d.server_name), d.capture_source)),
        );
        found.retain(|d| {
            selection.chosen(&d.server_name)
                == Some(crucible_plan::CaptureClass::of(d.capture_source))
        });
        found.sort_by(|a, b| {
            a.first_seen
                .cmp(&b.first_seen)
                .then_with(|| a.server_name.cmp(&b.server_name))
                .then_with(|| a.deploy_id.cmp(&b.deploy_id))
                .then_with(|| a.server_version.cmp(&b.server_version))
        });
        Ok((found, selection))
    }

    /// The latest schema hash of each `(server, tool)` listed in `window`, and every tool listed per deploy.
    async fn inventory_of(
        &mut self,
        window: TimeWindow,
        server: Option<&str>,
        deploys: &[String],
    ) -> Result<Vec<Listed>, QueryServiceError> {
        let plan = mcp_inventory(
            self.ctx.scope,
            window,
            &self.clamp,
            self.ctx.caller,
            server,
            deploys,
        );
        let mut found = Vec::new();
        for row in self.rows(plan).await? {
            let f = fields(&row);
            found.push(Listed {
                server_name: f.text(0)?,
                deploy_id: f.text(1)?,
                tool_name: f.text(2)?,
                schema_hash: f.uint(3)?,
                last_seen: f.time(5)?,
            });
        }
        Ok(found)
    }
}

/// One tool definition of the inventory.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Listed {
    server_name: Arc<str>,
    deploy_id: Arc<str>,
    tool_name: Arc<str>,
    schema_hash: u64,
    last_seen: Timestamp,
}

/// The latest schema hash per key among listed definitions.
fn latest_hashes<K: Ord>(listed: &[Listed], key: impl Fn(&Listed) -> K) -> BTreeMap<K, u64> {
    let mut latest: BTreeMap<K, (Timestamp, u64)> = BTreeMap::new();
    for item in listed {
        let entry = latest
            .entry(key(item))
            .or_insert((item.last_seen, item.schema_hash));
        // The latest definition wins; at one instant the larger hash, so the answer does not depend on order.
        if (item.last_seen, item.schema_hash) > *entry {
            *entry = (item.last_seen, item.schema_hash);
        }
    }
    latest
        .into_iter()
        .map(|(key, (_, hash))| (key, hash))
        .collect()
}

impl QueryService {
    /// The servers of a side (contract 9.9 `servers`): calls, errors, caller-fault calls, latency, clients and
    /// last seen per server from `mcp_tool_rollup`, and the version of its latest deploy from `mcp_deploys`.
    pub async fn mcp_servers(
        &self,
        ctx: &ReadContext,
        query: &McpQuery,
    ) -> Result<McpServers, QueryServiceError> {
        let consistency = self.consistency();
        let mut read = self.read(ctx, "mcp_servers");
        let selection = read.mcp_selection(query).await?;
        let plan = mcp_servers(
            ctx.scope,
            query.window,
            &read.clamp,
            ctx.caller,
            &read_for(query, &selection),
        );
        let layout = GroupLayout {
            dims: 2,
            histogram: Some(1),
            bytes: None,
            last_seen: Some(2),
        };
        let mut tallies: BTreeMap<Arc<str>, (Tally, BTreeSet<Arc<str>>)> = BTreeMap::new();
        for row in read.rows(plan).await? {
            let f = fields(&row);
            let entry = tallies.entry(f.text(0)?).or_default();
            let client = f.text(1)?;
            if !client.is_empty() {
                entry.1.insert(client);
            }
            entry.0.add(&layout.group(&f)?, query.include_caller_faults);
        }
        let (deploys, _) = read.deploys_of(query, query.window).await?;
        let mut servers: Vec<McpServer> = tallies
            .into_iter()
            .map(|(server_name, (tally, clients))| {
                let own: Vec<&McpDeploy> = deploys
                    .iter()
                    .filter(|d| d.server_name == server_name)
                    .collect();
                let latest = own
                    .iter()
                    .max_by(|a, b| {
                        a.last_seen
                            .cmp(&b.last_seen)
                            .then_with(|| a.server_version.cmp(&b.server_version))
                    })
                    .map(|d| Arc::clone(&d.server_version));
                McpServer {
                    server_name,
                    totals: tally.totals(),
                    clients: clients.len() as u64,
                    version: latest,
                    deploys: own.len() as u64,
                }
            })
            .collect();
        by_calls(&mut servers, |s| &s.totals, |s| s.server_name.to_string());
        let meta = read.mcp_meta(consistency, &ROLLUP)?;
        Ok(McpServers {
            servers,
            capture_sources: selection.servers().to_vec(),
            meta,
        })
    }

    /// The tools of a side (contract 9.9 `tools`): the calls of `tools/call` per server and tool from
    /// `mcp_tool_rollup`, with each tool's schema hash and the unused tools from `mcp_tool_inventory`.
    pub async fn mcp_tools(
        &self,
        ctx: &ReadContext,
        query: &McpQuery,
    ) -> Result<McpTools, QueryServiceError> {
        let consistency = self.consistency();
        let mut read = self.read(ctx, "mcp_tools");
        let selection = read.mcp_selection(query).await?;
        let plan = mcp_tools(
            ctx.scope,
            query.window,
            &read.clamp,
            ctx.caller,
            &read_for(query, &selection),
        );
        let layout = GroupLayout {
            dims: 2,
            histogram: Some(1),
            bytes: Some(2),
            last_seen: Some(4),
        };
        let mut tallies: BTreeMap<(Arc<str>, Arc<str>), Tally> = BTreeMap::new();
        for row in read.rows(plan).await? {
            let f = fields(&row);
            tallies
                .entry((f.text(0)?, f.text(1)?))
                .or_default()
                .add(&layout.group(&f)?, query.include_caller_faults);
        }
        let listed = read
            .inventory_of(query.window, query.server.as_deref(), &[])
            .await?;
        let hashes = latest_hashes(&listed, |l| {
            (Arc::clone(&l.server_name), Arc::clone(&l.tool_name))
        });
        // A server read under another class than the request names has no calls to compare its list with.
        let readable = |server: &str| {
            selection
                .servers()
                .iter()
                .find(|s| s.server_name == server)
                .is_none_or(|s| s.chosen.is_some())
        };
        let mut tools: Vec<McpTool> = tallies
            .iter()
            .map(|((server_name, tool_name), tally)| McpTool {
                server_name: Arc::clone(server_name),
                tool_name: Arc::clone(tool_name),
                totals: tally.totals(),
                schema_hash: hashes
                    .get(&(Arc::clone(server_name), Arc::clone(tool_name)))
                    .copied(),
                unused: false,
            })
            .collect();
        by_calls(
            &mut tools,
            |t| &t.totals,
            |t| format!("{}\0{}", t.server_name, t.tool_name),
        );
        for ((server_name, tool_name), hash) in &hashes {
            let called = tallies
                .get(&(Arc::clone(server_name), Arc::clone(tool_name)))
                .is_some_and(|t| t.totals().calls > 0);
            if !called && readable(server_name) {
                tools.push(McpTool {
                    server_name: Arc::clone(server_name),
                    tool_name: Arc::clone(tool_name),
                    totals: McpTotals::default(),
                    schema_hash: Some(*hash),
                    unused: true,
                });
            }
        }
        let meta = read.mcp_meta(consistency, &ROLLUP)?;
        Ok(McpTools {
            tools,
            capture_sources: selection.servers().to_vec(),
            meta,
        })
    }

    /// One tool (contract 9.9 `tools/{name}`): its calls, errors and latency over the window and per bucket,
    /// its error classes and top clients from `mcp_tool_rollup`, and the deploys of its servers from
    /// `mcp_deploys` as the series' markers.
    pub async fn mcp_tool(
        &self,
        ctx: &ReadContext,
        query: &McpQuery,
        tool_name: &str,
    ) -> Result<McpToolDetail, QueryServiceError> {
        let consistency = self.consistency();
        let mut read = self.read(ctx, "mcp_tool");
        let selection = read.mcp_selection(query).await?;
        let mcp = read_for(query, &selection);
        let (step, buckets) = series_buckets(query.window);
        let plan = mcp_tool_series(
            ctx.scope,
            query.window,
            &read.clamp,
            ctx.caller,
            &mcp,
            tool_name,
            step,
        );
        let series_layout = GroupLayout {
            dims: 0,
            histogram: Some(1),
            bytes: None,
            last_seen: None,
        };
        let mut per_bucket: BTreeMap<Timestamp, Tally> = BTreeMap::new();
        for row in read.rows(plan).await? {
            let values = row.values();
            let (Some(bucket), Some(rest)) = (values.first(), values.get(1..)) else {
                return Err(QueryServiceError::Internal(
                    "the store returned a series row without a bucket".to_owned(),
                ));
            };
            let crucible_plan::Value::Time(bucket) = bucket else {
                return Err(QueryServiceError::Internal(
                    "the store returned a series bucket that is not a time".to_owned(),
                ));
            };
            per_bucket.entry(*bucket).or_default().add(
                &series_layout.group(&crate::decode::Fields(rest))?,
                query.include_caller_faults,
            );
        }
        let series = buckets
            .into_iter()
            .map(|bucket| McpPoint {
                bucket,
                totals: per_bucket
                    .get(&bucket)
                    .map(Tally::totals)
                    .unwrap_or_default(),
            })
            .collect();

        let plan = mcp_tool_clients(
            ctx.scope,
            query.window,
            &read.clamp,
            ctx.caller,
            &mcp,
            tool_name,
        );
        let layout = GroupLayout {
            dims: 2,
            histogram: Some(1),
            bytes: Some(2),
            last_seen: Some(4),
        };
        let (mut total, mut by_class) = (Tally::default(), BTreeMap::new());
        let mut by_client: BTreeMap<Arc<str>, Tally> = BTreeMap::new();
        let mut servers: BTreeSet<Arc<str>> = BTreeSet::new();
        for row in read.rows(plan).await? {
            let f = fields(&row);
            let group = layout.group(&f)?;
            total.add(&group, query.include_caller_faults);
            servers.insert(f.text(0)?);
            by_client
                .entry(f.text(1)?)
                .or_default()
                .add(&group, query.include_caller_faults);
            if (query.include_caller_faults || !group.caller_fault)
                && let Some(class) = group.error_class
            {
                *by_class.entry(class).or_insert(0u64) += group.count;
            }
        }
        let mut clients: Vec<McpClient> = by_client
            .into_iter()
            .map(|(client_name, tally)| McpClient {
                client_name,
                totals: tally.totals(),
            })
            .filter(|c| c.totals.calls > 0)
            .collect();
        by_calls(&mut clients, |c| &c.totals, |c| c.client_name.to_string());
        clients.truncate(TOP_CLIENTS);

        let (mut deploys, _) = read.deploys_of(query, query.window).await?;
        deploys.retain(|d| servers.contains(&d.server_name));
        let meta = read.mcp_meta(consistency, &ROLLUP)?;
        Ok(McpToolDetail {
            tool_name: Arc::from(tool_name),
            totals: total.totals(),
            step,
            series,
            error_classes: classes(by_class),
            clients,
            deploys,
            capture_sources: selection.servers().to_vec(),
            meta,
        })
    }

    /// The error classes of a side (the gated section of contract 9.9 `errors`): calls per error class, and
    /// per method, target and class, from `mcp_tool_rollup`.
    pub async fn mcp_error_classes(
        &self,
        ctx: &ReadContext,
        query: &McpQuery,
    ) -> Result<McpErrorClasses, QueryServiceError> {
        let consistency = self.consistency();
        let mut read = self.read(ctx, "mcp_error_classes");
        let selection = read.mcp_selection(query).await?;
        let plan = mcp_error_classes(
            ctx.scope,
            query.window,
            &read.clamp,
            ctx.caller,
            &read_for(query, &selection),
        );
        let layout = GroupLayout {
            dims: 2,
            histogram: None,
            bytes: None,
            last_seen: None,
        };
        let (mut total, mut by_class) = (Tally::default(), BTreeMap::new());
        let mut by_target: BTreeMap<(Arc<str>, Arc<str>, ErrorClass), u64> = BTreeMap::new();
        for row in read.rows(plan).await? {
            let f = fields(&row);
            let group = layout.group(&f)?;
            total.add(&group, query.include_caller_faults);
            let Some(class) = group.error_class else {
                continue;
            };
            if group.caller_fault && !query.include_caller_faults {
                continue;
            }
            *by_class.entry(class).or_insert(0u64) += group.count;
            if class != ErrorClass::None {
                *by_target
                    .entry((f.text(0)?, f.text(1)?, class))
                    .or_insert(0) += group.count;
            }
        }
        let mut targets: Vec<McpTargetErrors> = by_target
            .into_iter()
            .map(
                |((method, target_name, error_class), calls)| McpTargetErrors {
                    method,
                    target_name,
                    error_class,
                    calls,
                },
            )
            .collect();
        targets.sort_by_key(|t| std::cmp::Reverse(t.calls));
        let meta = read.mcp_meta(consistency, &ROLLUP)?;
        Ok(McpErrorClasses {
            totals: total.totals(),
            classes: classes(by_class),
            targets,
            capture_sources: selection.servers().to_vec(),
            meta,
        })
    }

    /// The clients of a side by self-reported name (the gated section of contract 9.9 `clients`), from
    /// `mcp_tool_rollup`.
    pub async fn mcp_clients(
        &self,
        ctx: &ReadContext,
        query: &McpQuery,
    ) -> Result<McpClients, QueryServiceError> {
        let consistency = self.consistency();
        let mut read = self.read(ctx, "mcp_clients");
        let selection = read.mcp_selection(query).await?;
        let plan = mcp_clients(
            ctx.scope,
            query.window,
            &read.clamp,
            ctx.caller,
            &read_for(query, &selection),
        );
        let layout = GroupLayout {
            dims: 1,
            histogram: Some(1),
            bytes: None,
            last_seen: Some(2),
        };
        let mut by_client: BTreeMap<Arc<str>, Tally> = BTreeMap::new();
        for row in read.rows(plan).await? {
            let f = fields(&row);
            by_client
                .entry(f.text(0)?)
                .or_default()
                .add(&layout.group(&f)?, query.include_caller_faults);
        }
        let mut clients: Vec<McpClient> = by_client
            .into_iter()
            .map(|(client_name, tally)| McpClient {
                client_name,
                totals: tally.totals(),
            })
            .collect();
        by_calls(&mut clients, |c| &c.totals, |c| c.client_name.to_string());
        let meta = read.mcp_meta(consistency, &ROLLUP)?;
        Ok(McpClients {
            clients,
            capture_sources: selection.servers().to_vec(),
            meta,
        })
    }

    /// The deploys seen in the window (contract 9.9 `deploys`), from `mcp_deploys`. Deploys are recorded from
    /// server-side rows, so each server's capture source is chosen among its deploys' own.
    pub async fn mcp_deploys(
        &self,
        ctx: &ReadContext,
        query: &McpQuery,
    ) -> Result<McpDeploys, QueryServiceError> {
        let consistency = self.consistency();
        let mut read = self.read(ctx, "mcp_deploys");
        let (mut deploys, selection) = read.deploys_of(query, query.window).await?;
        deploys.reverse();
        let meta = read.mcp_meta(consistency, &[TableId::McpDeploys])?;
        Ok(McpDeploys {
            deploys,
            capture_sources: selection.servers().to_vec(),
            meta,
        })
    }

    /// A compare of one server's tools (contract 9.9 `compare`): calls, errors and latency per tool on each
    /// side from `mcp_tool_rollup`, and the tools added, removed or changed from `mcp_tool_inventory`. Both
    /// sides read the capture source selected over the request's window.
    pub async fn mcp_compare(
        &self,
        ctx: &ReadContext,
        compare: &McpCompareQuery,
    ) -> Result<McpCompare, QueryServiceError> {
        let query = &compare.query;
        let Some(server) = query.server.as_deref() else {
            return Err(QueryServiceError::Validation("server"));
        };
        let consistency = self.consistency();
        let mut read = self.read(ctx, "mcp_compare");
        let selection = read.mcp_selection(query).await?;
        let mcp = read_for(query, &selection);
        let layout = GroupLayout {
            dims: 2,
            histogram: Some(1),
            bytes: None,
            last_seen: None,
        };
        let mut tallies: BTreeMap<Arc<str>, (Tally, Tally)> = BTreeMap::new();
        let mut listed: [Vec<Listed>; 2] = [Vec::new(), Vec::new()];
        let sides = [&compare.baseline, &compare.candidate];
        for (at, side) in sides.into_iter().enumerate() {
            let (window, deploys) = match side {
                CompareSide::Deploy(id) => (query.window, vec![id.clone()]),
                CompareSide::Window(window) => (*window, Vec::new()),
            };
            let plan =
                mcp_compare_tools(ctx.scope, window, &read.clamp, ctx.caller, &mcp, &deploys);
            for row in read.rows(plan).await? {
                let f = fields(&row);
                let entry = tallies.entry(f.text(1)?).or_default();
                let tally = if at == 0 { &mut entry.0 } else { &mut entry.1 };
                tally.add(&layout.group(&f)?, query.include_caller_faults);
            }
            if let Some(found) = listed.get_mut(at) {
                *found = read.inventory_of(window, Some(server), &deploys).await?;
            }
        }
        let tools = tallies
            .into_iter()
            .map(|(tool_name, (baseline, candidate))| McpToolDelta {
                tool_name,
                baseline: baseline.totals(),
                candidate: candidate.totals(),
            })
            .collect();
        let [before, after] = listed;
        let before = latest_hashes(&before, |l| Arc::clone(&l.tool_name));
        let after = latest_hashes(&after, |l| Arc::clone(&l.tool_name));
        let added = after
            .keys()
            .filter(|t| !before.contains_key(*t))
            .cloned()
            .collect();
        let removed = before
            .keys()
            .filter(|t| !after.contains_key(*t))
            .cloned()
            .collect();
        let changed = after
            .iter()
            .filter(|(t, hash)| before.get(*t).is_some_and(|h| h != *hash))
            .map(|(t, _)| Arc::clone(t))
            .collect();
        let meta = read.mcp_meta(consistency, &ROLLUP)?;
        Ok(McpCompare {
            tools,
            added,
            removed,
            changed,
            capture_sources: selection.servers().to_vec(),
            meta,
        })
    }
}
