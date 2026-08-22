use std::ffi::OsStr;

#[tokio::main]
async fn main() {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let command = arguments.next();
    let option = arguments.next();
    if arguments.next().is_some() {
        fail();
    }

    match (command.as_deref(), option.as_deref()) {
        (Some(command), None) if command == OsStr::new("credential") => {
            recover_credential(false).await;
        }
        (Some(command), Some(option))
            if command == OsStr::new("credential")
                && option == OsStr::new("--reset-if-unavailable") =>
        {
            recover_credential(true).await;
        }
        (Some(command), None) if command == OsStr::new("activate") => {
            if dark_factory_control_plane::activate_runtime_from_env()
                .await
                .is_err()
            {
                eprintln!("control-plane runtime activation failed");
                std::process::exit(1);
            }
            eprintln!("control-plane runtime activation and verification succeeded");
        }
        _ => fail(),
    }
}

async fn recover_credential(reset_if_unavailable: bool) {
    let recovered =
        dark_factory_control_plane::recover_runtime_credential_from_env(reset_if_unavailable).await;
    let recovered = match recovered {
        Ok(recovered) => recovered,
        Err(dark_factory_control_plane::ProvisionError::PasswordUnavailable) => {
            eprintln!(
                "Neon has no stored runtime password; an explicit reset decision is required"
            );
            std::process::exit(1);
        }
        Err(_) => {
            eprintln!("control-plane runtime credential recovery failed");
            std::process::exit(1);
        }
    };
    if recovered.reset_was_performed() {
        eprintln!("new runtime credential accepted for immediate sensitive staging");
    } else {
        eprintln!("existing runtime credential recovered for sensitive staging");
    }
    println!("{}", recovered.database_url());
}

fn fail() -> ! {
    eprintln!(
        "usage: runtime-bootstrap credential [--reset-if-unavailable] | runtime-bootstrap activate"
    );
    std::process::exit(2);
}
