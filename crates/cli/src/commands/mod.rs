mod cluster;
mod deploy;
mod logs;
mod nodes;
mod secrets;
mod status;
mod tasks;
mod volumes;

use crate::args::Command;
use crate::client::MillClient;
use crate::error::CliError;
use crate::output::OutputMode;

pub async fn dispatch(
    cmd: Command,
    client: &MillClient,
    mode: OutputMode,
    data_dir: &str,
) -> Result<(), CliError> {
    match cmd {
        Command::Init { token, advertise, provider, provider_token, acme_email, acme_staging } => {
            cluster::init(
                data_dir,
                token,
                advertise,
                provider,
                provider_token,
                acme_email,
                acme_staging,
            )
            .await
        }

        Command::Join {
            address,
            token,
            advertise,
            provider,
            provider_token,
            acme_email,
            acme_staging,
        } => {
            cluster::join(
                data_dir,
                &address,
                &token,
                advertise,
                provider,
                provider_token,
                acme_email,
                acme_staging,
            )
            .await
        }

        Command::Leave => cluster::leave(data_dir).await,

        Command::Deploy { file } => deploy::deploy(client, &file, mode).await,

        Command::Restart { service } => deploy::restart(client, &service, mode).await,

        Command::Spawn { task, image, cpu, memory, timeout, envs } => {
            let args = tasks::SpawnArgs { task, image, cpu, memory, timeout, envs };
            tasks::spawn(client, args, mode).await
        }

        Command::Ps => tasks::ps(client, mode).await,

        Command::Kill { task_id } => tasks::kill(client, &task_id).await,

        Command::Status => status::status(client, mode).await,

        Command::Logs { name, follow, tail, all } => {
            logs::logs(client, name, follow, tail, all, mode).await
        }

        Command::Secret { action } => secrets::secret(client, action, mode).await,

        Command::Volume { action } => volumes::volume(client, action, mode).await,

        Command::Nodes => nodes::nodes(client, mode).await,

        Command::Drain { node_id } => nodes::drain(client, &node_id, mode).await,

        Command::Remove { node_id } => nodes::remove(client, &node_id).await,
    }
}
