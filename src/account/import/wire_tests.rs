//! Exercise the actual shared Postcard envelope, not a JSON-only fake peer.
use super::*;
use crate::{
    device_service::{Operation, Reply},
    model::{BootId, HostIncarnation, HostSessionId},
    terminal_ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
        ServerResponse,
    },
};

fn proof(name: &str) -> crate::model::HostSessionProof {
    crate::model::HostSessionProof {
        host_session_id: HostSessionId::new(name),
        boot_id: BootId::new("fixture-boot"),
        host_incarnation: HostIncarnation::new(name),
    }
}
fn plan(profile: CodingProfile) -> ImportPlan {
    ImportPlan {
        operation: Uuid::from_u128(1),
        destination: proof("original-daemon"),
        observation: SourceObservation {
            path: "/private/staged/auth.json".into(),
            parent: PhysicalIdentity {
                device: 1,
                inode: 2,
            },
            file: PhysicalIdentity {
                device: 1,
                inode: 3,
            },
            size: 128,
            mtime_seconds: 1,
            mtime_nanoseconds: 2,
            ctime_seconds: 3,
            ctime_nanoseconds: 4,
        },
        expected_email: "fixture@example.test".into(),
        consent: CopyConsent {
            staged_file_only: true,
            source_session_limit_acknowledged: true,
        },
        profile,
    }
}
fn roundtrip<T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(
    value: &T,
) -> T {
    let bytes = postcard::to_stdvec(value).expect("actual shared binary serialization");
    let decoded = postcard::from_bytes::<T>(&bytes).expect("actual shared binary deserialization");
    assert_eq!(&decoded, value);
    decoded
}
fn client(operation: ImportOperation, proof: crate::model::HostSessionProof) -> ClientEnvelope {
    ClientEnvelope {
        version: PROTOCOL_VERSION,
        request_id: 9,
        proof: Some(proof),
        request: ClientRequest::DeviceOperation {
            operation: Operation::AccountImport(operation),
        },
    }
}
fn server(reply: ImportReply, proof: crate::model::HostSessionProof) -> ServerEnvelope {
    ServerEnvelope {
        version: PROTOCOL_VERSION,
        request_id: Some(9),
        proof,
        message: ServerMessage::Response {
            response: ServerResponse::DeviceOperation {
                reply: Reply::AccountImport(Box::new(reply)),
            },
        },
    }
}

#[test]
fn profile_json_and_toml_keep_the_tagged_schema_and_refuse_unknown_or_duplicate_fields() {
    for (profile, json, toml) in [
        (
            CodingProfile::Unconfigured,
            serde_json::json!({"kind":"unconfigured"}),
            "kind = \"unconfigured\"\n",
        ),
        (
            CodingProfile::WorkspaceWrite {
                network_access: false,
            },
            serde_json::json!({"kind":"workspace_write","network_access":false}),
            "kind = \"workspace_write\"\nnetwork_access = false\n",
        ),
        (
            CodingProfile::WorkspaceWrite {
                network_access: true,
            },
            serde_json::json!({"kind":"workspace_write","network_access":true}),
            "kind = \"workspace_write\"\nnetwork_access = true\n",
        ),
    ] {
        assert_eq!(serde_json::to_value(profile).unwrap(), json);
        assert_eq!(
            serde_json::from_value::<CodingProfile>(json.clone()).unwrap(),
            profile
        );
        assert_eq!(toml::to_string(&profile).unwrap(), toml);
        assert_eq!(toml::from_str::<CodingProfile>(toml).unwrap(), profile);
        let mut unknown = json;
        unknown["unexpected"] = true.into();
        assert!(serde_json::from_value::<CodingProfile>(unknown).is_err());
        assert!(toml::from_str::<CodingProfile>(&format!("{toml}unexpected = true\n")).is_err());
    }
    for json in [
        r#"{"kind":"workspace_write","network_access":false,"network_access":true}"#,
        r#"{"kind":"workspace_write"}"#,
        r#"{"kind":"unconfigured","network_access":true}"#,
        r#"{"WorkspaceWrite":{"network_access":true}}"#,
    ] {
        assert!(serde_json::from_str::<CodingProfile>(json).is_err());
    }
}

#[test]
fn actual_postcard_import_envelopes_roundtrip_every_profile_and_original_vs_current_proof() {
    let current = proof("current-daemon");
    let mut statuses = Vec::new();
    for (index, profile) in [
        CodingProfile::Unconfigured,
        CodingProfile::WorkspaceWrite {
            network_access: false,
        },
        CodingProfile::WorkspaceWrite {
            network_access: true,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let mut original = plan(profile);
        original.operation = Uuid::from_u128(index as u128 + 1);
        assert!(original.validate().is_ok());
        roundtrip(&client(
            ImportOperation::Prepare {
                plan: original.clone(),
            },
            original.destination.clone(),
        ));
        roundtrip(&client(
            ImportOperation::Verify {
                operation: original.operation,
                destination: current.clone(),
            },
            current.clone(),
        ));
        let status = ImportStatus {
            observed_destination: current.clone(),
            plan: original.clone(),
            receipt: Some(PublicationReceipt {
                operation: original.operation,
                home: PhysicalIdentity {
                    device: 2,
                    inode: 10,
                },
                file: PhysicalIdentity {
                    device: 2,
                    inode: 11,
                },
                size: original.observation.size(),
            }),
            stage: ImportStage::Published,
            uncertain: false,
            publication: ImportPublication::Present,
            registered: None,
            coding_profile: None,
        };
        let decoded = roundtrip(&server(
            ImportReply::Status(status.clone()),
            current.clone(),
        ));
        let ServerMessage::Response {
            response:
                ServerResponse::DeviceOperation {
                    reply: Reply::AccountImport(decoded),
                },
        } = decoded.message
        else {
            panic!("different reply")
        };
        let ImportReply::Status(decoded) = *decoded else {
            panic!("different import reply")
        };
        assert_eq!(decoded.plan, original);
        assert_eq!(decoded.observed_destination, current);
        assert_ne!(decoded.observed_destination, decoded.plan.destination);
        roundtrip(&server(
            ImportReply::Listed(vec![status.clone()]),
            current.clone(),
        ));
        statuses.push(status);
    }
    roundtrip(&client(ImportOperation::List, current.clone()));
    roundtrip(&server(ImportReply::Listed(statuses), current));
}
