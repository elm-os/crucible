//! The API's fact writes through ingest (contract 3.8 and 4.5): the `FactWriter` of `crucible-api` over
//! `Ingest::append_facts`, which acknowledges once every `FactBatch` frame is durable.

use crucible_api::{AcceptedFact, FactAppend, FactAuthor, FactRefusal, FactWriter, NewFact};
use crucible_ingest::{self as ingest, FactWriteError, Ingest};
use crucible_records::ProjectScope;

/// Fact writes through the node's ingest.
#[derive(Debug)]
pub(crate) struct IngestFacts(pub(crate) Ingest);

impl FactWriter for IngestFacts {
    fn append(
        &self,
        scope: ProjectScope,
        author: FactAuthor,
        facts: Vec<NewFact>,
    ) -> FactAppend<'_> {
        Box::pin(async move {
            let facts: Vec<ingest::NewFact> = facts
                .into_iter()
                .map(|fact| ingest::NewFact {
                    target_kind: fact.target_kind,
                    trace_id: fact.trace_id,
                    span_id: fact.span_id,
                    session_id: fact.session_id,
                    target_time: fact.target_time,
                    key: fact.key,
                    retracted: fact.retracted,
                    body: fact.body,
                })
                .collect();
            let author = ingest::FactAuthor {
                kind: author.kind,
                id: author.id,
            };
            match self.0.append_facts(&scope, author, &facts).await {
                Ok(accepted) => Ok(accepted
                    .into_iter()
                    .map(|fact| AcceptedFact {
                        fact_id: fact.fact_id,
                        version: fact.version,
                        created_at: fact.created_at,
                    })
                    .collect()),
                Err(error) => Err(match error {
                    FactWriteError::Invalid { reason, .. } => FactRefusal::Invalid(reason),
                    FactWriteError::TooLarge { reason, .. } => FactRefusal::TooLarge(reason),
                    FactWriteError::Throttled { retry_after_s, .. } => {
                        FactRefusal::Throttled { retry_after_s }
                    }
                    FactWriteError::Unavailable { retry_after_s, .. } => {
                        FactRefusal::Unavailable { retry_after_s }
                    }
                    FactWriteError::Internal(reason) => FactRefusal::Internal(reason),
                }),
            }
        })
    }
}
