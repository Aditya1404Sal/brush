//! Environment variable retrieval for platforms without a native implementation (such as WASI).

/// Retrieves environment variables from the host process.
///
/// WASI hands a component its environment like any process gets one, so this reads it through
/// the standard library; variables whose name or value is not UTF-8 are skipped.
pub(crate) fn get_host_env_vars() -> impl Iterator<Item = (String, String)> {
    std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
}
