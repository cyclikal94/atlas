use anyhow::{Context, Result, ensure};

/// Secret values may come from a mounted file; ambiguous configuration fails closed.
pub fn secret(name: &str) -> Result<Option<String>> {
    let value = std::env::var(name).ok();
    let file = std::env::var(format!("{name}_FILE")).ok();
    ensure!(
        value.is_none() || file.is_none(),
        "set {name} or {name}_FILE, not both"
    );
    match file {
        Some(path) => {
            let value = std::fs::read_to_string(path)
                .with_context(|| format!("cannot read {name}_FILE"))?;
            let value = value.trim_end_matches(['\r', '\n']);
            ensure!(!value.is_empty(), "{name}_FILE is empty");
            Ok(Some(value.to_owned()))
        }
        None => Ok(value),
    }
}
