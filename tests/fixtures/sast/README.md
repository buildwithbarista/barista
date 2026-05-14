# SAST fixtures

Test inputs for the SAST suite (`.semgrep/`, CodeQL, the unsafe-line
ratchet). Each fixture is designed to either trip a specific rule or
serve as the clean counter-example for the same rule. The pair lets CI
prove both halves of the round trip: the rule fires on a known-bad
shape, and it does *not* fire on the corresponding well-formed shape.

## Inventory

| File | Asserts |
|---|---|
| `synthetic_command_injection.rs` | `.semgrep/barista-rust.yml :: barista-rust-unchecked-command-new` fires on raw `Command::new(user_input)` |
| `clean_command_new.rs` | The same rule does **not** fire on a hard-coded literal program name |
| `synthetic_from_utf8_unchecked.rs` | `barista-rust-string-from-utf8-unchecked` fires on `String::from_utf8_unchecked(...)` |
| `synthetic_transmute.rs` | `barista-rust-transmute` fires on `mem::transmute(...)` outside the cache crate |
| `synthetic_sql_injection.java` | CodeQL's `java/sql-injection` rule fires on a concatenated SQL string |

## Why `.rs` / `.java` and not a sentinel extension?

Semgrep and CodeQL identify a file's language from its extension; if
the fixtures used a custom extension they'd be silently skipped.
Cargo and javac don't see these files because they live outside any
crate's source tree (`crates/<name>/src/**`) and outside `barback/src/**`.
The `tests/fixtures/sast/` directory exists purely as a scanner input.

## Adding a fixture

1. Pick a stable, descriptive filename: `synthetic_<rule-shorthand>.<ext>`
   for a violation, or `clean_<rule-shorthand>.<ext>` for the negative
   case. Use the real language extension (`.rs`, `.java`, etc.) so
   Semgrep/CodeQL can identify the file's language.
2. Inline the smallest snippet that exercises exactly the pattern the
   rule matches — no extra noise.
3. Add a one-line top comment naming the rule the fixture targets, so
   future readers can trace the fixture back to its assertion.
4. Update the table above.
5. Wire it into the round-trip test for the scanner (`scripts/test-sast.sh`
   or the SAST workflow's verify step).

## Hygiene

Fixtures must not contain anything that triggers other security
scanners by accident — no real secrets, no real CVE-bearing dep
references, no third-party copyrighted code. If a fixture needs to look
*like* a secret to trip a scanner, follow the same hygiene rules as
`tests/fixtures/secrets/`: clearly shaped-but-fake, documented in this
README, and not a valid credential anywhere.
