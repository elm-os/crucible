// Fixture for the allow-attribute guard check in check.py: a file that switches off a determinism lint.
// The check copies it to a path outside the guard's allowlist and expects the guard to flag it.
#[expect(clippy::disallowed_methods)]
pub fn ambient_now() -> std::time::Instant {
    std::time::Instant::now()
}
