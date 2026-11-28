/// Remove ambient credentials, proxy settings and configuration overrides while
/// retaining the environment needed by the OS to run a subprocess on Windows.
/// Callers must provide isolated HOME/USERPROFILE and application directories.
pub fn clear_for_test(command: &mut std::process::Command) {
    command.env_clear();
    for name in ["SystemRoot", "WINDIR", "PATH", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
}
