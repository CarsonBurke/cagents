//! Explicit physical-device image cleanup, independent of daemon routing.
use super::*;

#[derive(Debug, clap::Subcommand)]
pub enum ImageAssetCommand {
    /// List private image receipts and quota usage; never prints image bytes or source paths.
    List,
    /// Remove one exact image receipt. Drafts and saved history may lose that image.
    Remove {
        /// Exact image receipt UUID from image-assets list.
        id: uuid::Uuid,
        /// Exact lowercase SHA-256 from image-assets list; refuses a stale or mismatched receipt.
        #[arg(long)]
        sha256: String,
        /// Explicitly acknowledge that drafts and saved history may lose this image.
        #[arg(long)]
        acknowledge_history_loss: bool,
    },
}

fn with_local<T>(
    device: Option<&str>,
    remote_selected: bool,
    hosted: bool,
    operation: impl FnOnce() -> Result<T, AppError>,
) -> Result<T, AppError> {
    if hosted {
        return Err(AppError::new(
            "image_assets_user_only",
            "Image asset management requires the user's local CLI, outside a hosted agent session",
        ));
    }
    if remote_selected || device.is_some_and(|device| device != "local") {
        return Err(AppError::new(
            "image_assets_local_only",
            "Image assets belong to this physical device; omit --device or use --device local",
        ));
    }
    operation()
}

pub(in crate::control) fn execute(
    command: &ImageAssetCommand,
    device: Option<&str>,
    remote_selected: bool,
    hosted: bool,
) -> Result<CommandOutput, AppError> {
    with_local(device, remote_selected, hosted, || match command {
        ImageAssetCommand::List => {
            let inventory = crate::chat::attachment_assets::list()
                .map_err(|error| AppError::new("image_assets", error))?;
            let mut images = inventory.images.iter().collect::<Vec<_>>();
            images.sort_by_key(|image| image.number);
            let rows = images
                .into_iter()
                .map(|image| {
                    [
                        format!("Image {}", image.number),
                        image.id.to_string(),
                        image.bytes.to_string(),
                        if image.retained { "yes" } else { "no" }.into(),
                        image.sha256.clone(),
                    ]
                })
                .collect();
            let table = format::table(
                ["IMAGE", "UUID", "BYTES", "RETAINED", "SHA256"],
                rows,
                "No image assets on this physical device",
            );
            Ok(CommandOutput {
                command: "chat.image-assets.list",
                human: format!(
                    "{table}\n{} / {} images; {} / {} bytes. Removal is explicit and may make draft/history images unavailable.",
                    inventory.images.len(),
                    inventory.max_images,
                    inventory.bytes,
                    inventory.max_bytes
                ),
                data: json!(inventory),
            })
        }
        ImageAssetCommand::Remove {
            id,
            sha256,
            acknowledge_history_loss,
        } => {
            crate::chat::attachment_assets::remove(*id, sha256, *acknowledge_history_loss)
                .map_err(|error| AppError::new("image_assets", error))?;
            Ok(CommandOutput {
                command: "chat.image-assets.remove",
                human: format!(
                    "Removed image {id}. Drafts and saved history may no longer display it."
                ),
                data: json!({"id":id,"removed":true,"history_loss_acknowledged":true}),
            })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosted_and_remote_contexts_refuse_before_any_storage_operation() {
        for (device, remote, hosted, expected) in [
            (None, false, true, "image_assets_user_only"),
            (Some("local"), false, true, "image_assets_user_only"),
            (
                Some("missing-remote"),
                false,
                false,
                "image_assets_local_only",
            ),
            (None, true, false, "image_assets_local_only"),
            (Some("local"), true, false, "image_assets_local_only"),
        ] {
            let mut accessed = false;
            let error = with_local(device, remote, hosted, || {
                accessed = true;
                Ok(())
            })
            .unwrap_err();
            assert_eq!(error.kind, expected);
            assert!(
                !accessed,
                "refusal must precede opening/initializing private image storage"
            );
        }
        for device in [None, Some("local")] {
            assert_eq!(with_local(device, false, false, || Ok(7)).unwrap(), 7);
        }
    }

    #[test]
    fn image_asset_commands_parse_exact_identity_and_explicit_loss_acknowledgment() {
        let id = uuid::Uuid::new_v4().to_string();
        let digest = "a".repeat(64);
        let cli = Cli::try_parse_from([
            "cagents",
            "--device",
            "local",
            "chat",
            "image-assets",
            "remove",
            &id,
            "--sha256",
            &digest,
            "--acknowledge-history-loss",
        ])
        .unwrap();
        assert!(
            matches!(cli.command, TopCommand::Chat { command: ChatCommand::ImageAssets { command: ImageAssetCommand::Remove { id: parsed, sha256, acknowledge_history_loss:true } } } if parsed.to_string() == id && sha256 == digest)
        );
        assert!(
            Cli::try_parse_from([
                "cagents",
                "chat",
                "image-assets",
                "remove",
                "../../arbitrary-file",
                "--sha256",
                &digest,
                "--acknowledge-history-loss"
            ])
            .is_err()
        );
    }

    #[cfg(any(feature = "host", feature = "ui"))]
    #[test]
    fn local_cli_reports_only_safe_metadata_and_removes_exact_acknowledged_receipt() {
        crate::chat::attachment_assets::with_test_image(|image| {
            let reference = image.reference.as_ref().unwrap();
            let listed = execute(&ImageAssetCommand::List, None, false, false).unwrap();
            let serialized = serde_json::to_string(&listed.data).unwrap();
            assert!(!serialized.contains(image.path.to_str().unwrap()));
            assert!(!serialized.contains(&reference.name));
            assert!(listed.human.contains(&reference.id.to_string()));
            assert!(listed.human.contains(&reference.sha256));
            let command = |digest, acknowledgment| ImageAssetCommand::Remove {
                id: reference.id,
                sha256: digest,
                acknowledge_history_loss: acknowledgment,
            };
            assert!(
                execute(
                    &command(reference.sha256.clone(), false),
                    None,
                    false,
                    false
                )
                .is_err()
            );
            assert!(execute(&command("b".repeat(64), true), None, false, false).is_err());
            assert!(image.path.exists());
            let removed =
                execute(&command(reference.sha256.clone(), true), None, false, false).unwrap();
            assert_eq!(removed.data["removed"], true);
            assert!(!image.path.exists());
            assert!(
                crate::chat::attachment_assets::list()
                    .unwrap()
                    .images
                    .is_empty()
            );
            let next = crate::chat::attachment_assets::test_image("new.png");
            assert!(next.reference.unwrap().number > reference.number);
        });
    }
}
