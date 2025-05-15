/// Utility function for resolving component and provider references
pub async fn resolve_ref(s: impl AsRef<str>) -> Result<String> {
    let resolved = match s.as_ref() {
        s if s.starts_with('/') => {
            format!("file://{}", &s) // prefix with file:// if it's an absolute path
        }
        s if tokio::fs::try_exists(s).await.is_ok_and(|exists| exists) => {
            format!(
                "file://{}",
                tokio::fs::canonicalize(&s)
                    .await
                    .with_context(|| format!("failed to resolve absolute path: {s}"))?
                    .display()
            )
        }
        // If a URI-formatted relative path was provided, resolve it
        s if s.starts_with("file://")
            && tokio::fs::try_exists(s.split_at(7).1)
                .await
                .is_ok_and(|exists| exists) =>
        {
            format!(
                "file://{}",
                tokio::fs::canonicalize(s.split_at(7).1)
                    .await
                    .with_context(|| format!("failed to resolve absolute path: {s}"))?
                    .display()
            )
        }
        // For all other cases, just take the provided string
        s => s.to_string(),
    };
    Ok(resolved)
}
