use std::time::Duration;

use reqwest::StatusCode;

/// Gives direct 5xx responses two retries before paying SSH startup costs.
/// Access blocks still switch immediately; server errors on SSH stay on that route.
pub(crate) fn should_try_ssh_after_status(
	status: StatusCode,
	attempt: usize,
	using_ssh: bool,
) -> bool {
	matches!(
		status,
		StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
	) || (status.is_server_error() && !using_ssh && attempt >= 2)
}

/// Formats an exact retry delay followed by an easier-to-scan approximation.
pub(crate) fn format_retry_delay(duration: Duration) -> String {
	let seconds = duration.as_secs();
	if seconds < 60 {
		let unit = if seconds == 1 { "second" } else { "seconds" };
		return format!("{seconds} {unit}");
	}

	let hours = seconds / 3600;
	let minutes = (seconds % 3600) / 60;
	if seconds.is_multiple_of(3600) {
		let unit = if hours == 1 { "hour" } else { "hours" };
		return format!("{seconds} seconds ({hours} {unit})");
	}
	if hours > 0 {
		let approximation = if minutes > 0 {
			format!("{hours}h {minutes}m")
		} else {
			format!("{hours}h")
		};
		return format!("{seconds} seconds (around {approximation})");
	}

	if seconds.is_multiple_of(60) {
		let unit = if minutes == 1 { "minute" } else { "minutes" };
		format!("{seconds} seconds ({minutes} {unit})")
	} else {
		format!("{seconds} seconds (around {minutes}m)")
	}
}
