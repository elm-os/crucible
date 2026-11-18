//! Placeholder.

use anyhow::Result;

use crate::Ctx;

pub(crate) async fn reads(_ctx: &Ctx) -> Result<String> {
    Ok(String::new())
}
