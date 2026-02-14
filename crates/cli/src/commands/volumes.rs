use mill_config::VolumeResponse;

use crate::args::VolumeAction;
use crate::client::MillClient;
use crate::error::CliError;
use crate::output::table;
use crate::output::{self, OutputMode};

pub async fn volume(
    client: &MillClient,
    action: VolumeAction,
    mode: OutputMode,
) -> Result<(), CliError> {
    match action {
        VolumeAction::List => {
            let volumes: Vec<VolumeResponse> = client.get("/v1/volumes").await?;
            output::print_list(mode, &volumes, |volumes| {
                table::print_volumes(volumes);
            });
        }
        VolumeAction::Delete { name } => {
            let path = format!("/v1/volumes/{name}");
            client.delete(&path).await?;
            if mode == OutputMode::Human {
                eprintln!("volume {name} deleted");
            }
        }
    }

    Ok(())
}
