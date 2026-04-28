//! Opt-in S3 round-trip test.
//!
//! All tests here are `#[ignore]`d by default — they require live AWS
//! credentials and an existing bucket. Invoke with:
//!
//! ```text
//! AWS_REGION=us-east-1 \
//! AWS_ACCESS_KEY_ID=... \
//! AWS_SECRET_ACCESS_KEY=... \
//! GRUMPYDB_TEST_S3_BUCKET=my-bucket \
//! cargo test -p grumpydb-server --features cloud-aws --test snapshot_aws -- --ignored
//! ```
//!
//! If `GRUMPYDB_TEST_S3_BUCKET` is not set the test logs a skip message and
//! returns successfully (so users who manually `--ignored` the whole suite
//! don't get spurious failures).

#![cfg(feature = "cloud-aws")]

use grumpydb_server::snapshot::{self, Location, SnapshotOptions};

#[tokio::test]
#[ignore]
async fn test_s3_round_trip() {
    let bucket = match std::env::var("GRUMPYDB_TEST_S3_BUCKET") {
        Ok(b) => b,
        Err(_) => {
            eprintln!("skip: GRUMPYDB_TEST_S3_BUCKET not set");
            return;
        }
    };
    let key = format!(
        "grumpydb-test/snap-{}.tar.gz",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    );

    // Build a tiny data dir.
    let src = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(src.path().join("d")).unwrap();
    std::fs::write(src.path().join("d/a"), b"hello").unwrap();

    let dest = Location::parse(&format!("s3://{bucket}/{key}")).unwrap();
    snapshot::snapshot(
        &SnapshotOptions {
            data_dir: src.path().to_path_buf(),
            force: false,
        },
        &dest,
    )
    .await
    .expect("snapshot to S3");

    let restored = tempfile::TempDir::new().unwrap();
    snapshot::restore(
        &SnapshotOptions {
            data_dir: restored.path().to_path_buf(),
            force: false,
        },
        &dest,
    )
    .await
    .expect("restore from S3");

    let got = std::fs::read(restored.path().join("d/a")).unwrap();
    assert_eq!(got, b"hello");
}
