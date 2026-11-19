//! Display primitives shared by the TUI and the log lines the tools write.
//!
//! The token-count compaction lives here rather than in `app.rs` because the
//! server tools format their own numbers the same way: a tool row that says
//! `18.43k chars` has to agree with the dashboard's `18.43k`, or one of the
//! two is misreporting the same number.
//!
//! The outcome words are the request log's vocabulary for "how did this tool
//! call end". The tools write them and the TUI colours them, so both sides
//! read them from here.

/// Placeholder for a cell with nothing to show yet. An em dash reads as
/// "no data" where a zero would read as "zero tokens", which is a
/// different (and misleading) claim for a model that has not been used.
pub const DASH: &str = "\u{2014}";

/// The call succeeded.
pub const OUTCOME_OK: &str = "ok";
/// The tool ran and failed.
pub const OUTCOME_ERROR: &str = "error";
/// A limit declined the call before it ran.
pub const OUTCOME_REFUSED: &str = "refused";
/// The call partly worked: some of it did, some of it did not.
pub const OUTCOME_PARTIAL: &str = "partial";

/// Format a token count compactly for the dashboard.
///
/// Counts below one thousand remain integer counts so small values do not
/// gain needless decimal places. Larger counts use decimal (base-1000) units
/// and are rounded to two decimal places. If rounding would produce
/// `1000.00` of one unit, the count is promoted to the next unit instead.
pub fn format_token_count(count: u64) -> String {
    const UNITS: [&str; 6] = ["", "k", "m", "b", "t", "q"];

    if count < 1_000 {
        return count.to_string();
    }

    let mut unit_index = 0;
    let mut scaled = count as f64;
    while scaled >= 1_000.0 && unit_index + 1 < UNITS.len() {
        scaled /= 1_000.0;
        unit_index += 1;
    }

    // Avoid output such as `1000.00k` when rounding carries into the next
    // unit. This also keeps values near a unit boundary intuitive.
    while scaled >= 999.995 && unit_index + 1 < UNITS.len() {
        scaled /= 1_000.0;
        unit_index += 1;
    }

    format!("{scaled:.2}{}", UNITS[unit_index])
}

#[cfg(test)]
mod tests;
