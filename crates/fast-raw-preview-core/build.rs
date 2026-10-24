// napi-rs build glue. Only invoked when the `napi` feature is on; the
// bench binary builds with --no-default-features and skips this entirely.
fn main() {
  #[cfg(feature = "napi")]
  napi_build::setup();
}
