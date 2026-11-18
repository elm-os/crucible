//! The key-to-sources index over the dialect layers (contract 10.2), built once, so that mapping one attribute
//! is one hash lookup plus a scan of the few prefixes that share its first byte, however many tables exist.
//!
//! Every source, type rule and key detection rule of every layer is filed under its key. A source's rank is
//! (layer position, position in its field's source list): lower wins, which makes a field's effective source
//! list the list of contract 10.3. A type rule's priority is (its contract 10.4 rank, layer position, position in
//! the table).

use std::sync::LazyLock;

use crucible_compat::{
    Decoder, DetectOn, DialectTable, KeyMatch, LAYERS, Location, MapValues, Scope, SdkGate, Source,
    Target, TypeRule,
};
use crucible_records::{DetMap, det_map};

/// Where a target's candidates are collected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Slot {
    /// A field that takes one value: its position in the scalar slots.
    Scalar(u8),
    /// `usage_provided`, merged per key.
    Usage,
    /// `cost_provided`, merged per key.
    Cost,
    /// A map field, merged per member: 0 `metadata`, 1 `trace_metadata`, 2 `model_parameters`.
    Map(u8),
}

/// The number of scalar slots.
pub(crate) const SCALAR_SLOTS: usize = 31;

/// The number of map fields.
pub(crate) const MAPS: usize = 3;

/// The slot of a target.
pub(crate) const fn slot(target: Target) -> Slot {
    Slot::Scalar(match target {
        Target::Usage(_) | Target::UsageAll => return Slot::Usage,
        Target::Cost(_) | Target::CostAll => return Slot::Cost,
        Target::Metadata => return Slot::Map(0),
        Target::TraceMetadata => return Slot::Map(1),
        Target::ModelParameters => return Slot::Map(2),
        Target::ServiceName => 0,
        Target::ServiceVersion => 1,
        Target::TelemetrySdkName => 2,
        Target::TelemetrySdkLanguage => 3,
        Target::TelemetrySdkVersion => 4,
        Target::UserId => 5,
        Target::SessionId => 6,
        Target::ModelRequested => 7,
        Target::ModelResponse => 8,
        Target::Provider => 9,
        Target::PromptName => 10,
        Target::FinishReasons => 11,
        Target::Input => 12,
        Target::Output => 13,
        Target::SystemInstructions => 14,
        Target::ToolDefinitions => 15,
        Target::TraceName => 16,
        Target::TraceTags => 17,
        Target::TracePublic => 18,
        Target::Tags => 19,
        Target::Level => 20,
        Target::StatusMessage => 21,
        Target::Environment => 22,
        Target::Release => 23,
        Target::Version => 24,
        Target::PromptVersion => 25,
        Target::CompletionStartTime => 26,
        Target::IsRoot => 27,
        Target::IsAppRoot => 28,
        Target::ToolCalls => 29,
        Target::ToolNames => 30,
    })
}

/// One source of one layer, as the engine reads it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SourceHit {
    pub(crate) target: Target,
    pub(crate) slot: Slot,
    /// (layer position, position in the field's source list): lower wins.
    pub(crate) rank: u16,
    /// Position of the table in the layers.
    pub(crate) layer: u8,
    /// The dialect of the table.
    pub(crate) dialect: u8,
    /// Whether the source is read only on spans of its table's dialect.
    pub(crate) own: bool,
    pub(crate) source: &'static Source,
    /// The family the source collects (indexed and usage prefixes); `u16::MAX` for any other source.
    pub(crate) family: u16,
    /// The gate of a JSON-encoding prefix map of the same layer that covers this exact key: while it holds, a
    /// string under this key that is a JSON string literal is read as the text it encodes.
    pub(crate) literal_gate: Option<&'static SdkGate>,
}

/// What is filed under a key.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Hit {
    Source(SourceHit),
    Type {
        /// (contract 10.4 rank, layer position, position in the table): lower wins.
        priority: u32,
        rule: &'static TypeRule,
    },
    Detect {
        dialect: u8,
        confidence: u8,
    },
}

/// A source read from events.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EventSource {
    /// The event name.
    pub(crate) name: &'static str,
    /// Whether the source reads the event's time; otherwise it reads the event's attributes.
    pub(crate) time: bool,
    pub(crate) hit: SourceHit,
}

type Range = (u32, u32);

