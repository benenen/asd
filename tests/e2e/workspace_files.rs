//! Real daemon directory listing without PTY attachment or input.
use super::*;

async fn list(client: &mut ProtoClient, identity: asd_proto::SessionIdentity, path: &str) -> Frame {
    client
        .send(Frame::ListWorkspaceFiles {
            identity,
            path: path.into(),
        })
        .await;
    client.recv().await
}

#[tokio::test]
async fn workspace_files_use_bound_directory_and_keep_identity_across_rename() {
    let daemon = Daemon::start("workspace-files");
    let workspace = daemon.dir.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(workspace.join("nested")).unwrap();
    std::fs::write(workspace.join("nested/inside.txt"), "inside").unwrap();
    std::fs::write(workspace.join(".hidden"), "hidden").unwrap();
    std::fs::write(workspace.join("hello.txt"), "hello").unwrap();
    let created = daemon
        .cli()
        .args(["new", "files", "--cwd", daemon.dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(created.status.success(), "{created:?}");
    let mut client = ProtoClient::connect(&daemon.socket).await;
    client.send(Frame::ListSessions).await;
    let Frame::SessionList { sessions } = client.recv().await else {
        panic!("sessions expected")
    };
    let identity = sessions[0].identity();
    client
        .send(Frame::SetSessionTask {
            identity,
            task: Some(asd_proto::SessionTask {
                description: "Browse workspace".into(),
                directory: workspace.to_str().unwrap().into(),
            }),
        })
        .await;
    assert!(matches!(client.recv().await, Frame::Ack));
    let Frame::WorkspaceFiles {
        identity: got,
        root,
        path,
        entries,
        truncated,
    } = list(&mut client, identity, "").await
    else {
        panic!("directory listing expected")
    };
    assert_eq!(got, identity);
    assert_eq!(PathBuf::from(root), workspace.canonicalize().unwrap());
    assert!(path.is_empty());
    assert!(!truncated);
    assert_eq!(entries[0].name, "nested");
    assert_eq!(entries[0].kind, asd_proto::WorkspaceEntryKind::Directory);
    assert!(entries.iter().any(|e| e.name == ".hidden"));
    let rename = daemon
        .cli()
        .args(["rename", "files", "renamed"])
        .output()
        .unwrap();
    assert!(rename.status.success(), "{rename:?}");
    let Frame::WorkspaceFiles { entries, path, .. } = list(&mut client, identity, "nested").await
    else {
        panic!("nested listing expected")
    };
    assert_eq!(path, "nested");
    assert_eq!(entries[0].name, "inside.txt");
    assert!(matches!(
        list(&mut client, identity, "../").await,
        Frame::Error { .. }
    ));
    assert!(matches!(
        list(&mut client, identity, "hello.txt").await,
        Frame::Error { .. }
    ));
    client.send(Frame::ListSessions).await;
    let Frame::SessionList { sessions } = client.recv().await else {
        panic!("sessions expected")
    };
    assert_eq!(sessions[0].attached_clients, 0);
    assert_eq!((sessions[0].cols, sessions[0].rows), (80, 24));
    assert!(matches!(
        list(
            &mut client,
            asd_proto::SessionIdentity { instance_id: 0 },
            ""
        )
        .await,
        Frame::Error { .. }
    ));
}
