//! Concurrent end-to-end stress: many parallel clients hitting one server.

use grumpydb_client::GrumpyClient;
use grumpydb_testing::TestServer;
use serde_json::json;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_e2e_concurrent_clients() {
    let server = TestServer::spawn().await;

    // Bootstrap: create the shared database once (idempotent USE on each
    // worker would race the create-or-open path).
    {
        let mut client = GrumpyClient::connect("127.0.0.1", server.addr.port(), false)
            .await
            .expect("admin connect");
        client
            .login(
                server.admin_tenant,
                server.admin_user,
                &server.admin_password,
            )
            .await
            .expect("admin login");
        client
            .create_database("concurrency_db")
            .await
            .expect("create db");

        // Pre-create per-worker collections to avoid concurrent CREATE COLLECTION races.
        let mut db = client.database("concurrency_db").await.expect("use");
        for w in 0..50_u32 {
            db.create_collection(&format!("c{w}"))
                .await
                .expect("create coll");
        }
    }

    let port = server.addr.port();
    let tenant = server.admin_tenant.to_string();
    let user = server.admin_user.to_string();
    let password = server.admin_password.clone();

    let mut handles = Vec::new();
    for worker in 0..50_u32 {
        let tenant = tenant.clone();
        let user = user.clone();
        let password = password.clone();
        handles.push(tokio::spawn(async move {
            let mut client = GrumpyClient::connect("127.0.0.1", port, false)
                .await
                .expect("client connect");
            client
                .login(&tenant, &user, &password)
                .await
                .expect("client login");
            let mut db = client.database("concurrency_db").await.expect("client use");
            let coll = format!("c{worker}");

            let mut keys = Vec::with_capacity(100);
            for i in 0..100_i64 {
                let key = Uuid::new_v4();
                db.insert(&coll, key, &json!({ "w": worker, "i": i }))
                    .await
                    .expect("insert");
                keys.push(key);
            }
            for (i, key) in keys.iter().enumerate() {
                let v = db.get(&coll, key).await.expect("get");
                let got = v.expect("doc must exist");
                assert_eq!(got.get("i"), Some(&json!(i as i64)));
            }

            let count = db.count(&coll).await.expect("count");
            assert_eq!(count, 100, "worker {worker} count mismatch");
        }));
    }

    for h in handles {
        h.await.expect("worker task");
    }
}
