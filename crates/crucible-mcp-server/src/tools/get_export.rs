//! `get_export`: one export of the project (contract 12.8), to follow an export that `create_export` started.

use crucible_api_types::ProblemCode;
use crucible_records::ExportJobId;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::error::ToolError;
use crate::tools::create_export::{ExportView, service, store_error, view};
use crate::tools::{Call, Check, Tool};

pub(crate) struct GetExport;

/// Which export.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Args {
    /// The export id that create_export answered with.
    id: String,
}

impl Tool for GetExport {
    const NAME: &'static str = "get_export";
    const DESCRIPTION: &'static str = "One export of the project: its state (`queued`, `running`, `ready`, \
        `failed` or `expired`), the spans and bytes written so far, and once it is ready its checksum, expiry and \
        the REST `download_path` to fetch the file from with the same key. A failed export names its `error_code`.";
    type Args = Args;
    type Output = ExportView;

    async fn call(call: &Call<'_>, args: Args) -> Result<ExportView, ToolError> {
        let mut check = Check::default();
        let id = ExportJobId::parse(&args.id).ok();
        if id.is_none() {
            check.error("id", "invalid_id", "an export id is a UUID, as create_export answered it");
        }
        check.finish()?;
        let Some(id) = id else {
            return Err(ToolError::new(ProblemCode::ValidationFailed, "the export id is invalid"));
        };
        let exports = service(call)?;
        let job = exports
            .get(call.ctx.scope(), id)
            .await
            .map_err(|error| store_error(&error))?
            .ok_or_else(|| ToolError::new(ProblemCode::NotFound, "no export of the project has this id"))?;
        Ok(view(exports, &job))
    }
}
