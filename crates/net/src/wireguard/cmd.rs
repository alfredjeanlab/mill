use tokio::process::Command;

use crate::error::NetError;

/// Run an external command and return its stdout on success.
pub async fn run_command(program: &str, args: &[&str]) -> Result<String, NetError> {
    let output = Command::new(program).args(args).output().await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            NetError::CommandNotFound { program: program.to_string() }
        } else {
            NetError::Io(e)
        }
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(NetError::Command {
            program: program.to_string(),
            message: stderr.trim().to_string(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Run an external command, piping data to its stdin.
pub async fn run_command_stdin(
    program: &str,
    args: &[&str],
    stdin_data: &str,
) -> Result<String, NetError> {
    use tokio::io::AsyncWriteExt;

    let mut child = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                NetError::CommandNotFound { program: program.to_string() }
            } else {
                NetError::Io(e)
            }
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(stdin_data.as_bytes()).await?;
    }

    let output = child.wait_with_output().await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(NetError::Command {
            program: program.to_string(),
            message: stderr.trim().to_string(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Generate a WireGuard keypair. Returns `(private_key, public_key)`.
pub async fn generate_keypair() -> Result<(String, String), NetError> {
    let private_key = run_command("wg", &["genkey"]).await?;
    let public_key = run_command_stdin("wg", &["pubkey"], &private_key).await?;
    Ok((private_key, public_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_command_echo() {
        let output = run_command("echo", &["hello"]).await;
        assert!(output.is_ok());
        assert_eq!(output.unwrap_or_default(), "hello");
    }

    #[tokio::test]
    async fn run_command_true() {
        let output = run_command("true", &[]).await;
        assert!(output.is_ok());
    }

    #[tokio::test]
    async fn run_command_not_found() {
        let output = run_command("nonexistent-binary-xyz", &[]).await;
        assert!(matches!(output, Err(NetError::CommandNotFound { .. })));
    }

    #[tokio::test]
    async fn run_command_failure() {
        let output = run_command("false", &[]).await;
        assert!(matches!(output, Err(NetError::Command { .. })));
    }
}
