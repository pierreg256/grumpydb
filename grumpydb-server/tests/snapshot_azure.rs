//! Opt-in Azure Blob round-trip test.
//!
//! All tests here are `#[ignore]`d by default — they require live Azure
//! credentials and an existing container. Invoke with:
//!
//! ```text
//! AZURE_STORAGE_CONNECTION_STRING="DefaultEndpointsProtocol=https;AccountName=...;AccountKey=..." \
//! GRUMPYDB_TEST_AZURE_CONTAINER=my-container \
//! cargo test -p grumpydb-server --features cloud-azure --test snapshot_azure -- --ignored
//! ```
//!
//! If `GRUMPYDB_TEST_AZURE_CONTAINER` is not set the test logs a skip
//! message and returns successfully.

#![cfg(feature = "cloud-azure")]

use grumpydb_server::snapshot::{self, Location, SnapshotOptions};

#[tokio::test]
#[ignore]
async fn test_azure_round_trip() {
    let container = match std::env::var("GRUMPYDB_TEST_AZURE_CONTAINER") {
        Ok(c) => c,
        Err(_) => {
            eprintln!("skip: GRUMPYDB_TEST_AZURE_CONTAINER not set");
            return;
        }
    };
    let blob = format!(
        "grumpydb-test/snap-{}.tar.gz",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    );

    let src = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(src.path().join("d")).unwrap();
    std::fs::write(src.path().join("d/a"), b"hello").unwrap();

    let dest = Location::parse(&format!("az://{container}/{blob}")).unwrap();
    snapshot::snapshot(
        &SnapshotOptions {
            data_dir: src.path().to_path_buf(),
            force: false,
        },
        &dest,
    )
    .await
    .expect("snapshot to Azure");

    let restored = tempfile::TempDir::new().unwrap();
    snapshot::restore(
        &SnapshotOptions {
            data_dir: restored.path().to_path_buf(),
            force: false,
        },
        &dest,
    )
    .await
    .expect("restore from Azure");

    let got = std::fs::read(restored.path().join("d/a")).unwrap();
    assert_eq!(got, b"hello");
}
