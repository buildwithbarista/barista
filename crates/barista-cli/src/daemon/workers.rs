//! Worker-count expression resolver for `barback.default_workers`.
//!
//! The daemon's worker-pool concurrency budget (M4.2 T2 / PRD §11.2.2)
//! is configurable as either a literal positive integer or a "core
//! multiplier" expression of the form `<float>C`:
//!
//! * `"1"`, `"2"`, … — literal positive integer.
//! * `"1C"` — one worker per available CPU core.
//! * `"0.75C"` — 75 % of available cores, rounded up to the nearest
//!   integer.
//! * `"2C"` — twice the available cores.
//!
//! The expression is resolved at daemon-spawn time on the CLI side
//! (here) before the integer value is passed to barback's
//! `--workers` flag. Resolving here rather than inside the daemon
//! lets the CLI surface a `BAR-CONFIG-WORKERS-MALFORMED` error
//! immediately on bad input, rather than after a JVM start cost.
//!
//! The expression set deliberately stays small in v0.1; PRD §11.2.2
//! reserves room for `<int>%` (percentage of cores) in v0.2 if a
//! user-study shows the `C`-suffix form is unintuitive. Adding a new
//! variant is an additive change.

use std::num::NonZeroUsize;

/// Errors produced by [`resolve_workers`].
#[derive(Debug, thiserror::Error)]
pub enum WorkersError {
    /// The expression couldn't be parsed.
    #[error(
        "malformed `barback.default_workers` expression {expr:?}: \
         expected a positive integer (e.g. `4`) or a core-multiplier \
         (e.g. `1C` / `0.75C`); detail: {detail}"
    )]
    Malformed {
        /// The raw expression as received from configuration / CLI.
        expr: String,
        /// Parser-side detail.
        detail: String,
    },
}

/// Resolve a `barback.default_workers` expression to a concrete
/// positive integer.
///
/// `available_cores` is the number of CPU cores the host has — usually
/// `std::thread::available_parallelism().map(NonZeroUsize::get)`. Passing
/// it explicitly (rather than calling the API inside this function)
/// keeps the resolver pure and unit-testable on a fixed-core fixture.
///
/// Returns the number of workers to ask for, clamped to >= 1.
///
/// # Examples
///
/// ```ignore
/// use std::num::NonZeroUsize;
/// use barista_cli::daemon::workers::resolve_workers;
/// let cores = NonZeroUsize::new(8).unwrap();
/// assert_eq!(resolve_workers("1C", cores).unwrap(), 8);
/// assert_eq!(resolve_workers("0.75C", cores).unwrap(), 6);
/// assert_eq!(resolve_workers("4", cores).unwrap(), 4);
/// ```
pub fn resolve_workers(expr: &str, available_cores: NonZeroUsize) -> Result<usize, WorkersError> {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return Err(WorkersError::Malformed {
            expr: expr.to_string(),
            detail: "expression is empty".to_string(),
        });
    }

    // Branch on the trailing `C`/`c`: core-multiplier form.
    if let Some(num_part) = trimmed
        .strip_suffix('C')
        .or_else(|| trimmed.strip_suffix('c'))
    {
        let multiplier: f64 =
            num_part
                .parse()
                .map_err(|e: std::num::ParseFloatError| WorkersError::Malformed {
                    expr: expr.to_string(),
                    detail: format!("core-multiplier prefix `{num_part}` is not a number: {e}"),
                })?;
        if !multiplier.is_finite() || multiplier <= 0.0 {
            return Err(WorkersError::Malformed {
                expr: expr.to_string(),
                detail: format!("core-multiplier must be positive and finite, got {multiplier}"),
            });
        }
        // `.ceil()` rather than `.round()`: `0.75C` on a 4-core host
        // produces 3 (not 4), and `0.1C` on an 8-core host produces 1
        // (not 0). The clamp below guarantees >=1 regardless.
        let cores = available_cores.get() as f64;
        let raw = (cores * multiplier).ceil();
        // `as usize` truncates; we've bounded `raw >= 0` via the
        // positivity check, and `f64::MAX` is way outside any sane
        // worker count, so a clamp into [1, isize::MAX] is sufficient.
        #[allow(
            clippy::as_conversions,
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation
        )]
        let count = raw.max(1.0).min(isize::MAX as f64) as usize;
        return Ok(count.max(1));
    }

    // Literal-integer form.
    let count: usize =
        trimmed
            .parse()
            .map_err(|e: std::num::ParseIntError| WorkersError::Malformed {
                expr: expr.to_string(),
                detail: format!("literal-integer parse failed: {e}"),
            })?;
    if count == 0 {
        return Err(WorkersError::Malformed {
            expr: expr.to_string(),
            detail: "literal-integer must be >= 1".to_string(),
        });
    }
    Ok(count)
}

/// Lookup [`std::thread::available_parallelism`] with a guaranteed-1
/// fallback when the platform refuses to answer (containerized
/// environments with `sched_setaffinity` weirdness, etc.).
///
/// Wrapped in a helper so `resolve_workers` stays pure and tests can
/// supply a fixed value.
#[must_use]
pub fn available_parallelism_or_one() -> NonZeroUsize {
    std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::unwrap_used)]
    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[test]
    fn literal_integer_passes_through() {
        assert_eq!(resolve_workers("4", nz(8)).unwrap(), 4);
        assert_eq!(resolve_workers("1", nz(1)).unwrap(), 1);
        assert_eq!(resolve_workers(" 12 ", nz(8)).unwrap(), 12);
    }

    #[test]
    fn one_c_equals_core_count() {
        assert_eq!(resolve_workers("1C", nz(8)).unwrap(), 8);
        assert_eq!(resolve_workers("1c", nz(8)).unwrap(), 8);
    }

    #[test]
    fn fractional_c_rounds_up() {
        // 0.75 * 8 = 6.0; ceil = 6.
        assert_eq!(resolve_workers("0.75C", nz(8)).unwrap(), 6);
        // 0.5 * 5 = 2.5; ceil = 3.
        assert_eq!(resolve_workers("0.5C", nz(5)).unwrap(), 3);
    }

    #[test]
    fn tiny_multiplier_clamps_to_one() {
        // 0.1 * 1 = 0.1; ceil = 1.
        assert_eq!(resolve_workers("0.1C", nz(1)).unwrap(), 1);
    }

    #[test]
    fn two_c_doubles() {
        assert_eq!(resolve_workers("2C", nz(4)).unwrap(), 8);
    }

    #[test]
    fn empty_is_malformed() {
        let err = resolve_workers("", nz(4)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("empty"), "msg = {msg}");
    }

    #[test]
    fn negative_is_malformed() {
        let err = resolve_workers("-1C", nz(4)).unwrap_err();
        assert!(err.to_string().contains("positive"));
    }

    #[test]
    fn zero_literal_is_malformed() {
        // ParseIntError doesn't catch zero — it's our explicit zero
        // check that fires. The message contains ">= 1".
        let err = resolve_workers("0", nz(4)).unwrap_err();
        assert!(err.to_string().contains(">= 1"));
    }

    #[test]
    fn garbage_is_malformed() {
        let err = resolve_workers("hello", nz(4)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("integer parse failed") || msg.contains("malformed"));
    }
}