#[derive(Debug)]
struct Keys {
    exact: DetMap<&'static str, Range>,
    /// Prefixes in ascending byte order.
    prefixes: Vec<(&'static str, Range)>,
    /// The range of `prefixes` that starts with each byte.
    by_first: Box<[(u16, u16); 256]>,
}

impl Keys {
    fn new(entries: Vec<(KeyMatch, Hit)>, hits: &mut Vec<Hit>) -> Self {
        let mut groups: Vec<(KeyMatch, Vec<Hit>)> = Vec::new();
        for (key, hit) in entries {
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, group)) => group.push(hit),
                None => groups.push((key, vec![hit])),
            }
        }
        let mut exact = det_map();
        let mut prefixes = Vec::new();
        for (key, group) in groups {
            let start = hits.len() as u32;
            hits.extend(group);
            let range = (start, hits.len() as u32);
            match key {
                KeyMatch::Exact(key) => {
                    exact.insert(key, range);
                }
                KeyMatch::Prefix(prefix) => prefixes.push((prefix, range)),
            }
        }
        prefixes.sort_by_key(|(prefix, _)| *prefix);
        let mut by_first = Box::new([(0u16, 0u16); 256]);
        for (i, (prefix, _)) in prefixes.iter().enumerate() {
            let Some(&first) = prefix.as_bytes().first() else {
                continue;
            };
            let entry = &mut by_first[usize::from(first)];
            if entry.0 == entry.1 {
                entry.0 = i as u16;
            }
            entry.1 = i as u16 + 1;
        }
        Self {
            exact,
            prefixes,
            by_first,
        }
    }

    #[inline]
    fn for_each<'i>(&'i self, hits: &'i [Hit], key: &str, mut f: impl FnMut(&'i Hit)) {
        let mut run = |&(start, end): &Range| {
            for hit in hits.get(start as usize..end as usize).unwrap_or_default() {
                f(hit);
            }
        };
        if let Some(range) = self.exact.get(key) {
            run(range);
        }
        let Some(&first) = key.as_bytes().first() else {
            return;
        };
        let (start, end) = self.by_first[usize::from(first)];
        for (prefix, range) in self
            .prefixes
            .get(usize::from(start)..usize::from(end))
            .unwrap_or_default()
        {
            if key.starts_with(prefix) {
                run(range);
            }
        }
    }
}

/// The index over a list of layers.
#[derive(Debug)]
pub(crate) struct Index {
    hits: Vec<Hit>,
    span: Keys,
    resource: Keys,
    /// Sources read from events.
    pub(crate) events: Vec<EventSource>,
    /// Detection rules on the instrumentation scope name, as (name, dialect, confidence).
    pub(crate) scopes: Vec<(KeyMatch, u8, u8)>,
    /// How many families the layers collect.
    pub(crate) families: usize,
    /// The sources of every family, by family id.
    pub(crate) family_sources: Vec<SourceHit>,
    layers: Vec<&'static DialectTable>,
}

