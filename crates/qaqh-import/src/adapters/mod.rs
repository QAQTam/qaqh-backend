mod claude_code;

use crate::{
    error::Result,
    model::{CapturedSource, ImportDiagnostic, OperationContext, PreparedImport},
};

pub(super) fn normalize_claude(
    captured: &CapturedSource,
    context: &OperationContext,
) -> Result<(PreparedImport, Vec<ImportDiagnostic>)> {
    claude_code::normalize(captured, context)
}

pub(crate) use claude_code::compute_normalization_hash;
