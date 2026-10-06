pub fn init() {
    // Dependencies may log complete requests or message bodies. Only our explicitly
    // structured events are enabled, regardless of RUST_LOG.
    use tracing_subscriber::prelude::*;
    let filter =
        tracing_subscriber::filter::Targets::new().with_target("nestbot", tracing::Level::INFO);
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().json().with_filter(filter))
        .init();
}

pub fn safe_error(error: &anyhow::Error) -> &'static str {
    let code = error.to_string();
    match code.as_str() {
        "cancelled" => "cancelled",
        "queue_full" => "queue_full",
        "invalid_password" => "invalid_password",
        "main_account_not_logged_in" => "main_account_not_logged_in",
        "missing_api_credentials" => "missing_api_credentials",
        "missing_target" => "missing_target",
        "disk_full" => "disk_full",
        "file_too_large" => "file_too_large",
        "transfer_uncertain" => "transfer_uncertain",
        "claim_incomplete" => "claim_incomplete",
        "telegram_timeout" => "telegram_timeout",
        "telegram_retries_exhausted" => "telegram_retries_exhausted",
        "search_no_results" => "search_no_results",
        "search_partial_timeout" => "search_partial_timeout",
        "invalid_config" => "invalid_config",
        "service_already_running" => "service_already_running",
        "vault_password_required" => "vault_password_required",
        "account_session_in_use" => "account_session_in_use",
        "config_not_found" => "config_not_found",
        "service_not_running" => "service_not_running",
        "admin_password_required" => "admin_password_required",
        "admin_password_too_short" => "admin_password_too_short",
        "invalid_keyword" => "invalid_keyword",
        "invalid_keys" => "invalid_keys",
        "missing_keys" => "missing_keys",
        "invalid_range" => "invalid_range",
        "batch_not_found" => "batch_not_found",
        "invalid_batch_progress" => "invalid_batch_progress",
        "batch_busy" => "batch_busy",
        "no_previous_batch" => "no_previous_batch",
        "invalid_sort" => "invalid_sort",
        "invalid_pages" => "invalid_pages",
        "invalid_target" => "invalid_target",
        "target_not_found" => "target_not_found",
        "job_not_retryable" => "job_not_retryable",
        "mtproto_requires_socks5_proxy" => "mtproto_requires_socks5_proxy",
        "source_media_missing" => "source_media_missing",
        "login_failed" => "login_failed",
        "telegram_failed" => "telegram_failed",
        "upload_failed" => "upload_failed",
        "invalid_preference" => "invalid_preference",
        "invalid_mode" => "invalid_mode",
        "caption_too_long" => "caption_too_long",
        "tag_edit_failed" => "tag_edit_failed",
        "backup_exists" => "backup_exists",
        _ => "operation_failed",
    }
}