impl Index {
    /// The index over `layers`, highest priority first.
    pub(crate) fn new(layers: &[&'static DialectTable]) -> Self {
        let mut span = Vec::new();
        let mut resource = Vec::new();
        let mut events = Vec::new();
        let mut scopes = Vec::new();
        let mut family_sources = Vec::new();
        for (layer, table) in layers.iter().enumerate() {
            let layer = layer as u8;
            for rule in table.detect {
                match rule.on {
                    DetectOn::ScopeName(name) => {
                        scopes.push((name, table.dialect, rule.confidence.0));
                    }
                    DetectOn::SpanKey(key) => span.push((
                        key,
                        Hit::Detect {
                            dialect: table.dialect,
                            confidence: rule.confidence.0,
                        },
                    )),
                }
            }
            for (position, rule) in table.observation_types.iter().enumerate() {
                let priority =
                    (u32::from(rule.rank) << 16) | (u32::from(layer) << 8) | position as u32;
                span.push((KeyMatch::Exact(rule.key), Hit::Type { priority, rule }));
            }
            for field in table.fields {
                for (position, source) in field.sources.iter().enumerate() {
                    let collects = matches!(
                        source.decoder,
                        Decoder::Indexed(_) | Decoder::IndexedList(_) | Decoder::PrefixUsage(_)
                    );
                    let mut hit = SourceHit {
                        target: field.target,
                        slot: slot(field.target),
                        rank: (u16::from(layer) << 8) | position.min(255) as u16,
                        layer,
                        dialect: table.dialect,
                        own: source.scope == Scope::Own,
                        source,
                        family: u16::MAX,
                        literal_gate: literal_gate(table, source),
                    };
                    if collects {
                        hit.family = family_sources.len() as u16;
                        family_sources.push(hit);
                    }
                    match source.location {
                        Location::Span => span.push((source.key, Hit::Source(hit))),
                        Location::Resource => resource.push((source.key, Hit::Source(hit))),
                        Location::Event(name) => events.push(EventSource {
                            name,
                            time: false,
                            hit,
                        }),
                        Location::EventTime(name) => events.push(EventSource {
                            name,
                            time: true,
                            hit,
                        }),
                    }
                }
            }
        }
        let mut hits = Vec::new();
        let span = Keys::new(span, &mut hits);
        let resource = Keys::new(resource, &mut hits);
        Self {
            hits,
            span,
            resource,
            events,
            scopes,
            families: family_sources.len(),
            family_sources,
            layers: layers.to_vec(),
        }
    }

    /// Calls `f` with everything filed under a span attribute key.
    #[inline]
    pub(crate) fn span<'i>(&'i self, key: &str, f: impl FnMut(&'i Hit)) {
        self.span.for_each(&self.hits, key, f);
    }

    /// Calls `f` with everything filed under a resource attribute key.
    #[inline]
    pub(crate) fn resource<'i>(&'i self, key: &str, f: impl FnMut(&'i Hit)) {
        self.resource.for_each(&self.hits, key, f);
    }

    /// The layers, highest priority first.
    pub(crate) fn layers(&self) -> &[&'static DialectTable] {
        &self.layers
    }
}

/// The gate of the JSON-encoding prefix map of `table` that covers the exact span key of `source`, if any.
fn literal_gate(table: &DialectTable, source: &Source) -> Option<&'static SdkGate> {
    let KeyMatch::Exact(key) = source.key else {
        return None;
    };
    if source.location != Location::Span {
        return None;
    }
    table
        .fields
        .iter()
        .flat_map(|field| field.sources)
        .find_map(|other| match (other.location, other.key, other.decoder) {
            (Location::Span, KeyMatch::Prefix(prefix), Decoder::PrefixMap(spec))
                if key.starts_with(prefix) =>
            {
                match spec.values {
                    MapValues::JsonWhen(gate) => Some(gate),
                    MapValues::Native => None,
                }
            }
            _ => None,
        })
}

/// The index over the production layers ([`LAYERS`]).
pub(crate) static PRODUCTION: LazyLock<Index> = LazyLock::new(|| Index::new(&LAYERS));

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(index: &Index, key: &str) -> Vec<(u16, Target)> {
        let mut found = Vec::new();
        index.span(key, |hit| {
            if let Hit::Source(s) = hit {
                found.push((s.rank, s.target));
            }
        });
        found.sort_by_key(|(rank, _)| *rank);
        found
    }

    #[test]
    fn every_source_is_filed_under_its_key_with_its_layer_rank() {
        let index = &*PRODUCTION;
        let mut expected = 0;
        for (layer, table) in LAYERS.iter().enumerate() {
            for field in table.fields {
                for (position, source) in field.sources.iter().enumerate() {
                    expected += 1;
                    let rank = ((layer as u16) << 8) | position as u16;
                    let probe = match source.key {
                        KeyMatch::Exact(key) => key.to_owned(),
                        KeyMatch::Prefix(prefix) => format!("{prefix}x"),
                    };
                    let mut found = false;
                    let check = |hit: &Hit| {
                        if let Hit::Source(s) = hit {
                            found |= s.rank == rank && s.target == field.target;
                        }
                    };
                    match source.location {
                        Location::Span => index.span(&probe, check),
                        Location::Resource => index.resource(&probe, check),
                        Location::Event(_) | Location::EventTime(_) => {
                            found = index
                                .events
                                .iter()
                                .any(|e| e.hit.rank == rank && e.hit.target == field.target);
                        }
                    }
                    assert!(found, "{source:?}");
                }
            }
        }
        let filed = index
            .hits
            .iter()
            .filter(|h| matches!(h, Hit::Source(_)))
            .count()
            + index.events.len();
        assert_eq!(filed, expected);
    }

    #[test]
    fn a_key_claimed_by_two_layers_lists_both_in_rank_order() {
        let index = &*PRODUCTION;
        let model: Vec<_> = sources(index, "gen_ai.request.model")
            .into_iter()
            .filter(|(_, t)| *t == Target::ModelRequested)
            .collect();
        assert_eq!(model.len(), 2);
        assert!(model[0].0 >> 8 < model[1].0 >> 8);
        // The model key is excluded from the request-parameter prefix by the prefix's own exception list, so the
        // prefix is still filed under it.
        assert!(
            sources(index, "gen_ai.request.temperature")
                .iter()
                .any(|(_, t)| *t == Target::ModelParameters)
        );
        assert!(sources(index, "unknown.key").is_empty());
        assert!(sources(index, "").is_empty());
    }
}
