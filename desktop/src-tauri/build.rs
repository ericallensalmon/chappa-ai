fn main() {
  // cargo test binaries don't receive tauri-build's application manifest
  // (it rides RT_MANIFEST on app binaries only), so on windows-msvc the
  // loader binds WinSxS comctl32 5.82 and the exe dies at load with
  // STATUS_ENTRYPOINT_NOT_FOUND — tauri's windowing imports need
  // Common-Controls 6. Merge the v6 dependency into test targets too.
  if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
    println!(
      "cargo:rustc-link-arg-tests=/MANIFESTDEPENDENCY:type='win32' \
       name='Microsoft.Windows.Common-Controls' version='6.0.0.0' \
       processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
    );
  }
  tauri_build::build()
}
