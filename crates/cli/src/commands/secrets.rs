use mill_config::{SecretListItem, SecretResponse, SecretSetRequest};

use crate::args::SecretAction;
use crate::client::MillClient;
use crate::error::CliError;
use crate::output::table;
use crate::output::{self, OutputMode};

pub async fn secret(
    client: &MillClient,
    action: SecretAction,
    mode: OutputMode,
) -> Result<(), CliError> {
    match action {
        SecretAction::Set { name, value } => {
            let path = format!("/v1/secrets/{name}");
            let body = SecretSetRequest { value };
            client.put_json(&path, &body).await?;
            if mode == OutputMode::Human {
                eprintln!("secret {name} set");
            }
        }
        SecretAction::Get { name } => {
            let path = format!("/v1/secrets/{name}");
            let secret: SecretResponse = client.get(&path).await?;
            output::print(mode, &secret, |s| {
                println!("{}", s.value);
            });
        }
        SecretAction::List => {
            let secrets: Vec<SecretListItem> = client.get("/v1/secrets").await?;
            output::print_list(mode, &secrets, |secrets| {
                table::print_secrets(secrets);
            });
        }
        SecretAction::Delete { name } => {
            let path = format!("/v1/secrets/{name}");
            client.delete(&path).await?;
            if mode == OutputMode::Human {
                eprintln!("secret {name} deleted");
            }
        }
    }

    Ok(())
}
