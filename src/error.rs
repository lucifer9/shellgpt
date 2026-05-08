pub const ERR_NO_PREVIOUS: &str = "No previous sgpt conversation exists in this session.\nRun sgpt \"...\" first, then sgpt -c \"...\" to continue.";
pub const ERR_BODY_TOO_LARGE: &str = "Request body too large.";
pub const ERR_BUSY: &str = "Another sgpt request is already in progress.";
pub const ERR_AI_NO_TEXT: &str = "AI response did not contain text message content.";
pub const ERR_AI_BODY_TOO_LARGE: &str = "AI response body exceeded 2 MiB limit.";
pub const ERR_AI_ANSWER_TOO_LARGE: &str = "AI response exceeded 512 KiB limit.";
