use mill_config::DataDir;
use mill_node::daemon;

use crate::error::CliError;

pub async fn init(
    data_dir: &str,
    token: Option<String>,
    advertise: Option<String>,
    provider: Option<String>,
    provider_token: Option<String>,
    acme_email: Option<String>,
    acme_staging: bool,
) -> Result<(), CliError> {
    let data_dir = DataDir::new(data_dir);
    daemon::init(&data_dir, token, advertise, provider, provider_token, acme_email, acme_staging)
        .await
        .map_err(|e| CliError::Arg(e.to_string()))
}

// TODO(refactor): group init/join params into an opts struct
#[allow(clippy::too_many_arguments)]
pub async fn join(
    data_dir: &str,
    address: &str,
    token: &str,
    advertise: Option<String>,
    provider: Option<String>,
    provider_token: Option<String>,
    acme_email: Option<String>,
    acme_staging: bool,
) -> Result<(), CliError> {
    let data_dir = DataDir::new(data_dir);
    daemon::join(
        &data_dir,
        address,
        token,
        advertise,
        provider,
        provider_token,
        acme_email,
        acme_staging,
    )
    .await
    .map_err(|e| CliError::Arg(e.to_string()))
}

pub async fn leave(data_dir: &str) -> Result<(), CliError> {
    let data_dir = DataDir::new(data_dir);
    daemon::leave(&data_dir).await.map_err(|e| CliError::Arg(e.to_string()))
}
